//! Linux editor actions built on the shared, undoable document editor.

use jiffshot_doc::editor::{Editor, Tool};
use jiffshot_doc::object::{BlurMode, Kind, Object, Style};
use jiffshot_doc::smart::{self, TextLine};
use jiffshot_doc::{effects, raster, redact, Document, Rgba};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub(super) struct DocumentLoadRequests {
    latest: AtomicU64,
}

pub(super) struct PendingDocumentLoad {
    sequence: u64,
    snapshot: Document,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum DocumentLoadState {
    Ready,
    Superseded,
    DocumentChanged,
}

impl DocumentLoadRequests {
    pub(super) const fn new() -> Self {
        Self {
            latest: AtomicU64::new(0),
        }
    }

    pub(super) fn begin(&self, snapshot: Document) -> PendingDocumentLoad {
        PendingDocumentLoad {
            sequence: self.latest.fetch_add(1, Ordering::AcqRel).wrapping_add(1),
            snapshot,
        }
    }

    pub(super) fn state(
        &self,
        pending: &PendingDocumentLoad,
        current: &Document,
    ) -> DocumentLoadState {
        if self.latest.load(Ordering::Acquire) != pending.sequence {
            DocumentLoadState::Superseded
        } else if !same_document(&pending.snapshot, current) {
            DocumentLoadState::DocumentChanged
        } else {
            DocumentLoadState::Ready
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SelectionAction {
    SelectAll,
    Deselect,
    Invert,
    Delete,
    Duplicate,
    Group,
    Ungroup,
    BringForward,
    SendBackward,
    FlattenSelected,
    FlattenAll,
}

pub(super) fn selection_action(
    editor: &mut Editor,
    action: SelectionAction,
) -> Result<&'static str, String> {
    match action {
        SelectionAction::SelectAll => {
            if editor.document().objects.is_empty() {
                return Err("There are no annotations to select.".to_owned());
            }
            editor.select_all();
            Ok("All annotations selected.")
        }
        SelectionAction::Deselect => {
            if editor.selected_ids().is_empty() {
                return Err("No annotations are selected.".to_owned());
            }
            editor.set_selection(Vec::new());
            Ok("Annotations deselected.")
        }
        SelectionAction::Invert => {
            if editor.document().objects.is_empty() {
                return Err("There are no annotations to select.".to_owned());
            }
            editor.invert_selection();
            Ok("Selection inverted.")
        }
        SelectionAction::Delete => editor
            .delete_selected()
            .then_some("Selected annotations deleted.")
            .ok_or_else(|| "Select annotations to delete.".to_owned()),
        SelectionAction::Duplicate => editor
            .duplicate_selected()
            .map(|_| "Selected annotations duplicated.")
            .ok_or_else(|| "Select annotations to duplicate.".to_owned()),
        SelectionAction::Group => editor
            .group_selected()
            .map(|_| "Selected annotations grouped.")
            .ok_or_else(|| "Select at least two annotations to group.".to_owned()),
        SelectionAction::Ungroup => editor
            .ungroup_selected()
            .then_some("Selected groups ungrouped.")
            .ok_or_else(|| "Select a group to ungroup.".to_owned()),
        SelectionAction::BringForward | SelectionAction::SendBackward => {
            let Some(selected) = editor.selection() else {
                return Err("Select an annotation to change its order.".to_owned());
            };
            let before = editor
                .document()
                .objects
                .iter()
                .position(|object| object.id == selected);
            let (step, success) = if action == SelectionAction::BringForward {
                (1, "Annotation moved forward.")
            } else {
                (-1, "Annotation moved backward.")
            };
            editor.arrange(step);
            let after = editor
                .document()
                .objects
                .iter()
                .position(|object| object.id == selected);
            if before == after {
                Err("The annotation is already at the edge of the stack.".to_owned())
            } else {
                Ok(success)
            }
        }
        SelectionAction::FlattenSelected => editor
            .flatten_selected()
            .then_some("Selected annotations flattened.")
            .ok_or_else(|| "Select annotations to flatten.".to_owned()),
        SelectionAction::FlattenAll => {
            if editor.document().objects.is_empty() {
                return Err("There are no annotations to flatten.".to_owned());
            }
            editor.select_all();
            if editor.flatten_selected() {
                Ok("All annotations flattened.")
            } else {
                Err("Could not flatten annotations.".to_owned())
            }
        }
    }
}

#[derive(Debug)]
pub(super) struct StyleInputs {
    pub(super) stroke: String,
    pub(super) fill: String,
    pub(super) text_color: String,
    pub(super) width: String,
    pub(super) font: String,
    pub(super) font_size: String,
    pub(super) opacity: String,
    pub(super) bold: bool,
    pub(super) italic: bool,
    pub(super) shadow: bool,
}

pub(super) fn apply_style(editor: &mut Editor, inputs: &StyleInputs) -> Result<(), String> {
    let stroke = parse_color(&inputs.stroke, "stroke")?;
    let fill = parse_color(&inputs.fill, "fill")?;
    let text_color = parse_color(&inputs.text_color, "text")?;
    let width = parse_number(&inputs.width, "stroke width", 0.0, 200.0)?;
    let font_size = parse_number(&inputs.font_size, "font size", 4.0, 400.0)?;
    let opacity = parse_number(&inputs.opacity, "opacity", 5.0, 100.0)? / 100.0;
    let font = inputs.font.trim();
    if font.is_empty() {
        return Err("font family cannot be empty".to_owned());
    }

    let resize_step = editor.current_style().font_size != font_size;
    editor.seal();
    editor.update_style("size", |style| {
        style.stroke = stroke;
        style.fill = fill;
        style.text_color = text_color;
        style.width = width;
        style.font = font.to_owned();
        style.font_size = font_size;
        style.opacity = opacity;
        style.bold = inputs.bold;
        style.italic = inputs.italic;
        style.shadow = inputs.shadow;
    });
    if resize_step {
        editor.resize_selected_step(font_size);
    }
    editor.seal();
    Ok(())
}

fn parse_color(value: &str, name: &str) -> Result<Rgba, String> {
    Rgba::parse(value).ok_or_else(|| format!("{name} color must be #RGB, #RRGGBB, or #AARRGGBB"))
}

fn parse_number(value: &str, name: &str, min: f32, max: f32) -> Result<f32, String> {
    let value = value
        .trim()
        .parse::<f32>()
        .map_err(|_| format!("{name} must be a number from {min} to {max}"))?;
    if !value.is_finite() || !(min..=max).contains(&value) {
        return Err(format!("{name} must be a number from {min} to {max}"));
    }
    Ok(value)
}

pub(super) fn apply_text(editor: &mut Editor, text: &str) -> Result<(), String> {
    if editor.selected().and_then(|object| object.text()).is_none() {
        return Err("select a text, callout, or step annotation first".to_owned());
    }
    editor.seal();
    editor.set_selected_text(text);
    editor.seal();
    Ok(())
}

#[derive(Clone, Copy)]
enum EffectParameterKind {
    Whole { min: u32, max: u32 },
    Decimal { min: f32, max: f32 },
}

#[derive(Clone, Copy)]
struct EffectParameterSpec {
    hint: &'static str,
    kind: EffectParameterKind,
}

#[derive(Debug, PartialEq)]
enum EffectParameter {
    Whole(u32),
    Decimal(f32),
}

fn effect_parameter_spec(name: &str) -> Option<EffectParameterSpec> {
    use EffectParameterKind::{Decimal, Whole};
    let (hint, kind) = match name {
        "Border" => ("Border width: 1–256 px", Whole { min: 1, max: 256 }),
        "Shadow" => ("Shadow size: 1–128 px", Whole { min: 1, max: 128 }),
        "Torn edge" | "Torn bottom" | "Sawtooth edge" | "Wave edge" => {
            ("Edge depth: 2–256 px", Whole { min: 2, max: 256 })
        }
        "Rounded corners" => ("Corner radius: 1–512 px", Whole { min: 1, max: 512 }),
        "Fade edges" => ("Fade size: 1–512 px", Whole { min: 1, max: 512 }),
        "Blur image" => ("Blur radius: 1–64 px", Whole { min: 1, max: 64 }),
        "Scale to 1920" => ("Maximum width: 1–8192 px", Whole { min: 1, max: 8192 }),
        "Perspective" | "Tilt" => (
            "Amount: 0–95%",
            Decimal {
                min: 0.0,
                max: 95.0,
            },
        ),
        "Brighten" | "Darken" => (
            "Brightness: -100–100%",
            Decimal {
                min: -100.0,
                max: 100.0,
            },
        ),
        "More contrast" | "Less contrast" => (
            "Contrast: -100–100%",
            Decimal {
                min: -100.0,
                max: 100.0,
            },
        ),
        "Saturate" | "Desaturate" => (
            "Saturation: 0–400%",
            Decimal {
                min: 0.0,
                max: 400.0,
            },
        ),
        "Hue shift" => (
            "Hue shift: -360–360 degrees",
            Decimal {
                min: -360.0,
                max: 360.0,
            },
        ),
        "Gamma up" | "Gamma down" => ("Gamma: 0.1–5", Decimal { min: 0.1, max: 5.0 }),
        "Watermark" => (
            "Watermark opacity: 0–100%",
            Decimal {
                min: 0.0,
                max: 100.0,
            },
        ),
        "Spotlight & magnify" => ("Magnification: 1–8 times", Decimal { min: 1.0, max: 8.0 }),
        _ => return None,
    };
    Some(EffectParameterSpec { hint, kind })
}

pub(super) fn effect_parameter_hint(name: &str) -> &'static str {
    effect_parameter_spec(name)
        .map(|spec| spec.hint)
        .unwrap_or("This effect has no numeric parameter")
}

fn parse_effect_parameter(name: &str, value: &str) -> Result<Option<EffectParameter>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let spec = effect_parameter_spec(name)
        .ok_or_else(|| format!("{name} does not use a numeric parameter"))?;
    let parameter = match spec.kind {
        EffectParameterKind::Whole { min, max } => {
            let parsed = value
                .parse::<u32>()
                .map_err(|_| format!("{}; enter a whole number", spec.hint))?;
            if !(min..=max).contains(&parsed) {
                return Err(format!("{}; enter a whole number", spec.hint));
            }
            EffectParameter::Whole(parsed)
        }
        EffectParameterKind::Decimal { min, max } => {
            EffectParameter::Decimal(parse_number(value, spec.hint, min, max)?)
        }
    };
    Ok(Some(parameter))
}

pub(super) fn validate_effect_parameter(name: &str, value: &str) -> Result<(), String> {
    parse_effect_parameter(name, value).map(|_| ())
}

pub(super) fn watermark_opacity(value: &str) -> Result<f32, String> {
    match parse_effect_parameter("Watermark", value)? {
        None => Ok(0.35),
        Some(EffectParameter::Decimal(opacity)) => Ok(opacity / 100.0),
        Some(EffectParameter::Whole(_)) => Err("Invalid watermark opacity.".to_owned()),
    }
}

fn parameterized_effect(
    name: &str,
    value: &str,
    stroke: Rgba,
    fill: Rgba,
) -> Result<effects::Effect, String> {
    let mut effect = effects::by_name_for(name, stroke, fill)
        .ok_or_else(|| format!("unknown image effect: {name}"))?;
    let Some(parameter) = parse_effect_parameter(name, value)? else {
        return Ok(effect);
    };
    match (&mut effect, parameter) {
        (effects::Effect::Border { width, .. }, EffectParameter::Whole(value)) => *width = value,
        (effects::Effect::Shadow { size }, EffectParameter::Whole(value)) => *size = value,
        (effects::Effect::TornEdge { depth }, EffectParameter::Whole(value))
        | (effects::Effect::Edges { depth, .. }, EffectParameter::Whole(value)) => *depth = value,
        (effects::Effect::RoundedCorners { radius }, EffectParameter::Whole(value)) => {
            *radius = value
        }
        (effects::Effect::FadeEdges { size }, EffectParameter::Whole(value)) => *size = value,
        (effects::Effect::BlurImage(radius), EffectParameter::Whole(value)) => *radius = value,
        (effects::Effect::ScaleTo(width), EffectParameter::Whole(value)) => *width = value,
        (effects::Effect::Perspective { amount, .. }, EffectParameter::Decimal(value)) => {
            *amount = value / 100.0
        }
        (
            effects::Effect::Adjust {
                brightness,
                contrast,
            },
            EffectParameter::Decimal(value),
        ) => {
            if matches!(name, "Brighten" | "Darken") {
                *brightness = value / 100.0;
            } else {
                *contrast = value / 100.0;
            }
        }
        (effects::Effect::Saturation(saturation), EffectParameter::Decimal(value)) => {
            *saturation = value / 100.0
        }
        (effects::Effect::HueShift(degrees), EffectParameter::Decimal(value)) => *degrees = value,
        (effects::Effect::Gamma(gamma), EffectParameter::Decimal(value)) => *gamma = value,
        _ => return Err(format!("{name} does not support this numeric parameter")),
    }
    Ok(effect)
}

pub(super) fn apply_effect(editor: &mut Editor, name: &str, value: &str) -> Result<(), String> {
    if name == "Spotlight & magnify" {
        let zoom = match parse_effect_parameter(name, value)? {
            None => 1.6,
            Some(EffectParameter::Decimal(zoom)) => zoom,
            Some(EffectParameter::Whole(_)) => {
                return Err("Invalid spotlight magnification.".to_owned())
            }
        };
        let (width, height) = editor.document().size();
        let area_is_valid = match editor.selected() {
            Some(object) => {
                let area = object.rect().unwrap_or_else(|| object.bounds());
                [area.x0, area.y0, area.x1, area.y1]
                    .into_iter()
                    .all(f32::is_finite)
                    && area.x1.min(width as f32) - area.x0.max(0.0) >= 4.0
                    && area.y1.min(height as f32) - area.y0.max(0.0) >= 4.0
            }
            None => width >= 12 && height >= 12,
        };
        if !area_is_valid {
            return Err("Select an area within the image large enough to magnify.".to_owned());
        }
        let before = editor.document().objects.len();
        editor.spotlight_magnify(zoom);
        let after = editor.document().objects.len();
        if after > before {
            return Ok(());
        }
        if after < before && !editor.undo() {
            return Err("Could not restore the selection after magnifying.".to_owned());
        }
        return Err("Select an area within the image large enough to magnify.".to_owned());
    }
    let style = editor.current_style();
    let effect = parameterized_effect(name, value, style.stroke, style.fill)?;
    apply_document_effect(editor, name, effect)
}

pub(super) fn apply_document_effect(
    editor: &mut Editor,
    name: &str,
    effect: effects::Effect,
) -> Result<(), String> {
    if matches!(effect, effects::Effect::CaptureInfo { .. }) && editor.document().width() <= 24 {
        return Err(
            "The image must be at least 25 pixels wide for capture information.".to_owned(),
        );
    }
    if !editor.edit_document(name, |document| effects::apply(document, effect)) {
        return Err(format!("could not apply image effect: {name}"));
    }
    Ok(())
}

pub(super) fn capture_info(source: Option<&Path>) -> Result<String, String> {
    let metadata = source
        .map(super::library::read_metadata)
        .transpose()?
        .unwrap_or_default();
    let mut parts = [
        metadata
            .captured_at
            .as_deref()
            .map(|value| value.replace('T', " ")),
        metadata.window_title,
        metadata.process,
    ]
    .into_iter()
    .flatten()
    .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
    .filter(|value| !value.is_empty())
    .collect::<Vec<_>>();
    if parts.is_empty() {
        let now = gstreamer::glib::DateTime::now_local()
            .map_err(|error| format!("could not read local time: {error}"))?;
        parts.push(
            now.format("%Y-%m-%d %H:%M:%S")
                .map_err(|error| format!("could not format local time: {error}"))?
                .to_string(),
        );
    }
    parts.push("Linux".to_owned());
    let text = parts.join("  ·  ");
    if text.chars().count() > 1024 {
        return Err("Capture information is too long to add to the image.".to_owned());
    }
    Ok(text)
}

pub(super) fn auto_trim(editor: &mut Editor) -> Result<(), String> {
    let bounds = raster::trim_bounds(editor.document().background())
        .ok_or_else(|| "No solid-colored edges to trim.".to_owned())?;
    if editor.edit_document("Auto trim", |document| document.crop(bounds)) {
        Ok(())
    } else {
        Err("Could not trim the image.".to_owned())
    }
}

pub(super) fn same_document(snapshot: &Document, current: &Document) -> bool {
    fn same_images(left: &Object, right: &Object) -> bool {
        match (&left.kind, &right.kind) {
            (Kind::Image { image: left, .. }, Kind::Image { image: right, .. }) => {
                match (&left.pixmap, &right.pixmap) {
                    (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                    (None, None) => true,
                    _ => false,
                }
            }
            (Kind::Group { objects: left }, Kind::Group { objects: right }) => left
                .iter()
                .zip(right)
                .all(|(left, right)| same_images(left, right)),
            _ => true,
        }
    }

    snapshot.size() == current.size()
        && Arc::ptr_eq(&snapshot.background_arc(), &current.background_arc())
        && snapshot.objects == current.objects
        && snapshot
            .objects
            .iter()
            .zip(&current.objects)
            .all(|(left, right)| same_images(left, right))
}

pub(super) fn smart_redact(editor: &mut Editor, lines: &[TextLine]) -> usize {
    let areas: Vec<_> = lines
        .iter()
        .filter(|line| !redact::find(&line.text).is_empty())
        .map(|line| line.area)
        .collect();
    if areas.is_empty() {
        return 0;
    }
    let style = Style {
        fill: editor.tool_style(Tool::Blur).fill.with_alpha(255),
        width: 0.0,
        ..Style::default()
    };
    let mut ids = Vec::with_capacity(areas.len());
    editor.edit_document("Smart Redact", |document| {
        for area in &areas {
            ids.push(document.add(
                Kind::Blur {
                    rect: area.to_box().inflate(3.0),
                    mode: BlurMode::Redact,
                    amount: 10.0,
                },
                style.clone(),
            ));
        }
        true
    });
    editor.set_selection(ids);
    areas.len()
}

pub(super) fn simplify_document(document: &Document, lines: &[TextLine]) -> (Document, usize) {
    let mut simplified = document.clone();
    simplified.flatten_objects();
    let bars = smart::simplify(&mut simplified, lines, 12);
    (simplified, bars)
}

pub(super) fn apply_simplification(editor: &mut Editor, mut simplified: Document) {
    let background = simplified.background_arc();
    let objects = std::mem::take(&mut simplified.objects);
    drop(simplified);
    let background = Arc::try_unwrap(background).unwrap_or_else(|image| (*image).clone());
    editor.edit_document("Simplify", move |document| {
        document.set_background(background);
        document.objects.clear();
        for object in objects {
            document.insert(object);
        }
        true
    });
}

pub(super) fn recognized_text(lines: &[TextLine]) -> String {
    lines
        .iter()
        .map(|line| line.text.trim())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiffshot_doc::object::{ImageRef, Kind, ShapeKind, Style};
    use jiffshot_doc::{Box2, Document, IRect, Pt};

    fn annotation_editor() -> Editor {
        let mut editor = Editor::new(Document::blank(80, 60, Rgba::WHITE));
        for x in [5.0, 30.0] {
            editor.add_object(
                "Shape",
                Kind::Shape {
                    rect: Box2::from_xywh(x, 5.0, 20.0, 20.0),
                    shape: ShapeKind::Rectangle,
                },
                Style::default(),
            );
        }
        editor
    }

    #[test]
    fn pending_document_load_is_rejected_after_an_edit() {
        let requests = DocumentLoadRequests::new();
        let mut editor = annotation_editor();
        let pending = requests.begin(editor.document().clone());
        assert_eq!(
            requests.state(&pending, editor.document()),
            DocumentLoadState::Ready
        );

        editor.add_object(
            "Shape",
            Kind::Shape {
                rect: Box2::from_xywh(50.0, 5.0, 15.0, 15.0),
                shape: ShapeKind::Rectangle,
            },
            Style::default(),
        );
        assert_eq!(
            requests.state(&pending, editor.document()),
            DocumentLoadState::DocumentChanged
        );
    }

    #[test]
    fn newer_document_load_supersedes_an_older_request() {
        let requests = DocumentLoadRequests::new();
        let editor = annotation_editor();
        let older = requests.begin(editor.document().clone());
        let newer = requests.begin(editor.document().clone());
        assert_eq!(
            requests.state(&older, editor.document()),
            DocumentLoadState::Superseded
        );
        assert_eq!(
            requests.state(&newer, editor.document()),
            DocumentLoadState::Ready
        );
    }

    #[test]
    fn selection_changes_do_not_cancel_a_pending_document_load() {
        let requests = DocumentLoadRequests::new();
        let mut editor = annotation_editor();
        let pending = requests.begin(editor.document().clone());
        editor.select_all();
        assert_eq!(
            requests.state(&pending, editor.document()),
            DocumentLoadState::Ready
        );
    }

    #[test]
    fn selection_actions_group_ungroup_and_delete_are_undoable() {
        let mut editor = annotation_editor();
        selection_action(&mut editor, SelectionAction::SelectAll).unwrap();
        assert_eq!(editor.selected_ids().len(), 2);
        selection_action(&mut editor, SelectionAction::Group).unwrap();
        assert_eq!(editor.document().objects.len(), 1);
        assert!(matches!(
            editor.selected().unwrap().kind,
            Kind::Group { .. }
        ));
        selection_action(&mut editor, SelectionAction::Ungroup).unwrap();
        assert_eq!(editor.document().objects.len(), 2);
        assert_eq!(editor.selected_ids().len(), 2);
        selection_action(&mut editor, SelectionAction::Delete).unwrap();
        assert!(editor.document().objects.is_empty());
        assert!(editor.undo(), "deletion should be undoable");
        assert_eq!(editor.document().objects.len(), 2);
    }

    #[test]
    fn selection_actions_invert_duplicate_arrange_and_flatten() {
        let mut editor = annotation_editor();
        let initial = editor.selected_ids();
        selection_action(&mut editor, SelectionAction::Invert).unwrap();
        assert_ne!(editor.selected_ids(), initial);
        selection_action(&mut editor, SelectionAction::Duplicate).unwrap();
        assert_eq!(editor.document().objects.len(), 3);
        let duplicate = editor.selection().unwrap();
        selection_action(&mut editor, SelectionAction::SendBackward).unwrap();
        assert_eq!(editor.document().objects[1].id, duplicate);
        selection_action(&mut editor, SelectionAction::BringForward).unwrap();
        assert_eq!(editor.document().objects[2].id, duplicate);
        selection_action(&mut editor, SelectionAction::FlattenSelected).unwrap();
        assert_eq!(editor.document().objects.len(), 2);
        assert!(editor.undo(), "flattening should be undoable");
        assert_eq!(editor.document().objects.len(), 3);
        selection_action(&mut editor, SelectionAction::FlattenAll).unwrap();
        assert!(editor.document().objects.is_empty());
        assert!(editor.undo(), "flattening all should be undoable");
        assert_eq!(editor.document().objects.len(), 3);
        selection_action(&mut editor, SelectionAction::SelectAll).unwrap();
        selection_action(&mut editor, SelectionAction::Deselect).unwrap();
        assert!(editor.selected_ids().is_empty());
    }

    #[test]
    fn selection_actions_report_unavailable_operations() {
        let mut editor = Editor::new(Document::blank(8, 8, Rgba::WHITE));
        for action in [
            SelectionAction::SelectAll,
            SelectionAction::Deselect,
            SelectionAction::Invert,
            SelectionAction::Delete,
            SelectionAction::Duplicate,
            SelectionAction::Group,
            SelectionAction::Ungroup,
            SelectionAction::BringForward,
            SelectionAction::SendBackward,
            SelectionAction::FlattenSelected,
            SelectionAction::FlattenAll,
        ] {
            assert!(
                selection_action(&mut editor, action).is_err(),
                "empty editor should reject {action:?}"
            );
        }
    }

    fn inputs() -> StyleInputs {
        StyleInputs {
            stroke: "#123456".to_owned(),
            fill: "#8000FF00".to_owned(),
            text_color: "#ffffff".to_owned(),
            width: "9".to_owned(),
            font: "Sans".to_owned(),
            font_size: "36".to_owned(),
            opacity: "75".to_owned(),
            bold: true,
            italic: true,
            shadow: true,
        }
    }

    fn text_editor() -> Editor {
        let mut editor = Editor::new(Document::blank(80, 60, Rgba::WHITE));
        editor.add_object(
            "Text",
            Kind::Text {
                rect: Box2::from_xywh(2.0, 2.0, 40.0, 30.0),
                text: "Before".to_owned(),
            },
            Style::default(),
        );
        editor
    }

    #[test]
    fn annotation_text_changes_are_editable_and_undoable() {
        let mut editor = text_editor();
        apply_text(&mut editor, "After").expect("selected text should update");
        assert_eq!(
            editor.selected().and_then(|object| object.text()),
            Some("After")
        );
        assert!(editor.undo(), "text changes should be undoable");
        assert_eq!(
            editor.selected().and_then(|object| object.text()),
            Some("Before")
        );
        editor.select(None);
        assert!(
            apply_text(&mut editor, "unused").is_err(),
            "editing text without a text-bearing selection should fail"
        );
    }

    #[test]
    fn style_fields_update_the_selection_and_undo_together() {
        let mut editor = text_editor();
        let before = editor.current_style();
        apply_style(&mut editor, &inputs()).expect("valid properties should apply");
        let style = editor.current_style();
        assert_eq!(style.stroke, Rgba::rgb(0x12, 0x34, 0x56));
        assert_eq!(style.fill, Rgba::new(0, 255, 0, 128));
        assert_eq!(style.text_color, Rgba::WHITE);
        assert_eq!(style.width, 9.0);
        assert_eq!(style.font, "Sans");
        assert_eq!(style.font_size, 36.0);
        assert_eq!(style.opacity, 0.75);
        assert!(style.bold && style.italic && style.shadow);
        assert!(editor.undo(), "the style change should be undoable");
        assert_eq!(editor.current_style(), before);
    }

    #[test]
    fn invalid_style_inputs_do_not_mutate_the_editor() {
        for invalid in ["NaN", "-1", "201", "not a number"] {
            let mut editor = text_editor();
            let before = editor.current_style();
            let mut values = inputs();
            values.width = invalid.to_owned();
            assert!(
                apply_style(&mut editor, &values).is_err(),
                "width {invalid:?}"
            );
            assert_eq!(
                editor.current_style(),
                before,
                "invalid input must not edit the style"
            );
        }
        let mut editor = text_editor();
        let before = editor.current_style();
        let mut values = inputs();
        values.stroke = "#12345".to_owned();
        assert!(apply_style(&mut editor, &values).is_err());
        assert_eq!(editor.current_style(), before);
        values = inputs();
        values.font = "  ".to_owned();
        assert!(apply_style(&mut editor, &values).is_err());
        assert_eq!(editor.current_style(), before);
        for (field, invalid) in [
            ("font_size", "3"),
            ("font_size", "401"),
            ("opacity", "4"),
            ("opacity", "101"),
        ] {
            let mut values = inputs();
            if field == "font_size" {
                values.font_size = invalid.to_owned();
            } else {
                values.opacity = invalid.to_owned();
            }
            assert!(
                apply_style(&mut editor, &values).is_err(),
                "{field} {invalid}"
            );
            assert_eq!(editor.current_style(), before);
        }
    }

    #[test]
    fn step_size_and_style_are_a_single_undoable_change() {
        let mut editor = Editor::new(Document::blank(80, 60, Rgba::WHITE));
        editor.add_object(
            "Step",
            Kind::Step {
                center: Pt::new(30.0, 30.0),
                radius: 12.0,
                label: "1".to_owned(),
                shape: ShapeKind::Ellipse,
            },
            Style::default(),
        );
        let before = editor.selected().expect("step should be selected").clone();
        apply_style(&mut editor, &inputs()).expect("valid properties should apply");
        let after = editor.selected().expect("step should remain selected");
        assert!(matches!(after.kind, Kind::Step { radius, .. } if radius == 36.0 * 0.85));
        assert!(editor.undo(), "changing size and style should be undoable");
        assert_eq!(
            editor.selected(),
            Some(&before),
            "one undo must restore both size and style"
        );
    }

    #[test]
    fn auto_trim_crops_border_and_is_undoable() {
        let mut document = Document::blank(20, 20, Rgba::WHITE);
        for y in 4..16 {
            for x in 3..17 {
                raster::set_pixel(document.background_mut(), x, y, Rgba::RED);
            }
        }
        let mut editor = Editor::new(document);
        auto_trim(&mut editor).expect("solid margins should trim");
        assert_eq!(editor.document().size(), (14, 12));
        assert_eq!(
            raster::pixel(editor.document().background(), 0, 0),
            Rgba::RED
        );
        assert!(editor.undo(), "trimming should be undoable");
        assert_eq!(editor.document().size(), (20, 20));
        assert_eq!(
            raster::pixel(editor.document().background(), 0, 0),
            Rgba::WHITE
        );
        let mut empty = Editor::new(Document::blank(10, 10, Rgba::WHITE));
        assert!(auto_trim(&mut empty).is_err());
        assert_eq!(empty.document().size(), (10, 10));
        assert!(empty.undo_label().is_none());
    }

    #[test]
    fn smart_redact_covers_matching_lines_as_one_undoable_edit() {
        let mut editor = Editor::new(Document::blank(300, 100, Rgba::WHITE));
        let lines = vec![
            TextLine {
                area: IRect::new(10, 10, 220, 20),
                text: "Email person@example.com for help".to_owned(),
                words: Vec::new(),
            },
            TextLine {
                area: IRect::new(10, 40, 100, 20),
                text: "Nothing sensitive".to_owned(),
                words: Vec::new(),
            },
        ];
        let color = editor.tool_style(Tool::Blur).fill.with_alpha(255);
        assert_eq!(smart_redact(&mut editor, &lines), 1);
        assert_eq!(editor.document().objects.len(), 1);
        assert!(matches!(
            editor.document().objects[0].kind,
            Kind::Blur { rect, mode: BlurMode::Redact, .. }
                if rect == Box2::from_xywh(7.0, 7.0, 226.0, 26.0)
        ));
        assert_eq!(raster::pixel(&editor.document().flatten(), 15, 15), color);
        assert!(editor.undo(), "all redactions should undo together");
        assert!(editor.document().objects.is_empty());
        assert_eq!(smart_redact(&mut editor, &lines[1..]), 0);
        assert!(editor.document().objects.is_empty());
    }

    #[test]
    fn document_snapshots_detect_background_object_and_image_changes() {
        let mut original = Document::blank(20, 20, Rgba::WHITE);
        let image = raster::solid(2, 2, Rgba::RED).expect("image should be valid");
        let image_id = original.add(
            Kind::Image {
                rect: Box2::from_xywh(0.0, 0.0, 2.0, 2.0),
                image: ImageRef::new("same-key".to_owned(), image),
            },
            Style::default(),
        );
        let snapshot = original.clone();
        assert!(same_document(&snapshot, &original));

        let mut background_changed = original.clone();
        raster::set_pixel(background_changed.background_mut(), 10, 10, Rgba::BLACK);
        assert!(!same_document(&snapshot, &background_changed));

        let mut object_changed = original.clone();
        object_changed
            .get_mut(image_id)
            .expect("image should exist")
            .rotation = 30.0;
        assert!(!same_document(&snapshot, &object_changed));

        let mut image_changed = original.clone();
        let Kind::Image { image, .. } = &mut image_changed
            .get_mut(image_id)
            .expect("image should exist")
            .kind
        else {
            panic!("object should remain an image");
        };
        let pixels = image.pixmap.as_mut().expect("image pixels should exist");
        raster::set_pixel(Arc::make_mut(pixels), 0, 0, Rgba::BLUE);
        assert_eq!(
            snapshot.objects, image_changed.objects,
            "image equality intentionally compares the key, not its pixels"
        );
        assert!(!same_document(&snapshot, &image_changed));

        let nested = original.get(image_id).expect("image should exist").clone();
        let mut group = Document::blank(20, 20, Rgba::WHITE);
        let group_id = group.add(
            Kind::Group {
                objects: vec![nested],
            },
            Style::default(),
        );
        let grouped_snapshot = group.clone();
        let Kind::Group { objects } = &mut group.get_mut(group_id).unwrap().kind else {
            panic!("object should remain a group");
        };
        let Kind::Image { image, .. } = &mut objects[0].kind else {
            panic!("group should contain an image");
        };
        raster::set_pixel(
            Arc::make_mut(image.pixmap.as_mut().expect("group image should exist")),
            0,
            0,
            Rgba::BLUE,
        );
        assert!(!same_document(&grouped_snapshot, &group));
    }

    #[test]
    fn recognized_text_preserves_line_order_and_omits_blank_lines() {
        let lines = [
            TextLine {
                area: IRect::new(0, 0, 1, 1),
                text: " first ".to_owned(),
                words: Vec::new(),
            },
            TextLine {
                area: IRect::new(0, 1, 1, 1),
                text: "  ".to_owned(),
                words: Vec::new(),
            },
            TextLine {
                area: IRect::new(0, 2, 1, 1),
                text: "second".to_owned(),
                words: Vec::new(),
            },
        ];
        assert_eq!(recognized_text(&lines), "first\nsecond");
    }

    #[test]
    fn style_changes_without_a_selection_apply_to_the_current_tool() {
        let mut editor = Editor::new(Document::blank(80, 60, Rgba::WHITE));
        editor.set_tool(jiffshot_doc::editor::Tool::Text);
        apply_style(&mut editor, &inputs()).expect("valid tool style should apply");
        assert_eq!(editor.current_style().stroke, Rgba::rgb(0x12, 0x34, 0x56));
        assert_eq!(editor.current_style().font_size, 36.0);
    }

    #[test]
    fn image_effects_keep_annotations_and_can_be_undone() {
        let mut editor = Editor::new(Document::blank(20, 20, Rgba::rgb(200, 30, 10)));
        editor.add_object(
            "Text",
            Kind::Text {
                rect: Box2::from_xywh(1.0, 1.0, 10.0, 10.0),
                text: "Keep".to_owned(),
            },
            Style::default(),
        );
        let before = editor.document().background().data().to_vec();
        apply_effect(&mut editor, "Grayscale", "").expect("grayscale effect should apply");
        assert_ne!(editor.document().background().data(), before.as_slice());
        assert_eq!(
            editor.document().objects.len(),
            1,
            "annotations must remain editable"
        );
        assert!(editor.undo(), "image effects should be undoable");
        assert_eq!(editor.document().background().data(), before.as_slice());
        assert!(apply_effect(&mut editor, "not an effect", "").is_err());
        assert_eq!(editor.document().background().data(), before.as_slice());
    }

    #[test]
    fn effect_parameters_preserve_defaults_and_apply_an_undoable_border() {
        let mut editor = Editor::new(Document::blank(20, 20, Rgba::WHITE));
        let original = editor.document().clone();
        apply_effect(&mut editor, "Border", "").expect("the default border should apply");
        assert_eq!(editor.document().size(), (36, 36));
        assert!(editor.undo(), "the default border must be undoable");
        assert!(same_document(&original, editor.document()));

        apply_effect(&mut editor, "Border", " 3 ").expect("the configured border should apply");
        assert_eq!(editor.document().size(), (26, 26));
        assert_eq!(
            raster::pixel(editor.document().background(), 0, 0),
            editor.current_style().stroke
        );
        assert_eq!(
            raster::pixel(editor.document().background(), 3, 3),
            Rgba::WHITE
        );
        assert!(editor.undo(), "the configured border must be undoable");
        assert!(same_document(&original, editor.document()));
    }

    #[test]
    fn effect_parameters_map_to_the_selected_effect_without_changing_other_options() {
        let effect = |name, value| {
            parameterized_effect(name, value, Rgba::RED, Rgba::BLUE)
                .expect("the configured effect should be valid")
        };
        assert!(matches!(
            effect("Border", "13"),
            effects::Effect::Border { width: 13, color } if color == Rgba::RED
        ));
        assert!(matches!(
            effect("Shadow", "22"),
            effects::Effect::Shadow { size: 22 }
        ));
        assert!(matches!(
            effect("Torn edge", "11"),
            effects::Effect::TornEdge { depth: 11 }
        ));
        for (name, expected_style, expected_sides) in [
            (
                "Torn bottom",
                effects::EdgeStyle::Torn,
                [false, false, true, false],
            ),
            ("Sawtooth edge", effects::EdgeStyle::Sawtooth, [true; 4]),
            ("Wave edge", effects::EdgeStyle::Wave, [true; 4]),
        ] {
            assert!(
                matches!(
                    effect(name, "9"),
                    effects::Effect::Edges { style, depth: 9, sides }
                        if style == expected_style && sides == expected_sides
                ),
                "{name} must preserve its edge style and sides"
            );
        }
        for (name, expected_tilt) in [("Perspective", false), ("Tilt", true)] {
            assert!(
                matches!(
                    effect(name, "25"),
                    effects::Effect::Perspective { amount, tilt }
                        if amount == 0.25 && tilt == expected_tilt
                ),
                "{name} must preserve its orientation"
            );
        }
        assert!(matches!(
            effect("Rounded corners", "32"),
            effects::Effect::RoundedCorners { radius: 32 }
        ));
        assert!(matches!(
            effect("Fade edges", "18"),
            effects::Effect::FadeEdges { size: 18 }
        ));
        assert!(matches!(
            effect("Blur image", "7"),
            effects::Effect::BlurImage(7)
        ));
        assert!(matches!(
            effect("Scale to 1920", "640"),
            effects::Effect::ScaleTo(640)
        ));
        for name in ["Brighten", "Darken"] {
            assert!(matches!(
                effect(name, "-25"),
                effects::Effect::Adjust { brightness, contrast }
                    if brightness == -0.25 && contrast == 0.0
            ));
        }
        for name in ["More contrast", "Less contrast"] {
            assert!(matches!(
                effect(name, "50"),
                effects::Effect::Adjust { brightness, contrast }
                    if brightness == 0.0 && contrast == 0.5
            ));
        }
        for name in ["Saturate", "Desaturate"] {
            assert!(matches!(
                effect(name, "75"),
                effects::Effect::Saturation(value) if value == 0.75
            ));
        }
        assert!(matches!(
            effect("Hue shift", "-90"),
            effects::Effect::HueShift(value) if value == -90.0
        ));
        for name in ["Gamma up", "Gamma down"] {
            assert!(matches!(
                effect(name, "2.5"),
                effects::Effect::Gamma(value) if value == 2.5
            ));
        }
    }

    #[test]
    fn effect_parameters_validate_catalog_boundaries_and_reject_nonfinite_values() {
        for (name, _) in effects::catalog(Rgba::BLACK) {
            let Some(spec) = effect_parameter_spec(name) else {
                assert!(
                    parameterized_effect(name, "1", Rgba::BLACK, Rgba::WHITE).is_err(),
                    "{name} must reject unexpected parameters"
                );
                continue;
            };
            match spec.kind {
                EffectParameterKind::Whole { min, max } => {
                    for valid in [min, max] {
                        assert!(
                            parameterized_effect(
                                name,
                                &valid.to_string(),
                                Rgba::BLACK,
                                Rgba::WHITE
                            )
                            .is_ok(),
                            "{name} must accept its boundary {valid}"
                        );
                    }
                    for invalid in [min - 1, max + 1] {
                        assert!(
                            parse_effect_parameter(name, &invalid.to_string()).is_err(),
                            "{name} must reject out-of-range {invalid}"
                        );
                    }
                    assert!(parse_effect_parameter(name, "1.5").is_err(), "{name}");
                }
                EffectParameterKind::Decimal { min, max } => {
                    for valid in [min, max] {
                        assert!(
                            parameterized_effect(
                                name,
                                &valid.to_string(),
                                Rgba::BLACK,
                                Rgba::WHITE
                            )
                            .is_ok(),
                            "{name} must accept its boundary {valid}"
                        );
                    }
                    for invalid in [min - 1.0, max + 1.0] {
                        assert!(
                            parse_effect_parameter(name, &invalid.to_string()).is_err(),
                            "{name} must reject out-of-range {invalid}"
                        );
                    }
                    for invalid in ["NaN", "inf", "-inf"] {
                        assert!(parse_effect_parameter(name, invalid).is_err(), "{name}");
                    }
                }
            }
        }
        assert_eq!(watermark_opacity("").expect("default opacity"), 0.35);
        assert_eq!(watermark_opacity("80").expect("custom opacity"), 0.8);
        for value in ["-1", "101", "NaN", "not a number"] {
            assert!(watermark_opacity(value).is_err(), "opacity {value}");
        }
        assert!(validate_effect_parameter("Capture info", "3").is_err());
        assert!(validate_effect_parameter("Capture info", "  ").is_ok());
        assert!(parse_effect_parameter("Spotlight & magnify", "0.9").is_err());
        assert!(parse_effect_parameter("Spotlight & magnify", "8.1").is_err());
        assert!(parse_effect_parameter("Spotlight & magnify", "1").is_ok());
        assert!(parse_effect_parameter("Spotlight & magnify", "8").is_ok());
    }

    #[test]
    fn invalid_effect_parameters_do_not_change_the_document_or_history() {
        let mut editor = Editor::new(Document::blank(20, 20, Rgba::rgb(10, 20, 30)));
        let original = editor.document().clone();
        for (name, value) in [
            ("Border", "1.5"),
            ("Border", "257"),
            ("Gamma up", "NaN"),
            ("Grayscale", "5"),
            ("Spotlight & magnify", "9"),
        ] {
            assert!(
                apply_effect(&mut editor, name, value).is_err(),
                "{name} must reject {value}"
            );
            assert!(same_document(&original, editor.document()), "{name}");
            assert!(editor.undo_label().is_none(), "{name}");
        }
        apply_effect(&mut editor, "Brighten", "50").expect("brightness should apply");
        assert_eq!(
            raster::pixel(editor.document().background(), 10, 10),
            Rgba::rgb(138, 148, 158)
        );
        assert!(editor.undo(), "the brightness change must be undoable");
        assert!(same_document(&original, editor.document()));
    }

    #[test]
    fn spotlight_parameter_controls_magnification_and_undoes() {
        let mut editor = Editor::new(Document::blank(120, 90, Rgba::WHITE));
        editor.add_object(
            "Area",
            Kind::Shape {
                rect: Box2::from_xywh(10.0, 20.0, 20.0, 20.0),
                shape: ShapeKind::Rectangle,
            },
            Style::default(),
        );
        let original = editor.document().clone();
        apply_effect(&mut editor, "Spotlight & magnify", "2")
            .expect("custom spotlight magnification should apply");
        assert!(matches!(
            editor.document().objects[1].kind,
            Kind::Image { rect, .. } if rect == Box2::from_xywh(0.0, 10.0, 40.0, 40.0)
        ));
        assert!(editor.undo(), "the custom spotlight must be undoable");
        assert!(same_document(&original, editor.document()));
    }

    #[test]
    fn interactive_effects_capture_info_formats_source_sidecar() {
        let directory = tempfile::tempdir().expect("create a temporary capture directory");
        let source = directory.path().join("capture.png");
        std::fs::write(
            source.with_extension("json"),
            r#"{"captured_at":" 2026-03-01T10:20:30Z ","window_title":" Design  \t review\n","process":" Browser ","tags":["ignored"]}"#,
        )
        .expect("write capture metadata");

        assert_eq!(
            capture_info(Some(&source)).expect("format capture metadata"),
            "2026-03-01 10:20:30Z  ·  Design review  ·  Browser  ·  Linux"
        );

        std::fs::write(
            source.with_extension("json"),
            r#"{"captured_at":"","window_title":" \t ","process":"Terminal"}"#,
        )
        .expect("write partial capture metadata");
        assert_eq!(
            capture_info(Some(&source)).expect("format partial capture metadata"),
            "Terminal  ·  Linux"
        );
    }

    #[test]
    fn interactive_effects_capture_info_defaults_to_local_time_and_linux() {
        let directory = tempfile::tempdir().expect("create a temporary capture directory");
        let missing = directory.path().join("missing.png");
        let empty = directory.path().join("empty.png");
        std::fs::write(
            empty.with_extension("json"),
            r#"{"captured_at":"  ","window_title":"","process":"\t"}"#,
        )
        .expect("write empty capture metadata");

        for source in [None, Some(missing.as_path()), Some(empty.as_path())] {
            let local_time = || {
                gstreamer::glib::DateTime::now_local()
                    .expect("read local time")
                    .format("%Y-%m-%d %H:%M:%S")
                    .expect("format local time")
                    .to_string()
            };
            let before = local_time();
            let actual = capture_info(source).expect("format default capture information");
            let after = local_time();
            let timestamp = actual
                .strip_suffix("  ·  Linux")
                .expect("default information must end with Linux");
            let is_between = if before <= after {
                before.as_str() <= timestamp && timestamp <= after.as_str()
            } else {
                // Local clocks can move backwards at a daylight-saving transition.
                before.as_str() <= timestamp || timestamp <= after.as_str()
            };
            assert!(
                is_between,
                "expected a local timestamp between {before} and {after}, got {timestamp}"
            );
        }
    }

    #[test]
    fn interactive_effects_capture_info_rejects_malformed_and_wrong_type_sidecars() {
        let directory = tempfile::tempdir().expect("create a temporary capture directory");
        let source = directory.path().join("capture.png");
        let sidecar = source.with_extension("json");
        for contents in [
            r#"{"window_title":"unfinished""#,
            r#"{"captured_at":123,"window_title":"Editor"}"#,
        ] {
            std::fs::write(&sidecar, contents).expect("write invalid metadata");
            let error = capture_info(Some(&source)).expect_err("invalid metadata must fail");
            assert!(
                error.starts_with("could not parse capture metadata "),
                "unexpected error for {contents}: {error}"
            );
            assert!(
                error.contains(&sidecar.display().to_string()),
                "error must identify the sidecar: {error}"
            );
        }
    }

    #[test]
    fn interactive_effects_capture_info_reports_unreadable_sidecars() {
        let directory = tempfile::tempdir().expect("create a temporary capture directory");
        let source = directory.path().join("capture.png");
        let sidecar = source.with_extension("json");
        std::fs::create_dir(&sidecar).expect("make the sidecar path a directory");
        let error = capture_info(Some(&source)).expect_err("reading a directory must fail");
        assert!(
            error.starts_with("could not read capture metadata "),
            "{error}"
        );
        assert!(
            error.contains(&sidecar.display().to_string()),
            "error must identify the sidecar: {error}"
        );
    }

    #[test]
    fn interactive_effects_capture_info_enforces_unicode_length_boundary() {
        let directory = tempfile::tempdir().expect("create a temporary capture directory");
        let source = directory.path().join("capture.png");
        let sidecar = source.with_extension("json");
        for title_length in [1014, 1015] {
            let title = "影".repeat(title_length);
            let metadata = serde_json::json!({ "window_title": title });
            std::fs::write(&sidecar, metadata.to_string()).expect("write capture metadata");
            let result = capture_info(Some(&source));
            if title_length == 1014 {
                let info = result.expect("1,024 characters must be accepted");
                assert_eq!(info.chars().count(), 1024);
                assert_eq!(info, format!("{}  ·  Linux", "影".repeat(title_length)));
            } else {
                assert_eq!(
                    result.expect_err("1,025 characters must be rejected"),
                    "Capture information is too long to add to the image."
                );
            }
        }
    }

    #[test]
    fn interactive_effects_capture_info_band_preserves_content_and_undoes() {
        let mut editor = Editor::new(Document::blank(240, 140, Rgba::rgb(30, 120, 190)));
        editor.add_object(
            "Annotation",
            Kind::Shape {
                rect: Box2::from_xywh(10.0, 10.0, 20.0, 20.0),
                shape: ShapeKind::Rectangle,
            },
            Style::default(),
        );
        let original_objects = editor.document().objects.clone();
        let original_pixels = editor.document().background().data().to_vec();
        apply_document_effect(
            &mut editor,
            "Capture info",
            effects::Effect::CaptureInfo {
                text: "2026-03-01 10:20:30  ·  Browser  ·  Linux".to_owned(),
            },
        )
        .expect("add a capture information band");

        let (width, height) = editor.document().size();
        assert_eq!(width, 240);
        assert!(height > 140, "the information must add a band");
        assert_eq!(
            raster::pixel(editor.document().background(), 0, 0),
            Rgba::rgb(30, 120, 190)
        );
        assert_eq!(
            &editor.document().background().data()[..original_pixels.len()],
            original_pixels.as_slice(),
            "the band must not alter any original image pixels"
        );
        assert_eq!(
            raster::pixel(editor.document().background(), 0, 140),
            Rgba::rgb(0x22, 0x26, 0x2e)
        );
        assert!(
            (140..height as i32).any(|y| (0..width as i32).any(|x| {
                raster::pixel(editor.document().background(), x, y) != Rgba::rgb(0x22, 0x26, 0x2e)
            })),
            "the band must contain visible capture information"
        );
        assert_eq!(editor.document().objects, original_objects);
        assert_eq!(editor.undo_label(), Some("Capture info"));
        assert!(editor.undo(), "one undo must remove the band");
        assert_eq!(editor.document().size(), (240, 140));
        assert_eq!(editor.document().background().data(), original_pixels);
        assert_eq!(editor.document().objects, original_objects);
    }

    #[test]
    fn interactive_effects_capture_info_rejects_narrow_images_without_changing_document() {
        for width in [8, 24] {
            let mut editor = Editor::new(Document::blank(width, 20, Rgba::WHITE));
            let id = editor.add_object(
                "Annotation",
                Kind::Shape {
                    rect: Box2::from_xywh(1.0, 1.0, 4.0, 4.0),
                    shape: ShapeKind::Rectangle,
                },
                Style::default(),
            );
            let original = editor.document().clone();
            let error = apply_document_effect(
                &mut editor,
                "Capture info",
                effects::Effect::CaptureInfo {
                    text: "Linux".to_owned(),
                },
            )
            .expect_err("an image at or below 24 pixels must be rejected");
            assert_eq!(
                error,
                "The image must be at least 25 pixels wide for capture information."
            );
            assert!(
                same_document(&original, editor.document()),
                "rejection must preserve the entire {width}-pixel-wide document"
            );
            assert_eq!(editor.selection(), Some(id));
            assert_eq!(editor.undo_label(), Some("Annotation"));
        }

        let mut minimum = Editor::new(Document::blank(25, 20, Rgba::WHITE));
        let original = minimum.document().clone();
        apply_document_effect(
            &mut minimum,
            "Capture info",
            effects::Effect::CaptureInfo {
                text: "Linux".to_owned(),
            },
        )
        .expect("an image 25 pixels wide must accept capture information");
        assert!(minimum.document().height() > 20);
        assert!(
            (20..minimum.document().height() as i32).any(|y| (0..25).any(|x| {
                raster::pixel(minimum.document().background(), x, y) != Rgba::rgb(0x22, 0x26, 0x2e)
            })),
            "the minimum-width band must contain visible text"
        );
        assert!(minimum.undo(), "the minimum-width band must be undoable");
        assert!(same_document(&original, minimum.document()));
    }

    #[test]
    fn interactive_effects_watermark_blends_bottom_right_at_35_percent_and_undoes() {
        let mut editor = Editor::new(Document::blank(80, 80, Rgba::WHITE));
        editor.add_object(
            "Annotation",
            Kind::Shape {
                rect: Box2::from_xywh(5.0, 5.0, 10.0, 10.0),
                shape: ShapeKind::Rectangle,
            },
            Style::default(),
        );
        let original_objects = editor.document().objects.clone();
        let original_pixels = editor.document().background().data().to_vec();
        let image =
            raster::solid(16, 16, Rgba::rgb(255, 0, 0)).expect("create an opaque red watermark");
        apply_document_effect(
            &mut editor,
            "Watermark",
            effects::Effect::Watermark {
                image: Arc::new(image),
                opacity: 0.35,
            },
        )
        .expect("apply the watermark");

        assert_eq!(editor.document().size(), (80, 80));
        assert_eq!(
            raster::pixel(editor.document().background(), 60, 60),
            Rgba::rgb(255, 166, 166),
            "35% red over white must produce a pale red watermark"
        );
        for (x, y) in [(0, 0), (55, 60), (72, 60), (60, 55), (60, 72)] {
            assert_eq!(
                raster::pixel(editor.document().background(), x, y),
                Rgba::WHITE,
                "pixel ({x}, {y}) is outside the watermark"
            );
        }
        assert_eq!(editor.document().objects, original_objects);
        assert_eq!(editor.undo_label(), Some("Watermark"));
        assert!(editor.undo(), "one undo must remove the watermark");
        assert_eq!(editor.document().background().data(), original_pixels);
        assert_eq!(editor.document().objects, original_objects);
    }

    #[test]
    fn interactive_effects_spotlight_uses_selected_area_and_undoes() {
        let mut document = Document::blank(120, 90, Rgba::WHITE);
        raster::set_pixel(document.background_mut(), 12, 22, Rgba::BLUE);
        let mut editor = Editor::new(document);
        let selected = editor.add_object(
            "Area",
            Kind::Shape {
                rect: Box2::from_xywh(10.0, 20.0, 20.0, 20.0),
                shape: ShapeKind::Rectangle,
            },
            Style::default(),
        );
        let original_objects = editor.document().objects.clone();
        let original_pixels = editor.document().background().data().to_vec();
        apply_effect(&mut editor, "Spotlight & magnify", "").expect("magnify the selected area");

        let objects = &editor.document().objects;
        assert_eq!(objects.len(), 2);
        assert!(editor.document().get(selected).is_none());
        let expected = Box2::from_xywh(4.0, 14.0, 32.0, 32.0);
        assert!(matches!(
            objects[0].kind,
            Kind::Spotlight { rect, dim, .. } if rect == expected && dim == 0.55
        ));
        let Kind::Image { rect, image } = &objects[1].kind else {
            panic!("the selected area must become a magnified image");
        };
        assert_eq!(*rect, expected);
        let piece = image.pixmap.as_ref().expect("magnified image has pixels");
        assert_eq!((piece.width(), piece.height()), (20, 20));
        assert_eq!(raster::pixel(piece, 2, 2), Rgba::BLUE);
        assert_eq!(editor.undo_label(), Some("Spotlight & magnify"));
        assert!(
            editor.undo(),
            "one undo must restore the selected annotation"
        );
        assert_eq!(editor.document().objects, original_objects);
        assert_eq!(editor.document().background().data(), original_pixels);
    }

    #[test]
    fn interactive_effects_spotlight_uses_image_center_without_selection() {
        let mut editor = Editor::new(Document::blank(120, 90, Rgba::WHITE));
        apply_effect(&mut editor, "Spotlight & magnify", "").expect("magnify the image center");
        let expected = Box2::from_xywh(28.0, 21.0, 64.0, 48.0);
        assert!(matches!(
            editor.document().objects.as_slice(),
            [
                Object {
                    kind: Kind::Spotlight { rect, .. },
                    ..
                },
                Object {
                    kind: Kind::Image { rect: image_rect, .. },
                    ..
                }
            ] if *rect == expected && *image_rect == expected
        ));
        assert!(editor.undo(), "the centered spotlight must be undoable");
        assert!(editor.document().objects.is_empty());
        assert_eq!(editor.document().size(), (120, 90));
    }

    #[test]
    fn interactive_effects_spotlight_rejects_small_and_outside_selections_without_losing_them() {
        for (description, rect) in [
            ("narrow", Box2::from_xywh(10.0, 10.0, 3.0, 10.0)),
            ("short", Box2::from_xywh(10.0, 10.0, 10.0, 3.0)),
            ("left", Box2::from_xywh(-20.0, 10.0, 10.0, 10.0)),
            ("right", Box2::from_xywh(130.0, 10.0, 10.0, 10.0)),
            ("above", Box2::from_xywh(10.0, -20.0, 10.0, 10.0)),
            ("below", Box2::from_xywh(10.0, 100.0, 10.0, 10.0)),
            ("partly outside", Box2::from_xywh(118.0, 10.0, 10.0, 10.0)),
        ] {
            let mut editor = Editor::new(Document::blank(120, 90, Rgba::WHITE));
            let id = editor.add_object(
                "Area",
                Kind::Shape {
                    rect,
                    shape: ShapeKind::Rectangle,
                },
                Style::default(),
            );
            let original = editor.selected().expect("area is selected").clone();
            let pixels = editor.document().background().data().to_vec();
            let error = apply_effect(&mut editor, "Spotlight & magnify", "")
                .expect_err("an invalid selected area must be rejected");
            assert_eq!(
                error, "Select an area within the image large enough to magnify.",
                "{description}"
            );
            assert_eq!(editor.selection(), Some(id), "{description}");
            assert_eq!(editor.selected(), Some(&original), "{description}");
            assert_eq!(editor.document().objects, vec![original], "{description}");
            assert_eq!(
                editor.document().background().data(),
                pixels,
                "{description}"
            );
            assert_eq!(editor.undo_label(), Some("Area"), "{description}");
        }
    }

    #[test]
    fn interactive_effects_spotlight_accepts_minimum_area_and_rejects_tiny_images() {
        let mut editor = Editor::new(Document::blank(120, 90, Rgba::WHITE));
        editor.add_object(
            "Area",
            Kind::Shape {
                rect: Box2::from_xywh(10.0, 10.0, 4.0, 4.0),
                shape: ShapeKind::Rectangle,
            },
            Style::default(),
        );
        apply_effect(&mut editor, "Spotlight & magnify", "")
            .expect("a four-by-four selected area must be accepted");
        assert_eq!(editor.document().objects.len(), 2);
        assert!(editor.undo(), "the minimum-area effect must be undoable");

        for (width, height) in [(11, 12), (12, 11)] {
            let mut editor = Editor::new(Document::blank(width, height, Rgba::WHITE));
            assert_eq!(
                apply_effect(&mut editor, "Spotlight & magnify", "")
                    .expect_err("a too-small centered area must be rejected"),
                "Select an area within the image large enough to magnify."
            );
            assert!(editor.document().objects.is_empty());
            assert!(editor.undo_label().is_none());
        }
        let mut minimum = Editor::new(Document::blank(12, 12, Rgba::WHITE));
        apply_effect(&mut minimum, "Spotlight & magnify", "")
            .expect("a twelve-by-twelve image has a four-by-four center");
        assert_eq!(minimum.document().objects.len(), 2);

        let mut clipped = Editor::new(Document::blank(120, 90, Rgba::WHITE));
        clipped.add_object(
            "Area",
            Kind::Shape {
                rect: Box2::from_xywh(116.0, 10.0, 10.0, 10.0),
                shape: ShapeKind::Rectangle,
            },
            Style::default(),
        );
        let original_objects = clipped.document().objects.clone();
        apply_effect(&mut clipped, "Spotlight & magnify", "")
            .expect("a partially outside area with four pixels inside must be accepted");
        let Kind::Image { image, .. } = &clipped.document().objects[1].kind else {
            panic!("the clipped area must become a magnified image");
        };
        let piece = image.pixmap.as_ref().expect("clipped image has pixels");
        assert_eq!((piece.width(), piece.height()), (4, 10));
        assert!(clipped.undo(), "undo must restore the clipped selection");
        assert_eq!(clipped.document().objects, original_objects);
    }

    #[test]
    fn interactive_effects_pending_work_rejects_replaced_and_superseded_documents() {
        let requests = DocumentLoadRequests::new();
        let mut editor = Editor::new(Document::blank(80, 60, Rgba::WHITE));
        let original = requests.begin(editor.document().clone());
        editor.load(Document::blank(80, 60, Rgba::WHITE));
        assert_eq!(
            requests.state(&original, editor.document()),
            DocumentLoadState::DocumentChanged,
            "a newly loaded document is not the original even if its pixels match"
        );
        let newer = requests.begin(editor.document().clone());
        assert_eq!(
            requests.state(&original, editor.document()),
            DocumentLoadState::Superseded
        );
        assert_eq!(
            requests.state(&newer, editor.document()),
            DocumentLoadState::Ready
        );
    }

    #[test]
    fn simplify_flattens_annotations_adds_editable_bars_and_undoes() {
        let mut document = Document::blank(80, 40, Rgba::WHITE);
        document.add(
            Kind::Text {
                rect: Box2::from_xywh(4.0, 4.0, 60.0, 24.0),
                text: "Sample".to_owned(),
            },
            Style {
                text_color: Rgba::BLACK,
                font_size: 16.0,
                ..Style::default()
            },
        );
        let original_objects = document.objects.clone();
        let original_pixels = document.background().data().to_vec();
        let lines = [TextLine {
            area: IRect::new(4, 4, 50, 20),
            text: "Sample".to_owned(),
            words: Vec::new(),
        }];
        let (simplified, bars) = simplify_document(&document, &lines);
        assert_eq!(bars, 1);
        assert!(matches!(
            simplified.objects.as_slice(),
            [Object {
                kind: Kind::Shape {
                    shape: ShapeKind::Rounded,
                    ..
                },
                ..
            }]
        ));
        let snapshot = document.clone();
        let mut editor = Editor::new(document);
        let temporary_id = editor.add_object(
            "Temporary annotation",
            Kind::Shape {
                rect: Box2::from_xywh(0.0, 0.0, 2.0, 2.0),
                shape: ShapeKind::Rectangle,
            },
            Style::default(),
        );
        assert!(editor.delete_selected());
        assert!(same_document(&snapshot, editor.document()));
        apply_simplification(&mut editor, simplified);
        assert_eq!(editor.document().objects.len(), 1);
        assert!(
            editor.document().objects[0].id > temporary_id,
            "simplified objects must use fresh ids from the current document"
        );
        assert!(
            editor.undo(),
            "simplification should undo as a single operation"
        );
        assert_eq!(editor.document().objects, original_objects);
        assert_eq!(editor.document().background().data(), original_pixels);
    }
}
