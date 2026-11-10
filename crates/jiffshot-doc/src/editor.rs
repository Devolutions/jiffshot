//! Interactive editing: tools, selection, handles, undo/redo, and the
//! display raster kept up to date with partial re-renders.
//!
//! The UI forwards pointer events in document pixels; every method that
//! changes something records the changed area, and [`Editor::take_damage`]
//! re-renders just that area into [`Editor::display`].

use crate::color::Rgba;
use crate::document::{allocate_canvas_pixmap, CanvasResizeError, Document};
use crate::geom::{Box2, IRect, Pt};
use crate::object::*;
use crate::raster;
use crate::stamps::Stamp;
use crate::text::Align;
use std::collections::{HashMap, HashSet};
use tiny_skia::Pixmap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tool {
    Move,
    Select,
    Arrow,
    Line,
    Shape,
    Text,
    Callout,
    Step,
    Pen,
    Highlight,
    Blur,
    Magnify,
    Spotlight,
    Stamp,
    Fill,
    Crop,
    CutOut,
    Selection,
    MagicWand,
    Eraser,
    Eyedropper,
    SmartMove,
    TextReplace,
}

pub const TOOLS: &[Tool] = &[
    Tool::Select,
    Tool::Move,
    Tool::Arrow,
    Tool::Line,
    Tool::Shape,
    Tool::Text,
    Tool::Callout,
    Tool::Step,
    Tool::Pen,
    Tool::Highlight,
    Tool::Blur,
    Tool::Magnify,
    Tool::Spotlight,
    Tool::Stamp,
    Tool::Fill,
    Tool::Crop,
    Tool::CutOut,
    Tool::Selection,
    Tool::MagicWand,
    Tool::Eraser,
    Tool::Eyedropper,
    Tool::SmartMove,
    Tool::TextReplace,
];

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::Move => "Move",
            Tool::Select => "Select",
            Tool::Arrow => "Arrow",
            Tool::Line => "Line",
            Tool::Shape => "Shape",
            Tool::Text => "Text",
            Tool::Callout => "Callout",
            Tool::Step => "Step",
            Tool::Pen => "Pen",
            Tool::Highlight => "Highlight",
            Tool::Blur => "Blur",
            Tool::Magnify => "Magnify",
            Tool::Spotlight => "Spotlight",
            Tool::Stamp => "Stamp",
            Tool::Fill => "Fill",
            Tool::Crop => "Crop",
            Tool::CutOut => "Cut Out",
            Tool::Selection => "Selection",
            Tool::MagicWand => "Magic Wand",
            Tool::Eraser => "Eraser",
            Tool::Eyedropper => "Eyedropper",
            Tool::SmartMove => "Smart Move",
            Tool::TextReplace => "Text Replace",
        }
    }

    pub fn from_name(name: &str) -> Option<Tool> {
        let key: String = name.chars().filter(|c| !c.is_whitespace()).collect();
        TOOLS
            .iter()
            .copied()
            .find(|t| t.name().replace(' ', "").eq_ignore_ascii_case(&key))
    }

    /// Choices for the tool's variant, first is the default.
    pub fn variants(self) -> &'static [&'static str] {
        match self {
            Tool::Shape => &["Rectangle", "Rounded", "Ellipse", "Triangle", "Polygon"],
            Tool::Callout => &["Rounded", "Rectangle", "Ellipse", "Arrow"],
            Tool::Highlight => &["Rectangle", "Rounded", "Ellipse", "Freehand"],
            Tool::Pen => &["Pen", "Marker"],
            Tool::Blur => &["Blur", "Pixelate", "Redact"],
            Tool::Magnify => &[
                "Circle",
                "Rectangle",
                "Circle with view",
                "Rectangle with view",
            ],
            Tool::Spotlight => &["Rectangle", "Rounded", "Ellipse"],
            Tool::Stamp => &[
                "Check",
                "Cross",
                "Star",
                "Heart",
                "Warning",
                "Info",
                "Question",
                "Exclamation",
                "Plus",
                "Minus",
                "Pointer",
                "Thumbs up",
            ],
            Tool::CutOut => &[
                "Rectangle",
                "Horizontal",
                "Vertical",
                "Horizontal sawtooth",
                "Vertical sawtooth",
                "Horizontal wave",
                "Vertical wave",
            ],
            Tool::Fill => &["Area", "Global"],
            Tool::MagicWand => &["Area", "Global"],
            Tool::Selection => &["Rectangle", "Ellipse", "Freehand"],
            Tool::Eraser => &["Round", "Square"],
            Tool::Step => &[
                "Numbers",
                "Letters",
                "Lowercase",
                "Roman",
                "Numbers square",
                "Letters square",
            ],
            _ => &[],
        }
    }

    /// Tools that draw new annotation objects.
    fn creates_objects(self) -> bool {
        !matches!(
            self,
            Tool::Select
            | Tool::Move
                | Tool::Fill
                | Tool::Crop
                | Tool::CutOut
                | Tool::Selection
                | Tool::MagicWand
                | Tool::Eraser
                | Tool::Eyedropper
                | Tool::SmartMove
                | Tool::TextReplace
        )
    }

    fn uses_marquee(self) -> bool {
        matches!(
            self,
            Tool::Crop | Tool::CutOut | Tool::Selection | Tool::SmartMove
        )
    }

    /// Tools that lift part of the image and so use a background fill.
    pub fn lifts(self) -> bool {
        matches!(
            self,
            Tool::Select
                | Tool::Selection
                | Tool::CutOut
                | Tool::MagicWand
                | Tool::SmartMove
        )
    }
}

pub fn default_style(tool: Tool) -> Style {
    let base = Style::default();
    match tool {
        Tool::Arrow => Style {
            end_cap: Cap::Arrow,
            width: 8.0,
            ..base
        },
        Tool::Line => Style { width: 6.0, ..base },
        Tool::Shape => Style { width: 6.0, ..base },
        Tool::Text => Style {
            width: 0.0,
            text_color: Rgba::RED,
            font_size: 32.0,
            bold: true,
            ..base
        },
        Tool::Callout => Style {
            width: 3.0,
            stroke: Rgba::rgb(0x1E, 0x6F, 0xD9),
            fill: Rgba::rgb(0x1E, 0x6F, 0xD9),
            text_color: Rgba::WHITE,
            font_size: 24.0,
            bold: true,
            align: Align::Center,
            ..base
        },
        Tool::Step => Style {
            width: 0.0,
            fill: Rgba::RED,
            text_color: Rgba::WHITE,
            font_size: 22.0,
            ..base
        },
        Tool::Pen => Style { width: 5.0, ..base },
        Tool::Highlight => Style {
            fill: Rgba::YELLOW.with_alpha(180),
            stroke: Rgba::YELLOW.with_alpha(180),
            width: 24.0,
            multiply: true,
            ..base
        },
        Tool::Blur => Style {
            fill: Rgba::BLACK,
            width: 0.0,
            ..base
        },
        Tool::Magnify => Style {
            width: 4.0,
            stroke: Rgba::rgb(0x30, 0x30, 0x30),
            shadow: true,
            ..base
        },
        Tool::Spotlight => Style { width: 0.0, ..base },
        Tool::Fill => Style {
            fill: Rgba::YELLOW,
            ..base
        },
        Tool::Eraser => Style {
            width: 24.0,
            ..base
        },
        Tool::Select | Tool::Selection | Tool::MagicWand | Tool::SmartMove => Style {
            fill: Rgba::WHITE,
            ..base
        },
        _ => base,
    }
}

/// Editor events for the UI, OR-ed together until taken.
pub const EVENT_SELECTION: u32 = 1;
pub const EVENT_DOCUMENT: u32 = 2;
pub const EVENT_FOCUS_TEXT: u32 = 4;
pub const EVENT_RESIZED: u32 = 8;
pub const EVENT_TOOL: u32 = 16;
pub const EVENT_COLOR_PICKED: u32 = 32;
pub const EVENT_CANVAS_RESIZE_ERROR: u32 = 64;
pub const EVENT_CANVAS_RESIZED: u32 = 128;
pub const EVENT_REGION_OVERLAP: u32 = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

/// Pointer shapes the UI should show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum Cursor {
    Default = 0,
    Move = 1,
    ResizeNwse = 2,
    ResizeNesw = 3,
    ResizeNs = 4,
    ResizeEw = 5,
    Cross = 6,
    Point = 7,
    Text = 8,
}

#[derive(Clone, Debug)]
enum Drag {
    Create {
        id: u64,
        start: Pt,
        history: std::sync::Arc<CreationHistory>,
    },
    Move {
        id: u64,
        last: Pt,
        moved: bool,
    },
    /// Moving every selected object together.
    MoveMany {
        last: Pt,
        moved: bool,
    },
    /// Moving an image region for the first time.
    MoveRegion {
        start: Pt,
    },
    /// Resizing an image region without changing its pixels.
    RegionHandle {
        handle: Handle,
        original: Box2,
    },
    /// Shift-dragging a box to add the objects it touches to the selection.
    Select {
        start: Pt,
    },
    Handle {
        id: u64,
        handle: Handle,
        /// The object as the drag found it. Every move resizes this copy
        /// afresh, so fixed corners, proportions and fitted text cannot
        /// drift, and a box dragged through zero size recovers.
        original: std::sync::Arc<Object>,
    },
    Marquee {
        start: Pt,
    },
    /// A freehand selection outline.
    Lasso {
        points: Vec<Pt>,
    },
    Erase {
        last: Pt,
    },
    /// Dragging a canvas edge or corner.
    Canvas {
        handle: Handle,
        start: Pt,
    },
    /// Moving the image inside a selected drop zone.
    Pan {
        id: u64,
        last: Pt,
        moved: bool,
    },
}

/// What fills the hole an object leaves when it is lifted off the image.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackgroundFill {
    /// Rebuilt from the surroundings.
    #[default]
    Auto,
    Transparent,
    /// The tool's fill color.
    Color,
}

impl BackgroundFill {
    pub const NAMES: &'static [&'static str] = &["Auto-fill", "Transparent", "Color"];

    pub fn name(self) -> &'static str {
        Self::NAMES[self as usize]
    }

    pub fn from_name(n: &str) -> BackgroundFill {
        match n {
            "Transparent" => BackgroundFill::Transparent,
            "Color" => BackgroundFill::Color,
            _ => BackgroundFill::Auto,
        }
    }
}

/// Text recognition, which the document crate does not do itself (the
/// editor plugs in the platform OCR engine).
pub trait Analyzer: Send {
    /// Lines of text found in `area` of `image`.
    fn text_in(&mut self, image: &Pixmap, area: IRect) -> Vec<crate::smart::TextLine>;
}

#[derive(Clone, Debug)]
struct Snapshot {
    doc: Document,
    label: String,
    key: Option<String>,
}

#[derive(Clone, Debug)]
struct CreationHistory {
    redo: Vec<Snapshot>,
    evicted: Option<Snapshot>,
    modified: bool,
}

pub struct Editor {
    doc: Document,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    tool: Tool,
    styles: HashMap<Tool, Style>,
    variants: HashMap<Tool, String>,
    amounts: HashMap<Tool, f32>,
    selection: Option<u64>,
    region: Option<IRect>,
    /// A polygon being drawn corner by corner.
    building: Option<u64>,
    /// Further selected objects (Shift-click, Select all, box select);
    /// `selection` is the one the properties panel shows.
    others: Vec<u64>,
    drag: Option<Drag>,
    marquee: Option<Box2>,
    display: Pixmap,
    dirty: Option<IRect>,
    events: u32,
    modified: bool,
    picked: Option<Rgba>,
    canvas_resize_error: Option<CanvasResizeError>,
    pub analyzer: Option<Box<dyn Analyzer>>,
    pub background_fill: BackgroundFill,
    /// Color of canvas areas added by resizing or expanding the canvas.
    pub canvas_color: Rgba,
    /// Stamps the user added (name, image), offered after the built-in ones.
    pub custom_stamps: Vec<(String, std::sync::Arc<Pixmap>)>,
}

const HISTORY_LIMIT: usize = 100;
const MIN_DRAG: f32 = 4.0;
const MIN_CANVAS_SIZE: f32 = 8.0;

#[derive(Clone, Copy)]
struct CanvasBounds {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl CanvasBounds {
    fn to_box(self) -> Option<Box2> {
        let [x0, y0, x1, y1] =
            [self.x0, self.y0, self.x1, self.y1].map(|value| value.round() as f32);
        [x0, y0, x1, y1]
            .iter()
            .all(|value| value.is_finite())
            .then(|| Box2::new(x0, y0, x1, y1))
    }
}

fn valid_canvas_size(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= i32::MAX as u32
        && height <= i32::MAX as u32
        && u64::from(width) * u64::from(height) <= crate::io::MAX_DOCUMENT_PIXELS
}

fn canvas_drag_bounds(
    width: u32,
    height: u32,
    handle: Handle,
    start: Pt,
    point: Pt,
) -> Result<CanvasBounds, CanvasResizeError> {
    let (w, h) = (f64::from(width), f64::from(height));
    let (dx, dy) = (
        f64::from(point.x) - f64::from(start.x),
        f64::from(point.y) - f64::from(start.y),
    );
    if !dx.is_finite() || !dy.is_finite() || handle > H_L {
        return Err(CanvasResizeError::InvalidDimensions);
    }
    let min_size = f64::from(MIN_CANVAS_SIZE);
    let (min_w, min_h) = (w.min(min_size), h.min(min_size));
    let x0 = if matches!(handle, H_TL | H_L | H_BL) {
        dx.min(w - min_w)
    } else {
        0.0
    };
    let x1 = if matches!(handle, H_TR | H_R | H_BR) {
        (w + dx).max(min_w)
    } else {
        w
    };
    let y0 = if matches!(handle, H_TL | H_T | H_TR) {
        dy.min(h - min_h)
    } else {
        0.0
    };
    let y1 = if matches!(handle, H_BL | H_B | H_BR) {
        (h + dy).max(min_h)
    } else {
        h
    };
    if [x0, y0, x1, y1].iter().any(|value| !value.is_finite()) {
        return Err(CanvasResizeError::InvalidDimensions);
    }
    Ok(CanvasBounds { x0, y0, x1, y1 })
}

fn canvas_resize_geometry(bounds: CanvasBounds) -> Result<(u32, u32, i32, i32), CanvasResizeError> {
    let rounded = |value: f64| {
        let value = value.round();
        if value.is_finite() && (i32::MIN as f64..=i32::MAX as f64).contains(&value) {
            Ok(value as i32)
        } else {
            Err(CanvasResizeError::InvalidDimensions)
        }
    };
    let (x0, y0, x1, y1) = (
        rounded(bounds.x0)?,
        rounded(bounds.y0)?,
        rounded(bounds.x1)?,
        rounded(bounds.y1)?,
    );
    let width = u32::try_from(i64::from(x1) - i64::from(x0))
        .map_err(|_| CanvasResizeError::InvalidDimensions)?;
    let height = u32::try_from(i64::from(y1) - i64::from(y0))
        .map_err(|_| CanvasResizeError::InvalidDimensions)?;
    let dx = x0
        .checked_neg()
        .ok_or(CanvasResizeError::InvalidDimensions)?;
    let dy = y0
        .checked_neg()
        .ok_or(CanvasResizeError::InvalidDimensions)?;
    if !valid_canvas_size(width, height) {
        return Err(CanvasResizeError::InvalidDimensions);
    }
    Ok((width, height, dx, dy))
}

impl Editor {
    pub fn new(doc: Document) -> Editor {
        let display = Pixmap::new(doc.width(), doc.height()).expect("non-zero document");
        let mut e = Editor {
            doc,
            undo: Vec::new(),
            redo: Vec::new(),
            tool: Tool::Arrow,
            styles: HashMap::new(),
            variants: HashMap::new(),
            amounts: HashMap::new(),
            selection: None,
            region: None,
            building: None,
            others: Vec::new(),
            drag: None,
            marquee: None,
            display,
            dirty: None,
            events: 0,
            modified: false,
            picked: None,
            canvas_resize_error: None,
            analyzer: None,
            background_fill: BackgroundFill::Auto,
            custom_stamps: Vec::new(),
            canvas_color: Rgba::WHITE,
        };
        e.refresh_all();
        e
    }

    pub fn document(&self) -> &Document {
        &self.doc
    }

    pub fn display(&self) -> &Pixmap {
        &self.display
    }

    pub fn is_modified(&self) -> bool {
        self.modified
    }

    /// Marks a newly created document unsaved without adding an undo step.
    pub fn mark_unsaved(&mut self) {
        self.modified = true;
        self.events |= EVENT_DOCUMENT;
    }

    pub fn mark_saved(&mut self) {
        self.modified = false;
    }

    /// Replaces the document (open, new capture); clears history.
    pub fn load(&mut self, doc: Document) {
        self.doc = doc;
        self.undo.clear();
        self.redo.clear();
        self.selection = None;
        self.region = None;
        self.others.clear();
        self.drag = None;
        self.marquee = None;
        self.canvas_resize_error = None;
        self.modified = false;
        self.refresh_all();
        self.events |= EVENT_SELECTION | EVENT_DOCUMENT;
    }

    pub fn take_events(&mut self) -> u32 {
        std::mem::take(&mut self.events)
    }

    // ---- tools and styles ------------------------------------------------

    pub fn tool(&self) -> Tool {
        self.tool
    }

    pub fn set_tool(&mut self, tool: Tool) {
        self.finish_polygon();
        if self.tool != tool {
            if let Some(Drag::Create { id, history, .. }) = self.drag.take() {
                self.discard_creation(id, history);
            }
            self.seal();
            self.select(None);
            self.tool = tool;
            self.set_marquee(None);
            self.events |= EVENT_TOOL;
        }
    }

    pub fn tool_style(&self, tool: Tool) -> Style {
        self.styles
            .get(&tool)
            .cloned()
            .unwrap_or_else(|| default_style(tool))
    }

    pub fn set_tool_style(&mut self, tool: Tool, style: Style) {
        self.styles.insert(tool, style);
    }

    /// The variants offered for `tool`, including custom stamps.
    pub fn variants_for(&self, tool: Tool) -> Vec<String> {
        let mut v: Vec<String> = tool.variants().iter().map(|s| s.to_string()).collect();
        if tool == Tool::Stamp {
            v.extend(self.custom_stamps.iter().map(|(n, _)| n.clone()));
        }
        v
    }

    pub fn variant(&self, tool: Tool) -> String {
        self.variants
            .get(&tool)
            .cloned()
            .unwrap_or_else(|| tool.variants().first().unwrap_or(&"").to_string())
    }

    /// Sets the current tool's variant, and the selected object's when it
    /// was made by that tool.
    pub fn set_variant(&mut self, value: &str) {
        let tool = self.tool;
        self.variants.insert(tool, value.to_owned());
        if let Some(id) = self.selection {
            let v = value.to_owned();
            self.edit(id, "Change type", Some(format!("variant{id}")), move |o| {
                apply_variant(o, &v)
            });
        }
        let variant = self.variant(tool);
        if tool == Tool::Pen || tool == Tool::Highlight {
            let marker = variant == "Marker" || variant == "Freehand";
            let mut style = self.tool_style(tool);
            if tool == Tool::Pen {
                style.multiply = marker;
                if marker && style.width < 12.0 {
                    style.width = 20.0;
                    style.stroke = Rgba::YELLOW.with_alpha(180);
                }
            }
            self.styles.insert(tool, style);
        }
    }

    /// The blur intensity, magnifier zoom or spotlight dimming new objects
    /// of `tool` get.
    pub fn tool_amount(&self, tool: Tool) -> f32 {
        let default = match tool {
            Tool::Blur => {
                if self.variant(Tool::Blur) == "Pixelate" {
                    12.0
                } else {
                    10.0
                }
            }
            Tool::Magnify => 2.0,
            Tool::Spotlight => 0.6,
            Tool::MagicWand => 25.0,
            Tool::Fill => 10.0,
            Tool::Pen => 3.0,
            _ => 0.0,
        };
        self.amounts.get(&tool).copied().unwrap_or(default)
    }

    pub fn set_tool_amount(&mut self, tool: Tool, value: f32) {
        self.amounts.insert(tool, value);
    }

    /// Style shown in the properties panel: the selection's, or the tool's.
    pub fn current_style(&self) -> Style {
        self.selected()
            .map(|o| o.style.clone())
            .unwrap_or_else(|| self.tool_style(self.tool))
    }

    /// Edits the selection's style (undoably, coalescing edits of the same
    /// property) and the current tool's style for new objects.
    pub fn update_style(&mut self, property: &str, change: impl Fn(&mut Style)) {
        for id in self.others.clone() {
            self.edit(id, "Change style", Some(format!("{property}*")), |o| {
                o.walk_mut(&mut |o| change(&mut o.style));
                o.fit_text();
            });
        }
        if let Some(id) = self.selection {
            self.edit(id, "Change style", Some(format!("{property}{id}")), |o| {
                change(&mut o.style);
                o.fit_text();
            });
            if self.selected().map(|o| tool_for(o)) != Some(self.tool) {
                return;
            }
        }
        let mut style = self.tool_style(self.tool);
        change(&mut style);
        self.styles.insert(self.tool, style);
    }

    // ---- selection ---------------------------------------------------------

    pub fn selection(&self) -> Option<u64> {
        self.selection
    }

    /// A selected background region, before it is moved or otherwise edited.
    pub fn selected_region(&self) -> Option<IRect> {
        self.region
    }

    pub fn selected(&self) -> Option<&Object> {
        self.selection.and_then(|id| self.doc.get(id))
    }

    /// Every selected object, the panel's one first.
    pub fn selected_ids(&self) -> Vec<u64> {
        self.selection
            .into_iter()
            .chain(self.others.iter().copied())
            .collect()
    }

    /// Clones every selected annotation in document stacking order.
    pub fn selected_objects(&self) -> Vec<Object> {
        let selected: HashSet<_> = self.selected_ids().into_iter().collect();
        self.doc
            .objects
            .iter()
            .filter(|object| selected.contains(&object.id))
            .cloned()
            .collect()
    }

    /// Adds `id` to the selection, or takes it out (Shift-click).
    pub fn toggle_selected(&mut self, id: u64) {
        let mut ids = self.selected_ids();
        if let Some(i) = ids.iter().position(|x| *x == id) {
            ids.remove(i);
        } else {
            ids.insert(0, id);
        }
        self.set_selection(ids);
    }

    /// Replaces the selection; the first id is the panel's.
    pub fn set_selection(&mut self, ids: Vec<u64>) {
        let mut seen = HashSet::new();
        let mut ids: Vec<u64> = ids
            .into_iter()
            .filter(|id| self.doc.get(*id).is_some() && seen.insert(*id))
            .collect();
        let first = if ids.is_empty() {
            None
        } else {
            Some(ids.remove(0))
        };
        self.select(first);
        self.others = ids
            .into_iter()
            .filter(|id| self.doc.get(*id).is_some())
            .collect();
        self.events |= EVENT_SELECTION;
    }

    fn prune_selection(&mut self) {
        self.others.retain(|id| self.doc.get(*id).is_some());
        if self.selection.is_some_and(|id| self.doc.get(id).is_none()) {
            self.selection = None;
        }
        if self.selection.is_none() && !self.others.is_empty() {
            self.selection = Some(self.others.remove(0));
        }
    }

    /// Selects the whole image, whatever the active tool, so Copy takes the
    /// flattened image with its annotations. Invert Selection from an empty
    /// selection picks every annotation instead.
    pub fn select_all(&mut self) {
        self.finish_polygon();
        if let Some(Drag::Create { id, history, .. }) = self.drag.take() {
            self.discard_creation(id, history);
        }
        self.seal();
        self.set_marquee(None);
        self.select_region(self.doc.bounds());
    }

    /// Returns whether the active region intersects any editable annotation.
    pub fn region_overlaps_annotations(&self) -> bool {
        self.region
            .is_some_and(|region| self.intersects_annotation(region))
    }

    fn intersects_annotation(&self, region: IRect) -> bool {
        let region = region.to_box();
        self.doc
            .objects
            .iter()
            .any(|object| object.bounds().intersects(&region))
    }

    /// Renders selected annotations into a transparent image in document order.
    pub fn selected_objects_image(&self) -> Option<Pixmap> {
        let selected: HashSet<_> = self.selected_ids().into_iter().collect();
        if selected.is_empty() {
            return None;
        }
        let objects: Vec<_> = self
            .doc
            .objects
            .iter()
            .filter(|object| selected.contains(&object.id))
            .collect();
        let bounds = match objects.as_slice() {
            [object]
                if matches!(object.kind, Kind::Image { .. })
                    && object.rotation.abs() <= 0.01
                    && object.style.width <= 0.0
                    && !object.style.shadow
                    && object.style.outline_width <= 0.0
                    && object.zone.is_none() =>
            {
                object.rect()?
            }
            _ => objects
                .iter()
                .map(|object| object.bounds())
                .reduce(|bounds, object| bounds.union(&object))?,
        };
        let mut area = bounds.to_irect();
        let mut width = u32::try_from(area.w).ok()?;
        let mut height = u32::try_from(area.h).ok()?;
        if !valid_canvas_size(width, height) {
            area = area.intersect(&self.doc.bounds())?;
            width = u32::try_from(area.w).ok()?;
            height = u32::try_from(area.h).ok()?;
            if !valid_canvas_size(width, height) {
                return None;
            }
        }
        let mut pixels = Pixmap::new(width, height)?;
        for object in objects {
            object.draw(&mut pixels, (area.x, area.y));
        }
        Some(pixels)
    }

    pub fn invert_selection(&mut self) {
        let chosen = self.selected_ids();
        let ids = self
            .doc
            .objects
            .iter()
            .rev()
            .map(|o| o.id)
            .filter(|id| !chosen.contains(id))
            .collect();
        self.set_selection(ids);
    }

    /// Boxes of the selected objects other than the panel's one.
    pub fn other_boxes(&self) -> Vec<Box2> {
        self.others
            .iter()
            .filter_map(|id| self.doc.get(*id))
            .filter_map(|o| o.rect())
            .collect()
    }

    /// Joins the selected objects into one group (keeping their order).
    pub fn group_selected(&mut self) -> Option<u64> {
        let ids = self.selected_ids();
        if ids.len() < 2 {
            return None;
        }
        self.checkpoint("Group", None);
        let at = ids.iter().filter_map(|id| self.doc.index_of(*id)).max()?;
        let mut members: Vec<Object> = Vec::new();
        let mut kept = Vec::new();
        for (i, o) in std::mem::take(&mut self.doc.objects)
            .into_iter()
            .enumerate()
        {
            if ids.contains(&o.id) {
                members.push(o);
            } else {
                kept.push((i, o));
            }
        }
        let insert_at = kept.iter().filter(|(i, _)| *i < at).count();
        self.doc.objects = kept.into_iter().map(|(_, o)| o).collect();
        let id = self.doc.allocate_id();
        let group = Object::new(
            id,
            Kind::Group { objects: members },
            Style {
                width: 0.0,
                ..Style::default()
            },
        );
        let b = group.bounds();
        self.doc.objects.insert(insert_at, group);
        self.invalidate_box(b);
        self.others.clear();
        self.selection = Some(id);
        self.events |= EVENT_SELECTION;
        Some(id)
    }

    /// Splits selected groups back into their objects.
    pub fn ungroup_selected(&mut self) -> bool {
        let groups: Vec<u64> = self
            .selected_ids()
            .into_iter()
            .filter(|id| matches!(self.doc.get(*id).map(|o| &o.kind), Some(Kind::Group { .. })))
            .collect();
        if groups.is_empty() {
            return false;
        }
        self.checkpoint("Ungroup", None);
        let mut freed = Vec::new();
        for id in groups {
            let Some(i) = self.doc.index_of(id) else {
                continue;
            };
            let group = self.doc.objects.remove(i);
            self.invalidate_box(group.bounds());
            if let Kind::Group { objects } = group.kind {
                for (k, mut o) in objects.into_iter().enumerate() {
                    o.id = self.doc.allocate_id();
                    freed.push(o.id);
                    self.doc.objects.insert(i + k, o);
                }
            }
        }
        self.set_selection(freed);
        true
    }

    /// Draws the selected objects into the image and removes them.
    pub fn flatten_selected(&mut self) -> bool {
        let ids = self.selected_ids();
        if ids.is_empty() {
            return false;
        }
        self.edit_document("Flatten", |d| {
            let mut keep = Vec::new();
            let mut flat = Document::new(d.background().clone());
            for o in std::mem::take(&mut d.objects) {
                if ids.contains(&o.id) {
                    flat.objects.push(o);
                } else {
                    keep.push(o);
                }
            }
            let image = flat.flatten();
            d.set_background(image);
            d.objects = keep;
            true
        });
        self.selection = None;
        self.others.clear();
        true
    }

    /// Lines up the selected objects' edges or centers: `how` is one of
    /// left, center, right, top, middle, bottom.
    pub fn align_selected(&mut self, how: &str) {
        let ids = self.selected_ids();
        let boxes: Vec<(u64, Box2)> = ids
            .iter()
            .filter_map(|id| Some((*id, self.doc.get(*id)?.rect()?)))
            .collect();
        if boxes.len() < 2 {
            return;
        }
        let all = boxes
            .iter()
            .map(|(_, b)| *b)
            .reduce(|a, b| a.union(&b))
            .unwrap();
        self.checkpoint("Align", None);
        for (id, b) in boxes {
            let (dx, dy) = match how {
                "left" => (all.x0 - b.x0, 0.0),
                "right" => (all.x1 - b.x1, 0.0),
                "center" => (all.center().x - b.center().x, 0.0),
                "top" => (0.0, all.y0 - b.y0),
                "bottom" => (0.0, all.y1 - b.y1),
                _ => (0.0, all.center().y - b.center().y),
            };
            if let Some(o) = self.doc.get_mut(id) {
                let before = o.bounds();
                o.translate(dx, dy);
                let after = o.bounds();
                self.invalidate_box(before.union(&after));
            }
        }
    }

    /// Selects just `id` (or nothing).
    pub fn select(&mut self, id: Option<u64>) {
        if self.region.take().is_some() {
            self.events |= EVENT_SELECTION;
        }
        let id = id.filter(|id| self.doc.get(*id).is_some());
        if !self.others.is_empty() {
            self.others.clear();
            self.events |= EVENT_SELECTION;
        }
        if self.selection == id {
            return;
        }
        if let Some(prev) = self.selection {
            // Text boxes left empty disappear, as nothing would show them.
            if self
                .doc
                .get(prev)
                .and_then(|o| o.text())
                .is_some_and(|t| t.trim().is_empty())
                && !matches!(self.doc.get(prev).map(|o| &o.kind), Some(Kind::Step { .. }))
            {
                if let Some(o) = self.doc.remove(prev) {
                    self.invalidate_box(o.bounds());
                }
            }
        }
        self.selection = id;
        self.events |= EVENT_SELECTION;
    }

    fn select_region(&mut self, region: IRect) {
        self.select(None);
        self.region = region.intersect(&self.doc.bounds());
        self.events |= EVENT_SELECTION;
    }

    /// Handles of the selection, for the UI to draw.
    pub fn selection_handles(&self) -> Vec<(Handle, Pt)> {
        if let Some(region) = self.region {
            return box_handles(&region.to_box());
        }
        self.selected().map(|o| o.handles()).unwrap_or_default()
    }

    pub fn selection_box(&self) -> Option<Box2> {
        if let Some(region) = self.region {
            return Some(region.to_box());
        }
        let o = self.selected()?;
        match &o.kind {
            Kind::Spotlight { rect, .. } => Some(*rect),
            Kind::Callout { rect, .. } => Some(*rect),
            _ => o.rect(),
        }
    }

    pub fn marquee(&self) -> Option<Box2> {
        self.marquee
    }

    pub fn marquee_is_box_selection(&self) -> bool {
        matches!(self.drag, Some(Drag::Select { .. }))
    }

    fn set_marquee(&mut self, m: Option<Box2>) {
        if self.marquee != m {
            self.marquee = m;
            self.events |= EVENT_SELECTION;
        }
    }

    pub fn picked_color(&mut self) -> Option<Rgba> {
        self.picked.take()
    }

    pub fn take_canvas_resize_error(&mut self) -> Option<CanvasResizeError> {
        self.canvas_resize_error.take()
    }

    fn report_canvas_resize_error(&mut self, error: CanvasResizeError) {
        self.canvas_resize_error = Some(error);
        self.events |= EVENT_CANVAS_RESIZE_ERROR;
    }

    // ---- history -------------------------------------------------------

    /// Records the state before an edit. Consecutive edits with the same
    /// `key` share one undo step.
    pub fn checkpoint(&mut self, label: &str, key: Option<String>) {
        self.modified = true;
        self.events |= EVENT_DOCUMENT;
        if self.redo.is_empty() && key.is_some() && self.undo.last().is_some_and(|s| s.key == key) {
            return;
        }
        self.undo.push(Snapshot {
            doc: self.doc.clone(),
            label: label.to_owned(),
            key,
        });
        if self.undo.len() > HISTORY_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Ends coalescing, so the next edit starts a new undo step.
    pub fn seal(&mut self) {
        if let Some(s) = self.undo.last_mut() {
            s.key = None;
        }
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|s| s.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|s| s.label.as_str())
    }

    pub fn undo(&mut self) -> bool {
        let Some(s) = self.undo.pop() else {
            return false;
        };
        let current = std::mem::replace(&mut self.doc, s.doc);
        self.redo.push(Snapshot {
            doc: current,
            label: s.label,
            key: None,
        });
        self.after_history();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(s) = self.redo.pop() else {
            return false;
        };
        let current = std::mem::replace(&mut self.doc, s.doc);
        self.undo.push(Snapshot {
            doc: current,
            label: s.label,
            key: None,
        });
        self.after_history();
        true
    }

    fn after_history(&mut self) {
        self.drag = None;
        self.region = None;
        self.prune_selection();
        self.modified = true;
        self.refresh_all();
        self.events |= EVENT_SELECTION | EVENT_DOCUMENT;
    }

    // ---- damage ----------------------------------------------------------

    fn invalidate(&mut self, r: IRect) {
        self.dirty = Some(self.dirty.map_or(r, |d| d.union(&r)));
    }

    fn invalidate_box(&mut self, b: Box2) {
        self.invalidate(b.to_irect());
    }

    /// Re-renders everything, reallocating the display if the canvas size
    /// changed.
    pub fn refresh_all(&mut self) {
        if (self.display.width(), self.display.height()) != self.doc.size() {
            self.display =
                Pixmap::new(self.doc.width(), self.doc.height()).expect("non-zero document");
            self.events |= EVENT_RESIZED;
        }
        self.invalidate(self.doc.bounds());
    }

    /// Brings the display up to date; returns the area that changed.
    pub fn take_damage(&mut self) -> Option<IRect> {
        let area = self.dirty.take()?;
        self.doc.render_into(&mut self.display, area, true)
    }

    /// Runs `change` on object `id` with an undo step, invalidating its old
    /// and new extent.
    pub fn edit(
        &mut self,
        id: u64,
        label: &str,
        key: Option<String>,
        change: impl FnOnce(&mut Object),
    ) {
        let Some(before) = self.doc.get(id).map(|o| o.bounds()) else {
            return;
        };
        self.checkpoint(label, key);
        let o = self.doc.get_mut(id).unwrap();
        change(o);
        let after = o.bounds();
        self.invalidate_box(before.union(&after));
    }

    /// Changes the document as a whole (one undo step, full redraw).
    pub fn edit_document(
        &mut self,
        label: &str,
        change: impl FnOnce(&mut Document) -> bool,
    ) -> bool {
        self.edit_document_with_display(label, None, change)
    }

    fn edit_document_with_display(
        &mut self,
        label: &str,
        display: Option<Pixmap>,
        change: impl FnOnce(&mut Document) -> bool,
    ) -> bool {
        let saved = self.doc.clone();
        if !change(&mut self.doc) {
            self.doc = saved;
            return false;
        }
        self.undo.push(Snapshot {
            doc: saved,
            label: label.to_owned(),
            key: None,
        });
        if self.undo.len() > HISTORY_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.modified = true;
        self.region = None;
        self.prune_selection();
        self.events |= EVENT_DOCUMENT | EVENT_SELECTION;
        if let Some(display) = display {
            // The replacement display bypasses refresh_all's size-change detection.
            if (self.display.width(), self.display.height()) != self.doc.size() {
                self.events |= EVENT_RESIZED;
            }
            self.display = display;
        }
        self.refresh_all();
        true
    }

    fn resize_canvas_from_drag(
        &mut self,
        width: u32,
        height: u32,
        dx: i32,
        dy: i32,
        mut allocate: impl FnMut(u32, u32) -> Result<Pixmap, CanvasResizeError>,
    ) {
        if (width, height) == self.doc.size() && dx == 0 && dy == 0 {
            return;
        }
        let buffers = allocate(width, height)
            .and_then(|display| allocate(width, height).map(|background| (display, background)));
        match buffers {
            Ok((display, background)) => {
                let color = self.canvas_color;
                self.edit_document_with_display("Resize canvas", Some(display), |doc| {
                    doc.resize_canvas_preserving_pixels(background, dx, dy, color);
                    true
                });
                self.canvas_resize_error = None;
                self.events |= EVENT_CANVAS_RESIZED;
            }
            Err(error) => self.report_canvas_resize_error(error),
        }
    }

    // ---- object commands -------------------------------------------------

    pub fn add_object(&mut self, label: &str, kind: Kind, style: Style) -> u64 {
        self.checkpoint(label, None);
        let id = self.doc.add(kind, style);
        let b = self.doc.get(id).unwrap().bounds();
        self.invalidate_box(b);
        self.select(Some(id));
        id
    }

    pub fn delete_selected(&mut self) -> bool {
        if self.region.is_some() {
            let Some(id) = self.lift_selected_region() else {
                return false;
            };
            if let Some(o) = self.doc.remove(id) {
                self.invalidate_box(o.bounds());
            }
            self.select(None);
            return true;
        }
        let ids = self.selected_ids();
        if ids.is_empty() {
            return false;
        }
        self.checkpoint("Delete", None);
        for id in ids {
            if let Some(o) = self.doc.remove(id) {
                self.invalidate_box(o.bounds());
            }
        }
        self.selection = None;
        self.others.clear();
        self.events |= EVENT_SELECTION;
        true
    }

    fn relabel_duplicated_group_steps(&mut self, id: u64) {
        fn step_ids(object: &Object, ids: &mut Vec<u64>) {
            match &object.kind {
                Kind::Step { .. } => ids.push(object.id),
                Kind::Group { objects } => {
                    for child in objects {
                        step_ids(child, ids);
                    }
                }
                _ => {}
            }
        }

        let Some(root) = self.doc.get(id) else { return };
        let mut ids = Vec::new();
        step_ids(root, &mut ids);
        if ids.is_empty() {
            return;
        }

        let labels = self.doc.next_step_labels(ids.len());
        let by_id: HashMap<u64, String> = ids.into_iter().zip(labels).collect();
        if let Some(root) = self.doc.get_mut(id) {
            root.walk_mut(&mut |object| {
                if let Some(label) = by_id.get(&object.id) {
                    if let Kind::Step { label: current, .. } = &mut object.kind {
                        *current = label.clone();
                    }
                }
            });
        }
    }

    pub fn duplicate_selected(&mut self) -> Option<u64> {
        if let Some(region) = self.region {
            let image = raster::extract(self.doc.background(), region)?;
            let rect = region.to_box().offset(20.0, 20.0);
            return Some(self.add_object(
                "Duplicate",
                Kind::Image {
                    rect,
                    image: ImageRef::new(crate::object::fresh_key("selection"), image),
                },
                Style {
                    width: 0.0,
                    ..Style::default()
                },
            ));
        }
        if !self.others.is_empty() {
            let copies: Vec<Object> = self
                .selected_ids()
                .iter()
                .filter_map(|id| self.doc.get(*id).cloned())
                .collect();
            self.checkpoint("Duplicate", None);
            let mut ids = Vec::new();
            for mut c in copies {
                c.translate(20.0, 20.0);
                if let Kind::Step { label, .. } = &mut c.kind {
                    *label = self.doc.next_step_label();
                }
                let id = self.doc.insert(c);
                if matches!(self.doc.get(id).map(|object| &object.kind), Some(Kind::Group { .. })) {
                    self.relabel_duplicated_group_steps(id);
                }
                let b = self.doc.get(id).unwrap().bounds();
                self.invalidate_box(b);
                ids.push(id);
            }
            self.set_selection(ids.clone());
            return ids.first().copied();
        }
        let mut copy = self.selected()?.clone();
        copy.translate(20.0, 20.0);
        if let Kind::Step { label, .. } = &mut copy.kind {
            *label = self.doc.next_step_label();
        }
        self.checkpoint("Duplicate", None);
        let id = self.doc.insert(copy);
        if matches!(self.doc.get(id).map(|object| &object.kind), Some(Kind::Group { .. })) {
            self.relabel_duplicated_group_steps(id);
        }
        let b = self.doc.get(id).unwrap().bounds();
        self.invalidate_box(b);
        self.select(Some(id));
        Some(id)
    }

    /// Inserts a copied object (for paste), offset so it is visible.
    pub fn paste_object(&mut self, object: Object) -> u64 {
        self.paste_objects(vec![object])
            .into_iter()
            .next()
            .expect("one object was pasted")
    }

    /// Inserts copied annotations together in one undoable operation.
    pub fn paste_objects(&mut self, objects: Vec<Object>) -> Vec<u64> {
        if objects.is_empty() {
            return Vec::new();
        }
        self.checkpoint("Paste", None);
        let mut ids = Vec::with_capacity(objects.len());
        let mut bounds = None;
        for mut object in objects {
            object.translate(16.0, 16.0);
            if let Kind::Step { label, .. } = &mut object.kind {
                *label = self.doc.next_step_label();
            }
            let id = self.doc.insert(object);
            if matches!(self.doc.get(id).map(|object| &object.kind), Some(Kind::Group { .. })) {
                self.relabel_duplicated_group_steps(id);
            }
            let object_bounds = self
                .doc
                .get(id)
                .expect("pasted object was inserted")
                .bounds();
            bounds = Some(bounds.map_or(object_bounds, |current: Box2| current.union(&object_bounds)));
            ids.push(id);
        }
        if let Some(bounds) = bounds {
            self.invalidate_box(bounds);
        }
        self.set_selection(ids.clone());
        ids
    }

    /// Spotlight and magnify: the selected object's area (or the middle of
    /// the image) enlarged `zoom` times in place, framed, with the rest
    /// dimmed; the area object itself is removed. All of it stays
    /// editable.
    pub fn spotlight_magnify(&mut self, zoom: f32) {
        let (w, h) = (self.doc.width() as f32, self.doc.height() as f32);
        let picked = self
            .selected()
            .map(|o| (o.id, o.rect().unwrap_or_else(|| o.bounds())));
        let area = match picked {
            Some((_, r)) => r,
            None => Box2::around(Pt::new(w / 2.0, h / 2.0), w / 6.0, h / 6.0),
        };
        let area = Box2::new(
            area.x0.max(0.0),
            area.y0.max(0.0),
            area.x1.min(w),
            area.y1.min(h),
        );
        if area.is_degenerate(4.0) {
            return;
        }
        let picked_id = picked.map(|(id, _)| id);
        self.checkpoint("Spotlight & magnify", None);
        if let Some(id) = picked_id {
            self.doc.remove(id);
        }
        let flat = self.doc.flatten();
        let Some(piece) = raster::extract(&flat, area.to_irect()) else {
            return;
        };
        let c = area.center();
        let (bw, bh) = (
            (area.width() * zoom).min(w * 0.9),
            (area.height() * zoom).min(h * 0.9),
        );
        let big = Box2::around(
            Pt::new(
                c.x.clamp(bw / 2.0, w - bw / 2.0),
                c.y.clamp(bh / 2.0, h - bh / 2.0),
            ),
            bw / 2.0,
            bh / 2.0,
        );
        let dim = Style {
            width: 0.0,
            ..Style::default()
        };
        self.doc.add(
            Kind::Spotlight {
                rect: big,
                ellipse: false,
                dim: 0.55,
                blur: 4.0,
                rounded: false,
            },
            dim,
        );
        let frame = Style {
            width: 4.0,
            stroke: Rgba::WHITE,
            shadow: true,
            ..Style::default()
        };
        let id = self.doc.add(
            Kind::Image {
                rect: big,
                image: ImageRef::new(crate::object::fresh_key("magnified"), piece),
            },
            frame,
        );
        self.invalidate(IRect::of_size(self.doc.width(), self.doc.height()));
        self.seal();
        self.select(Some(id));
    }

    /// Adds a tail to the selected callout (pointing beside the first),
    /// or removes its last extra one.
    pub fn callout_tail(&mut self, add: bool) -> bool {
        let Some(id) = self.selection.filter(|id| {
            self.doc
                .get(*id)
                .is_some_and(|o| matches!(o.kind, Kind::Callout { .. }))
        }) else {
            return false;
        };
        let label = if add { "Add tail" } else { "Remove tail" };
        self.edit(id, label, None, move |o| {
            if let Kind::Callout {
                rect,
                tail,
                more_tails,
                ..
            } = &mut o.kind
            {
                if add && more_tails.len() < 6 {
                    let c = rect.center();
                    let n = more_tails.len() as f32 + 1.0;
                    // Fan out around the first tail.
                    let angle = 0.6 * n * if more_tails.len() % 2 == 0 { 1.0 } else { -1.0 };
                    let d = tail.sub(c);
                    let (sin, cos) = angle.sin_cos();
                    more_tails.push(Pt::new(
                        c.x + d.x * cos - d.y * sin,
                        c.y + d.x * sin + d.y * cos,
                    ));
                } else if !add {
                    more_tails.pop();
                }
            }
        });
        true
    }

    /// The drop zone at `p`, if any (topmost first).
    pub fn zone_at(&self, p: Pt) -> Option<u64> {
        self.doc
            .objects
            .iter()
            .rev()
            .find(|o| o.zone.is_some() && o.bounds().contains(p))
            .map(|o| o.id)
    }

    /// Puts `image` into drop zone `id`, centered.
    pub fn fill_zone(&mut self, id: u64, image: Pixmap) {
        let key = crate::object::fresh_key("zone");
        self.edit(id, "Fill drop zone", None, move |o| {
            if let Kind::Image { image: r, .. } = &mut o.kind {
                *r = ImageRef::new(key, image);
            }
            if let Some(z) = o.zone.as_mut() {
                z.pan = Pt::default();
            }
        });
        self.select(Some(id));
    }

    /// An image dropped at `p`: into the drop zone there, else as a new
    /// image object centered on `p`.
    pub fn drop_image(&mut self, p: Pt, image: Pixmap) -> u64 {
        if let Some(id) = self.zone_at(p) {
            self.fill_zone(id, image);
            return id;
        }
        let (dw, dh) = (self.doc.width() as f32, self.doc.height() as f32);
        let (w, h) = (image.width() as f32, image.height() as f32);
        let scale = (dw * 0.6 / w).min(dh * 0.6 / h).min(1.0);
        let rect = Box2::around(p, w * scale / 2.0, h * scale / 2.0);
        let style = Style {
            width: 0.0,
            ..Style::default()
        };
        self.add_object(
            "Drop image",
            Kind::Image {
                rect,
                image: ImageRef::new(crate::object::fresh_key("image"), image),
            },
            style,
        )
    }

    /// Exchanges the pictures of the two selected drop zones.
    pub fn swap_zones(&mut self) -> bool {
        let ids = self.selected_ids();
        let zones: Vec<u64> = ids
            .into_iter()
            .filter(|id| self.doc.get(*id).is_some_and(|o| o.zone.is_some()))
            .collect();
        let [a, b] = zones[..] else { return false };
        let take = |d: &Document, id| match &d.get(id).unwrap().kind {
            Kind::Image { image, .. } => image.clone(),
            _ => unreachable!(),
        };
        let (ia, ib) = (take(&self.doc, a), take(&self.doc, b));
        self.checkpoint("Swap images", None);
        for (id, img) in [(a, ib), (b, ia)] {
            let o = self.doc.get_mut(id).unwrap();
            if let Kind::Image { image, .. } = &mut o.kind {
                *image = img;
            }
            if let Some(z) = o.zone.as_mut() {
                z.pan = Pt::default();
            }
            let bounds = o.bounds();
            self.invalidate_box(bounds);
        }
        self.seal();
        true
    }

    /// Fill (crop to the zone) or fit (show all) for the selected zones.
    pub fn set_zone_fit(&mut self, fit: bool) {
        for id in self.selected_ids() {
            if self.doc.get(id).is_some_and(|o| o.zone.is_some()) {
                self.edit(id, "Fill or fit", Some(format!("fit{id}")), move |o| {
                    if let Some(z) = o.zone.as_mut() {
                        z.fit = fit;
                        z.pan = Pt::default();
                    }
                });
            }
        }
    }

    /// Takes the picture out of the selected drop zone.
    pub fn empty_zone(&mut self) {
        let Some(id) = self
            .selection
            .filter(|id| self.doc.get(*id).is_some_and(|o| o.zone.is_some()))
        else {
            return;
        };
        self.edit(id, "Empty drop zone", None, |o| {
            if let Kind::Image { image, .. } = &mut o.kind {
                *image = ImageRef {
                    key: String::new(),
                    pixmap: None,
                };
            }
        });
    }

    /// Inserts a raster as an image object, centered and scaled to fit.
    pub fn paste_image(&mut self, image: Pixmap) -> u64 {
        if let Some(id) = self
            .selection
            .filter(|id| self.doc.get(*id).is_some_and(|o| o.zone.is_some()))
        {
            self.fill_zone(id, image);
            return id;
        }
        let (dw, dh) = (self.doc.width() as f32, self.doc.height() as f32);
        let (w, h) = (image.width() as f32, image.height() as f32);
        let scale = (dw * 0.9 / w).min(dh * 0.9 / h).min(1.0);
        let rect = Box2::around(
            Pt::new(dw / 2.0, dh / 2.0),
            w * scale / 2.0,
            h * scale / 2.0,
        );
        let key = crate::object::fresh_key("image");
        let style = Style {
            width: 0.0,
            ..Style::default()
        };
        self.add_object(
            "Paste image",
            Kind::Image {
                rect,
                image: ImageRef::new(key, image),
            },
            style,
        )
    }

    pub fn nudge(&mut self, dx: f32, dy: f32) {
        if self.region.is_some() {
            if dx == 0.0 && dy == 0.0 {
                return;
            }
            if let Some(id) = self.lift_selected_region() {
                if let Some(o) = self.doc.get_mut(id) {
                    let before = o.bounds();
                    o.translate(dx, dy);
                    let after = o.bounds();
                    self.invalidate_box(before.union(&after));
                    self.events |= EVENT_SELECTION;
                }
            }
            return;
        }
        for id in self.selected_ids() {
            self.edit(id, "Move", Some("nudge".to_owned()), |o| {
                o.translate(dx, dy)
            });
        }
    }

    /// Moves the selection in z-order: +1/-1 one step, +2/-2 to the
    /// top/bottom.
    pub fn arrange(&mut self, how: i32) {
        let Some(id) = self.selection else { return };
        let Some(i) = self.doc.index_of(id) else {
            return;
        };
        let n = self.doc.objects.len();
        let j = match how {
            2 => n - 1,
            1 => (i + 1).min(n - 1),
            -1 => i.saturating_sub(1),
            _ => 0,
        };
        if i == j {
            return;
        }
        self.checkpoint("Arrange", None);
        let o = self.doc.objects.remove(i);
        let b = o.bounds();
        self.doc.objects.insert(j, o);
        self.invalidate_box(b);
    }

    pub fn set_selected_text(&mut self, text: &str) {
        let Some(id) = self.selection else { return };
        if self.selected().and_then(|o| o.text()) == Some(text) {
            return;
        }
        let owned = text.to_owned();
        self.edit(id, "Edit text", Some(format!("text{id}")), move |o| {
            o.set_text(&owned)
        });
    }

    /// Sets a numeric parameter of the selection (blur amount, zoom, dim).
    pub fn set_selected_amount(&mut self, value: f32) {
        let Some(id) = self.selection else { return };
        self.edit(
            id,
            "Change amount",
            Some(format!("amount{id}")),
            |o| match &mut o.kind {
                Kind::Blur { amount, .. } => *amount = value.clamp(1.0, 100.0),
                Kind::Magnify { zoom, .. } => *zoom = value.clamp(1.0, 8.0),
                Kind::Spotlight { dim, .. } => *dim = value.clamp(0.0, 1.0),
                _ => {}
            },
        );
    }

    pub fn set_selected_rotation(&mut self, degrees: f32) {
        let Some(id) = self.selection else { return };
        if !self.selected().is_some_and(|o| o.rotatable()) {
            return;
        }
        self.edit(id, "Rotate", Some(format!("rotate{id}")), |o| {
            o.rotation = degrees.rem_euclid(360.0)
        });
    }

    /// Font size edits also size a selected step badge.
    pub fn resize_selected_step(&mut self, font_size: f32) {
        let Some(id) = self.selection else { return };
        if matches!(self.selected().map(|o| &o.kind), Some(Kind::Step { .. })) {
            self.edit(id, "Change style", Some(format!("size{id}")), |o| {
                if let Kind::Step { radius, .. } = &mut o.kind {
                    *radius = font_size * 0.85;
                }
            });
        }
    }

    pub fn has_damage(&self) -> bool {
        self.dirty.is_some()
    }

    pub fn selected_amount(&self) -> Option<f32> {
        match self.selected()?.kind {
            Kind::Blur { amount, .. } => Some(amount),
            Kind::Magnify { zoom, .. } => Some(zoom),
            Kind::Spotlight { dim, .. } => Some(dim),
            _ => None,
        }
    }

    // ---- pointer -----------------------------------------------------------

    /// Handles at the canvas's edges and corners, shown when nothing is selected
    /// and the active tool is not selecting a region.
    pub fn canvas_handles(&self) -> Vec<(Handle, Pt)> {
        if self.selection.is_some()
            || self.region.is_some()
            || !matches!(
                self.tool,
                Tool::Move | Tool::Arrow | Tool::Shape | Tool::Line
            )
        {
            return Vec::new();
        }
        let (w, h) = (self.doc.width() as f32, self.doc.height() as f32);
        vec![
            (H_TL, Pt::new(0.0, 0.0)),
            (H_T, Pt::new(w / 2.0, 0.0)),
            (H_TR, Pt::new(w, 0.0)),
            (H_R, Pt::new(w, h / 2.0)),
            (H_BR, Pt::new(w, h)),
            (H_B, Pt::new(w / 2.0, h)),
            (H_BL, Pt::new(0.0, h)),
            (H_L, Pt::new(0.0, h / 2.0)),
        ]
    }

    fn canvas_handle_at(&self, p: Pt, tolerance: f32) -> Option<Handle> {
        let handle = self
            .canvas_handles()
            .into_iter()
            .filter_map(|(handle, point)| {
                let distance = point.dist(p);
                (distance <= tolerance * 1.5).then_some((handle, distance))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(handle, _)| handle)?;
        if self.tool == Tool::Move && self.pick_at(p, tolerance).is_some() {
            return None;
        }
        Some(handle)
    }

    /// Grows the canvas to include every object (in the canvas color).
    pub fn expand_to_fit(&mut self) -> bool {
        let Some(all) = self
            .doc
            .objects
            .iter()
            .map(|o| o.bounds())
            .reduce(|a, b| a.union(&b))
        else {
            return false;
        };
        let canvas = self.doc.bounds().to_box();
        let trim = |start: f32, end: f32| {
            if end - start > 4.0 {
                (start + 2.0, end - 2.0)
            } else {
                (start, end)
            }
        };
        let (x0, x1) = trim(all.x0, all.x1);
        let (y0, y1) = trim(all.y0, all.y1);
        let b = Box2::new(x0, y0, x1, y1).union(&canvas);
        if b == canvas {
            return false;
        }
        let r = b.to_irect();
        let (Some(dx), Some(dy)) = (r.x.checked_neg(), r.y.checked_neg()) else {
            return false;
        };
        if !valid_canvas_size(r.w as u32, r.h as u32) {
            return false;
        }
        let color = self.canvas_color;
        self.edit_document("Expand canvas", |d| {
            d.resize_canvas(r.w as u32, r.h as u32, dx, dy, color);
            true
        })
    }

    pub fn hover(&self, p: Pt, tolerance: f32) -> Cursor {
        if let Some(handle) = self.canvas_handle_at(p, tolerance) {
            return handle_cursor(handle);
        }
        if self.tool.uses_marquee()
            || matches!(
                self.tool,
                Tool::Fill | Tool::Eyedropper | Tool::TextReplace | Tool::MagicWand | Tool::Eraser
            )
        {
            return Cursor::Cross;
        }
        if let Some(region) = self.region {
            for (handle, point) in box_handles(&region.to_box()) {
                if point.dist(p) <= tolerance * 1.5 {
                    return handle_cursor(handle);
                }
            }
            if region.to_box().contains(p) {
                return Cursor::Move;
            }
        }
        if let Some(o) = self.selected().filter(|o| self.can_use_handles(o)) {
            for (h, q) in o.handles() {
                if q.dist(p) <= tolerance * 1.5 {
                    return handle_cursor(h);
                }
            }
        }
        if self.pick_at(p, tolerance).is_some() {
            return Cursor::Move;
        }
        match self.tool {
            Tool::Text => Cursor::Text,
            _ => Cursor::Cross,
        }
    }

    fn can_use_handles(&self, object: &Object) -> bool {
        if self.tool == Tool::Move {
            return true;
        }
        if !self.tool.creates_objects()
            || (self.selection != Some(object.id) && !self.others.contains(&object.id))
        {
            return false;
        }
        match (&object.kind, self.tool) {
            (Kind::Line { .. }, Tool::Arrow | Tool::Line)
            | (Kind::Pen { .. }, Tool::Pen | Tool::Highlight) => true,
            (Kind::Image { image, .. }, Tool::Stamp) => image.key.starts_with("stamp-"),
            _ => tool_for(object) == self.tool,
        }
    }

    fn can_pick(&self, object: &Object) -> bool {
        self.can_use_handles(object)
            && (self.tool == Tool::Move
                || (self.tool != Tool::Pen
                    && !(self.tool == Tool::Highlight
                        && self.variant(Tool::Highlight) == "Freehand")))
    }

    fn pick_at(&self, p: Pt, tolerance: f32) -> Option<u64> {
        self.doc
            .hit(p, tolerance)
            .filter(|id| self.doc.get(*id).is_some_and(|o| self.can_pick(o)))
    }

    pub fn pointer_down(&mut self, p: Pt, m: Modifiers, tolerance: f32) {
        self.pointer_down_impl(p, m, tolerance, true);
    }

    /// Starts a new annotation without picking up objects underneath it.
    /// Raster and selection tools keep their usual behavior.
    pub fn pointer_down_new(&mut self, p: Pt, m: Modifiers) {
        self.pointer_down_impl(p, m, 0.0, false);
    }

    fn pointer_down_impl(&mut self, p: Pt, m: Modifiers, tolerance: f32, pick_existing: bool) {
        self.drag = None;
        if pick_existing {
            if let Some(handle) = self.canvas_handle_at(p, tolerance) {
                self.drag = Some(Drag::Canvas { handle, start: p });
                return;
            }
        }
        if let Some(id) = self.building {
            let first = match self.doc.get(id).map(|o| &o.kind) {
                Some(Kind::Polygon { points }) => points.first().copied(),
                _ => None,
            };
            match first {
                Some(f) if f.dist(p) <= tolerance * 2.0 => self.finish_polygon(),
                Some(_) => self.edit(id, "Add Shape", Some(format!("poly{id}")), move |o| {
                    if let Kind::Polygon { points } = &mut o.kind {
                        if let Some(last) = points.last_mut() {
                            *last = p;
                        }
                        points.push(p);
                    }
                }),
                None => self.building = None,
            }
            return;
        }
        if self.tool == Tool::Selection && self.variant(Tool::Selection) == "Freehand" {
            self.select(None);
            self.drag = Some(Drag::Lasso { points: vec![p] });
            self.set_marquee(Some(Box2::from_points(p, p)));
            return;
        }
        if self.tool == Tool::MagicWand {
            self.select(None);
            return self.magic_wand(p);
        }
        if self.tool == Tool::Eraser {
            self.select(None);
            self.checkpoint("Erase", None);
            self.erase_segment(p, p);
            self.drag = Some(Drag::Erase { last: p });
            return;
        }
        if self.tool.uses_marquee() {
            self.select(None);
            self.drag = Some(Drag::Marquee { start: p });
            self.set_marquee(Some(Box2::from_points(p, p)));
            return;
        }
        match self.tool {
            Tool::Fill => return self.fill_at(p),
            Tool::Eyedropper => {
                self.picked = Some(raster::pixel(&self.doc.flatten(), p.x as i32, p.y as i32));
                self.events |= EVENT_COLOR_PICKED;
                return;
            }
            Tool::TextReplace => {
                self.select(None);
                self.drag = Some(Drag::Marquee { start: p });
                self.set_marquee(Some(Box2::from_points(p, p)));
                return;
            }
            _ => {}
        }
        if self.tool == Tool::Select && !m.shift && !m.alt {
            if let Some(region) = self.region {
                let rect = region.to_box();
                if let Some((handle, _)) = box_handles(&rect)
                    .into_iter()
                    .find(|(_, point)| point.dist(p) <= tolerance * 1.5)
                {
                    self.drag = Some(Drag::RegionHandle {
                        handle,
                        original: rect,
                    });
                    return;
                }
                if rect.contains(p) {
                    self.drag = Some(Drag::MoveRegion { start: p });
                    return;
                }
            }
        }
        if let Some(o) = self
            .selected()
            .filter(|o| pick_existing && self.tool != Tool::Select && self.can_use_handles(o))
        {
            let id = o.id;
            let original = std::sync::Arc::new(o.clone());
            if let Some((h, _)) = o
                .handles()
                .into_iter()
                .find(|(_, q)| q.dist(p) <= tolerance * 1.5)
            {
                self.checkpoint("Resize", None);
                self.drag = Some(Drag::Handle {
                    id,
                    handle: h,
                    original,
                });
                return;
            }
        }
        if self.tool != Tool::Select && (pick_existing || self.tool == Tool::Move) {
            if let Some(id) = self.pick_at(p, tolerance) {
                if m.shift {
                    self.toggle_selected(id);
                    return;
                }
                if self.others.contains(&id)
                    || (!self.others.is_empty() && self.selection == Some(id))
                {
                    self.drag = Some(Drag::MoveMany {
                        last: p,
                        moved: false,
                    });
                    return;
                }
                // A drag inside a selected, filled drop zone moves its picture.
                let pans = self.selection == Some(id) && self.doc.get(id).is_some_and(|o| {
                    o.zone.is_some()
                        && matches!(&o.kind, Kind::Image { image, .. } if image.pixmap.is_some())
                });
                self.select(Some(id));
                self.others.clear();
                self.drag = Some(if pans {
                    Drag::Pan {
                        id,
                        last: p,
                        moved: false,
                    }
                } else {
                    Drag::Move {
                        id,
                        last: p,
                        moved: false,
                    }
                });
                return;
            }
        }
        if self.tool == Tool::Select {
            self.set_selection(Vec::new());
            self.drag = Some(Drag::Marquee { start: p });
            self.set_marquee(Some(Box2::from_points(p, p)));
            return;
        }
        if self.tool == Tool::Move {
            if !m.shift {
                self.set_selection(Vec::new());
            }
            self.drag = Some(Drag::Select { start: p });
            self.set_marquee(Some(Box2::from_points(p, p)));
            return;
        }
        if !self.tool.creates_objects() {
            self.select(None);
            return;
        }
        self.others.clear();
        self.select(None);
        let (kind, style) = self.new_object(p);
        let label = format!("Add {}", self.tool.name());
        let step = matches!(kind, Kind::Step { .. }) || self.tool == Tool::Stamp;
        let polygon = matches!(kind, Kind::Polygon { .. });
        let history = (!step && !polygon).then(|| CreationHistory {
            redo: std::mem::take(&mut self.redo),
            evicted: (self.undo.len() >= HISTORY_LIMIT).then(|| self.undo.remove(0)),
            modified: self.modified,
        });
        let id = self.add_object(&label, kind, style);
        if polygon {
            self.building = Some(id);
            self.drag = None;
            return;
        }
        self.drag = Some(if step {
            Drag::Move {
                id,
                last: p,
                moved: true,
            }
        } else {
            Drag::Create {
                id,
                start: p,
                history: std::sync::Arc::new(history.expect("creation history")),
            }
        });
    }

    /// Closes the polygon being built (double-click, Enter, or a click on
    /// its first corner).
    pub fn finish_polygon(&mut self) {
        let Some(id) = self.building.take() else {
            return;
        };
        let too_small = match self.doc.get_mut(id).map(|o| &mut o.kind) {
            Some(Kind::Polygon { points }) => {
                // Drop the floating corner that follows the pointer.
                points.pop();
                points.dedup_by(|a, b| a.dist(*b) < 1.0);
                points.len() < 3
            }
            _ => true,
        };
        if too_small {
            if let Some(o) = self.doc.remove(id) {
                self.invalidate_box(o.bounds());
            }
            self.selection = None;
        } else if let Some(o) = self.doc.get(id) {
            let b = o.bounds();
            self.invalidate_box(b.inflate(4.0));
        }
        self.seal();
        self.events |= EVENT_SELECTION;
    }

    pub fn pointer_move(&mut self, p: Pt, m: Modifiers) {
        if let Some(id) = self.building {
            if let Some(o) = self.doc.get_mut(id) {
                let before = o.bounds();
                if let Kind::Polygon { points } = &mut o.kind {
                    let anchor = points[points.len().saturating_sub(2)];
                    if let Some(last) = points.last_mut() {
                        *last = if m.shift { snap45(anchor, p) } else { p };
                    }
                }
                let after = o.bounds();
                self.invalidate_box(before.union(&after));
            }
            return;
        }
        let Some(drag) = self.drag.clone() else {
            return;
        };
        match drag {
            Drag::Create { id, start, .. } => {
                let Some(o) = self.doc.get(id) else { return };
                let before = o.bounds();
                let pen = matches!(o.kind, Kind::Pen { .. });
                let o = self.doc.get_mut(id).unwrap();
                let damage = if let Kind::Pen { points } = &mut o.kind {
                    let last = *points.last().unwrap();
                    if last.dist(p) < 0.75 {
                        return;
                    }
                    let tail: Vec<Pt> = points.iter().rev().take(2).copied().chain([p]).collect();
                    points.push(p);
                    Box2::bounding(tail)
                        .unwrap()
                        .inflate(o.style.width / 2.0 + 3.0)
                } else {
                    shape_drag(o, start, p, m.shift);
                    before.union(&o.bounds())
                };
                let _ = pen;
                self.invalidate_box(damage);
            }
            Drag::Move { id, last, moved } => {
                if !moved && last.dist(p) < 1.0 {
                    return;
                }
                if !moved {
                    self.checkpoint("Move", None);
                }
                let (dx, dy) = (p.x - last.x, p.y - last.y);
                let Some(o) = self.doc.get_mut(id) else {
                    return;
                };
                let before = o.bounds();
                o.translate(dx, dy);
                let after = o.bounds();
                self.invalidate_box(before.union(&after));
                self.drag = Some(Drag::Move {
                    id,
                    last: p,
                    moved: true,
                });
            }
            Drag::MoveRegion { start } => {
                if start.dist(p) < 1.0 {
                    return;
                }
                if let Some(id) = self.lift_selected_region() {
                    self.drag = Some(Drag::Move {
                        id,
                        last: start,
                        moved: true,
                    });
                    self.pointer_move(p, m);
                }
            }
            Drag::RegionHandle { handle, original } => {
                let p = Pt::new(
                    p.x.clamp(0.0, self.doc.width() as f32),
                    p.y.clamp(0.0, self.doc.height() as f32),
                );
                let region = resize_box(original, handle, p, m.shift)
                    .to_irect()
                    .intersect(&self.doc.bounds());
                if let Some(region) = region {
                    if self.region != Some(region) {
                        self.region = Some(region);
                        self.events |= EVENT_SELECTION;
                    }
                }
            }
            Drag::Pan { id, last, moved } => {
                if !moved && last.dist(p) < 1.0 {
                    return;
                }
                if !moved {
                    self.checkpoint("Pan image", None);
                }
                let Some(o) = self.doc.get_mut(id) else {
                    return;
                };
                if let (Some(z), Kind::Image { rect, image }) = (o.zone.as_mut(), &o.kind) {
                    if let Some(src) = &image.pixmap {
                        z.pan = Pt::new(z.pan.x + p.x - last.x, z.pan.y + p.y - last.y);
                        // Keep the stored pan within what shows.
                        z.pan = z.placement(rect, src.width() as f32, src.height() as f32).1;
                    }
                }
                let b = o.bounds();
                self.invalidate_box(b);
                self.drag = Some(Drag::Pan {
                    id,
                    last: p,
                    moved: true,
                });
            }
            Drag::Handle {
                id,
                handle,
                original,
            } => {
                let Some(o) = self.doc.get_mut(id) else {
                    return;
                };
                let before = o.bounds();
                *o = (*original).clone();
                o.move_handle(handle, p, m.shift);
                let after = o.bounds();
                self.invalidate_box(before.union(&after));
                self.events |= EVENT_SELECTION;
            }
            Drag::Marquee { start } => {
                let b = self.marquee_box(start, p, m.shift);
                self.set_marquee(Some(b));
            }
            Drag::Select { start } => self.set_marquee(Some(Box2::from_points(start, p))),
            Drag::Lasso { mut points } => {
                if points.last().is_some_and(|q| q.dist(p) >= 2.0) {
                    points.push(p);
                }
                self.set_marquee(Box2::bounding(points.iter().copied()));
                self.drag = Some(Drag::Lasso { points });
            }
            Drag::Erase { last } => {
                self.erase_segment(last, p);
                self.drag = Some(Drag::Erase { last: p });
            }
            Drag::Canvas { handle, start } => {
                let bounds =
                    canvas_drag_bounds(self.doc.width(), self.doc.height(), handle, start, p);
                self.set_marquee(bounds.ok().and_then(CanvasBounds::to_box));
            }
            Drag::MoveMany { last, moved } => {
                if !moved && last.dist(p) < 1.0 {
                    return;
                }
                if !moved {
                    self.checkpoint("Move", None);
                }
                let (dx, dy) = (p.x - last.x, p.y - last.y);
                for id in self.selected_ids() {
                    if let Some(o) = self.doc.get_mut(id) {
                        let before = o.bounds();
                        o.translate(dx, dy);
                        let after = o.bounds();
                        self.invalidate_box(before.union(&after));
                    }
                }
                self.drag = Some(Drag::MoveMany {
                    last: p,
                    moved: true,
                });
                self.events |= EVENT_SELECTION;
            }
        }
    }

    pub fn pointer_up(&mut self, p: Pt, m: Modifiers) {
        self.pointer_move(p, m);
        let Some(drag) = self.drag.take() else { return };
        match drag {
            Drag::Create { id, start, history } => {
                let small = start.dist(p) < MIN_DRAG;
                let Some(o) = self.doc.get(id) else { return };
                let text = matches!(o.kind, Kind::Text { .. } | Kind::Callout { .. });
                if small && !matches!(o.kind, Kind::Pen { .. }) {
                    if text || matches!(o.kind, Kind::Magnify { .. }) {
                        let before = o.bounds();
                        let o = self.doc.get_mut(id).unwrap();
                        default_size(o, start);
                        let after = o.bounds();
                        self.invalidate_box(before.union(&after));
                    } else {
                        // A click with a drawing tool: nothing to keep.
                        self.discard_creation(id, history);
                        return;
                    }
                }
                if text {
                    self.events |= EVENT_FOCUS_TEXT;
                }
                let strength = self.tool_amount(Tool::Pen);
                if let Some(o) = self.doc.get_mut(id) {
                    let before = o.bounds();
                    if let Kind::Pen { points } = &mut o.kind {
                        if strength > 0.0 && points.len() > 3 {
                            *points = smooth_stroke(points, strength);
                            self.invalidate_box(before);
                        }
                    }
                }
                if self.tool == Tool::Magnify && self.variant(Tool::Magnify).ends_with("view") {
                    let v = self.variant(Tool::Magnify);
                    self.edit(id, "Add Magnify", Some(format!("view{id}")), move |o| {
                        apply_variant(o, &v)
                    });
                }
                self.events |= EVENT_SELECTION;
            }
            Drag::Lasso { points } => {
                self.set_marquee(None);
                if points.len() >= 3 {
                    self.lift_polygon(&points);
                }
            }
            Drag::Erase { .. } => {}
            Drag::Canvas { handle, start } => {
                self.set_marquee(None);
                let geometry =
                    canvas_drag_bounds(self.doc.width(), self.doc.height(), handle, start, p)
                        .and_then(canvas_resize_geometry);
                match geometry {
                    Ok((width, height, dx, dy)) => {
                        self.resize_canvas_from_drag(width, height, dx, dy, allocate_canvas_pixmap);
                    }
                    Err(error) => self.report_canvas_resize_error(error),
                }
            }
            Drag::Select { start } => {
                let b = Box2::from_points(start, p);
                self.set_marquee(None);
                let mut ids = if m.shift {
                    self.selected_ids()
                } else {
                    Vec::new()
                };
                ids.extend(
                    self.doc
                        .objects
                        .iter()
                        .rev()
                        .filter(|o| o.rect().is_some_and(|r| r.intersects(&b)))
                        .map(|o| o.id),
                );
                if !b.is_degenerate(MIN_DRAG) {
                    self.set_selection(ids);
                }
            }
            Drag::Marquee { start } => {
                let b = self.marquee_box(start, p, m.shift);
                self.set_marquee(None);
                if b.is_degenerate(MIN_DRAG) {
                    return;
                }
                let (x0, y0) = (b.x0.round() as i32, b.y0.round() as i32);
                let r = IRect::new(x0, y0, b.x1.round() as i32 - x0, b.y1.round() as i32 - y0);
                match self.tool {
                    Tool::Crop => {
                        self.edit_document("Crop", |d| d.crop(r));
                    }
                    Tool::CutOut if self.variant(Tool::CutOut) == "Rectangle" => {
                        self.lift_selection(r, false)
                    }
                    Tool::CutOut => {
                        let v = self.variant(Tool::CutOut);
                        let vertical = v.starts_with("Vertical");
                        let edge = if v.ends_with("sawtooth") {
                            crate::effects::Edge::Sawtooth
                        } else if v.ends_with("wave") {
                            crate::effects::Edge::Wave
                        } else {
                            crate::effects::Edge::Straight
                        };
                        self.edit_document("Cut out", |d| {
                            crate::effects::cut_out_with(d, r, vertical, edge)
                        });
                    }
                    Tool::Selection if self.variant(Tool::Selection) == "Ellipse" => {
                        self.lift_ellipse(r)
                    }
                    Tool::Select => self.select_region(r),
                    Tool::Selection => self.lift_selection(r, false),
                    Tool::SmartMove => self.lift_selection(r, true),
                    Tool::TextReplace => self.replace_text_in(r),
                    _ => {}
                }
            }
            _ => self.events |= EVENT_SELECTION,
        }
    }

    fn discard_creation(&mut self, id: u64, history: std::sync::Arc<CreationHistory>) {
        // A later edit owns the latest checkpoint; restoring it would discard that edit.
        if self.doc.get(id).is_none()
            || self.undo.last().is_none_or(|snapshot| snapshot.doc.get(id).is_some())
        {
            return;
        }
        self.selection = None;
        if let Some(o) = self.doc.remove(id) {
            self.invalidate_box(o.bounds());
        }
        if let Some(s) = self.undo.pop() {
            self.doc = s.doc;
        }
        let history =
            std::sync::Arc::try_unwrap(history).unwrap_or_else(|history| (*history).clone());
        if let Some(evicted) = history.evicted {
            self.undo.insert(0, evicted);
        }
        self.redo = history.redo;
        self.modified = history.modified;
        self.events |= EVENT_SELECTION;
    }

    fn marquee_box(&self, start: Pt, p: Pt, square: bool) -> Box2 {
        let (w, h) = (self.doc.width() as f32, self.doc.height() as f32);
        let p = Pt::new(p.x.clamp(0.0, w), p.y.clamp(0.0, h));
        let b = Box2::from_points(start, p);
        if self.tool == Tool::CutOut && self.variant(Tool::CutOut) != "Rectangle" {
            return if self.variant(Tool::CutOut).starts_with("Vertical") {
                Box2::new(b.x0, 0.0, b.x1, h)
            } else {
                Box2::new(0.0, b.y0, w, b.y1)
            };
        }
        if square {
            let s = b.width().max(b.height());
            return Box2::from_points(
                start,
                Pt::new(
                    start.x + s * (p.x - start.x).signum(),
                    start.y + s * (p.y - start.y).signum(),
                ),
            );
        }
        b
    }

    fn new_object(&mut self, p: Pt) -> (Kind, Style) {
        let tool = self.tool;
        let mut style = self.tool_style(tool);
        let variant = self.variant(tool);
        let at = Box2::from_points(p, p);
        let kind = match tool {
            Tool::Arrow | Tool::Line => Kind::Line {
                a: p,
                b: p,
                control: None,
            },
            Tool::Shape if variant == "Polygon" => Kind::Polygon { points: vec![p, p] },
            Tool::Shape => Kind::Shape {
                rect: at,
                shape: shape_of(&variant),
            },
            Tool::Text => Kind::Text {
                rect: at,
                text: String::new(),
            },
            Tool::Callout => Kind::Callout {
                rect: at,
                tail: p,
                text: String::new(),
                shape: shape_of(&variant),
                arrow: variant == "Arrow",
                more_tails: Vec::new(),
            },
            Tool::Step => {
                let format = variant.split(' ').next().unwrap_or("Numbers");
                let shape = if variant.ends_with("square") {
                    ShapeKind::Rounded
                } else {
                    ShapeKind::Ellipse
                };
                Kind::Step {
                    center: p,
                    radius: style.font_size * 0.85,
                    label: self.doc.next_step_label_as(format),
                    shape,
                }
            }
            Tool::Pen => Kind::Pen { points: vec![p] },
            Tool::Highlight if variant == "Freehand" => {
                style.stroke = style.fill;
                Kind::Pen { points: vec![p] }
            }
            Tool::Highlight => Kind::Highlight {
                rect: at,
                shape: shape_of(&variant),
            },
            Tool::Blur => Kind::Blur {
                rect: at,
                mode: match variant.as_str() {
                    "Pixelate" => BlurMode::Pixelate,
                    "Redact" => BlurMode::Redact,
                    _ => BlurMode::Blur,
                },
                amount: self.tool_amount(Tool::Blur),
            },
            Tool::Magnify => Kind::Magnify {
                rect: at,
                zoom: self.tool_amount(Tool::Magnify),
                ellipse: !variant.starts_with("Rectangle"),
                offset: Pt::default(),
            },
            Tool::Spotlight => Kind::Spotlight {
                rect: at,
                ellipse: variant == "Ellipse",
                dim: self.tool_amount(Tool::Spotlight),
                blur: 0.0,
                rounded: variant == "Rounded",
            },
            Tool::Stamp if self.custom_stamps.iter().any(|(n, _)| *n == variant) => {
                let (name, image) = self
                    .custom_stamps
                    .iter()
                    .find(|(n, _)| *n == variant)
                    .unwrap();
                let scale = (96.0 / image.width().max(image.height()) as f32).min(1.0);
                let (w, h) = (image.width() as f32 * scale, image.height() as f32 * scale);
                style.width = 0.0;
                Kind::Image {
                    rect: Box2::around(p, w / 2.0, h / 2.0),
                    image: ImageRef {
                        key: format!("stamp-{name}"),
                        pixmap: Some(image.clone()),
                    },
                }
            }
            Tool::Stamp => {
                let size = 72.0;
                Kind::Stamp {
                    rect: Box2::around(p, size / 2.0, size / 2.0),
                    stamp: Stamp::from_name(&variant).unwrap_or_default(),
                }
            }
            _ => Kind::Shape {
                rect: at,
                shape: ShapeKind::Rectangle,
            },
        };
        (kind, style)
    }

    fn fill_at(&mut self, p: Pt) {
        let color = self.tool_style(Tool::Fill).fill;
        let tolerance = (self.tool_amount(Tool::Fill).clamp(0.0, 100.0) * 2.55) as u8;
        let global = self.variant(Tool::Fill) == "Global";
        self.edit_document("Fill", |d| {
            if global {
                raster::replace_color(d.background_mut(), p.x as i32, p.y as i32, color, tolerance)
            } else {
                raster::flood_fill(d.background_mut(), p.x as i32, p.y as i32, color, tolerance)
                    .is_some()
            }
        });
    }

    fn lift_selected_region(&mut self) -> Option<u64> {
        if self.region_overlaps_annotations() {
            self.events |= EVENT_REGION_OVERLAP;
            return None;
        }
        let region = self.region.take()?;
        self.lift_selection(region, false);
        if self.selection.is_none() {
            self.region = Some(region);
            self.events |= EVENT_SELECTION;
        }
        self.selection
    }

    /// Lifts the pixels in `r` off the background into a movable image.
    /// Smart Move cuts out just the object and fills the hole from its
    /// surroundings; plain Selection lifts the rectangle and leaves the
    /// canvas color behind.
    fn lift_selection(&mut self, r: IRect, smart: bool) {
        let Some(r) = r.intersect(&self.doc.bounds()) else {
            return;
        };
        if self.intersects_annotation(r) {
            self.events |= EVENT_REGION_OVERLAP;
            return;
        }
        let lifted = if smart {
            crate::smart::lift_mask(self.doc.background(), r)
        } else {
            None
        };
        let (area, mask) = lifted.unwrap_or_else(|| (r, vec![true; (r.w * r.h) as usize]));
        self.lift_mask(
            area,
            &mask,
            if smart {
                "Smart Move"
            } else {
                "Move selection"
            },
        );
    }

    fn lift_ellipse(&mut self, r: IRect) {
        let Some(r) = r.intersect(&self.doc.bounds()) else {
            return;
        };
        if self.intersects_annotation(r) {
            self.events |= EVENT_REGION_OVERLAP;
            return;
        }
        let (cx, cy, rx, ry) = (
            r.w as f32 / 2.0,
            r.h as f32 / 2.0,
            r.w as f32 / 2.0,
            r.h as f32 / 2.0,
        );
        let mask = (0..r.h)
            .flat_map(|y| {
                (0..r.w).map(move |x| {
                    ((x as f32 + 0.5 - cx) / rx).powi(2) + ((y as f32 + 0.5 - cy) / ry).powi(2)
                        <= 1.0
                })
            })
            .collect::<Vec<_>>();
        self.lift_mask(r, &mask, "Move selection");
    }

    fn lift_polygon(&mut self, points: &[Pt]) {
        let Some(b) = Box2::bounding(points.iter().copied()) else {
            return;
        };
        let Some(r) = b.to_irect().intersect(&self.doc.bounds()) else {
            return;
        };
        // Even-odd point-in-polygon per pixel center.
        let inside = |x: f32, y: f32| {
            let mut c = false;
            let n = points.len();
            for i in 0..n {
                let (a, b) = (points[i], points[(i + n - 1) % n]);
                if (a.y > y) != (b.y > y) && x < (b.x - a.x) * (y - a.y) / (b.y - a.y) + a.x {
                    c = !c;
                }
            }
            c
        };
        let mask: Vec<bool> = (0..r.h)
            .flat_map(|y| (0..r.w).map(move |x| (x, y)))
            .map(|(x, y)| inside((r.x + x) as f32 + 0.5, (r.y + y) as f32 + 0.5))
            .collect();
        self.lift_mask(r, &mask, "Move selection");
    }

    /// Selects the area of similar color around `p` and lifts it.
    fn magic_wand(&mut self, p: Pt) {
        let tolerance = (self.tool_amount(Tool::MagicWand).clamp(0.0, 100.0) * 2.55) as u8;
        let found = if self.variant(Tool::MagicWand) == "Global" {
            raster::color_mask(self.doc.background(), p.x as i32, p.y as i32, tolerance)
        } else {
            raster::flood_mask(self.doc.background(), p.x as i32, p.y as i32, tolerance)
        };
        let Some((area, mask)) = found else { return };
        self.lift_mask(area, &mask, "Magic Wand");
    }

    /// Clears the image along a stroke from `a` to `b`.
    fn erase_segment(&mut self, a: Pt, b: Pt) {
        let style = self.tool_style(Tool::Eraser);
        let square = self.variant(Tool::Eraser) == "Square";
        let w = style.width.max(1.0);
        let mut pb = tiny_skia::PathBuilder::new();
        pb.move_to(a.x, a.y);
        pb.line_to(b.x + 0.01, b.y);
        let Some(path) = pb.finish() else { return };
        let mut paint = tiny_skia::Paint::default();
        paint.anti_alias = true;
        paint.blend_mode = tiny_skia::BlendMode::Clear;
        let cap = if square {
            tiny_skia::LineCap::Square
        } else {
            tiny_skia::LineCap::Round
        };
        let stroke = tiny_skia::Stroke {
            width: w,
            line_cap: cap,
            ..Default::default()
        };
        self.doc.background_mut().stroke_path(
            &path,
            &paint,
            &stroke,
            tiny_skia::Transform::identity(),
            None,
        );
        self.invalidate_box(Box2::from_points(a, b).inflate(w));
    }

    /// Lifts the masked pixels of `area` into an image object and fills the
    /// hole as `background_fill` says.
    fn lift_mask(&mut self, area: IRect, mask: &[bool], label: &str) {
        let bg = self.doc.background();
        let (aw, ah) = (area.w as usize, area.h as usize);
        let (mut x0, mut y0, mut x1, mut y1) = (aw, ah, 0, 0);
        for y in 0..ah {
            for x in 0..aw {
                if mask[y * aw + x] {
                    (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                }
            }
        }
        if x1 < x0 {
            return;
        }
        let at = IRect::new(
            area.x + x0 as i32,
            area.y + y0 as i32,
            (x1 - x0 + 1) as i32,
            (y1 - y0 + 1) as i32,
        );
        let Some(mut piece) = tiny_skia::Pixmap::new(at.w as u32, at.h as u32) else {
            return;
        };
        for y in 0..at.h {
            for x in 0..at.w {
                if mask[(y as usize + y0) * aw + x as usize + x0] {
                    raster::set_pixel(&mut piece, x, y, raster::pixel(bg, at.x + x, at.y + y));
                }
            }
        }
        let mut filled = bg.clone();
        match self.background_fill {
            BackgroundFill::Auto => {
                // Pad the mask with a ring of known pixels to fill from.
                let padded = area
                    .inflate(2)
                    .intersect(&self.doc.bounds())
                    .unwrap_or(area);
                let (pw, ph) = (padded.w as usize, padded.h as usize);
                let mut ring = vec![false; pw * ph];
                for y in 0..ah {
                    for x in 0..aw {
                        let (px, py) = (area.x - padded.x + x as i32, area.y - padded.y + y as i32);
                        if px >= 0 && py >= 0 && (px as usize) < pw && (py as usize) < ph {
                            ring[py as usize * pw + px as usize] = mask[y * aw + x];
                        }
                    }
                }
                crate::smart::fill_mask(&mut filled, padded, &ring)
            }
            BackgroundFill::Transparent | BackgroundFill::Color => {
                let color = if self.background_fill == BackgroundFill::Color {
                    self.tool_style(self.tool).fill
                } else {
                    Rgba::TRANSPARENT
                };
                for y in 0..ah {
                    for x in 0..aw {
                        if mask[y * aw + x] {
                            raster::set_pixel(
                                &mut filled,
                                area.x + x as i32,
                                area.y + y as i32,
                                color,
                            );
                        }
                    }
                }
            }
        }
        let rect = at.to_box();
        let key = format!(
            "lift-{}-{}",
            self.doc.objects.len() + 1,
            at.x ^ (at.y << 12)
        );
        let mut new_id = 0;
        let below_annotations = self.tool == Tool::Select;
        self.edit_document(label, |d| {
            d.set_background(filled);
            new_id = d.add(
                Kind::Image {
                    rect,
                    image: ImageRef::new(key, piece),
                },
                Style {
                    width: 0.0,
                    ..Style::default()
                },
            );
            if below_annotations {
                d.objects.rotate_right(1);
            }
            true
        });
        self.set_tool(Tool::Select);
        self.select(Some(new_id));
    }

    fn replace_text_in(&mut self, r: IRect) {
        let bg = self.doc.background_arc();
        let found = match self.analyzer.as_mut() {
            Some(a) => a.text_in(&bg, r),
            None => return,
        };
        if found.is_empty() {
            return;
        }
        let mut first = None;
        self.edit_document("Text Replace", |d| {
            for line in found {
                first.get_or_insert(crate::smart::replace_line(d, &line));
            }
            true
        });
        self.selection = first;
        self.events |= EVENT_SELECTION | EVENT_FOCUS_TEXT;
    }
}

fn handle_cursor(h: Handle) -> Cursor {
    match h {
        H_TL | H_BR => Cursor::ResizeNwse,
        H_TR | H_BL => Cursor::ResizeNesw,
        H_T | H_B => Cursor::ResizeNs,
        H_L | H_R => Cursor::ResizeEw,
        _ => Cursor::Point,
    }
}

fn shape_of(variant: &str) -> ShapeKind {
    match variant {
        "Rounded" => ShapeKind::Rounded,
        "Ellipse" | "Circle" => ShapeKind::Ellipse,
        "Triangle" => ShapeKind::Triangle,
        _ => ShapeKind::Rectangle,
    }
}

/// The tool that makes objects like `o`.
pub fn tool_for(o: &Object) -> Tool {
    match &o.kind {
        Kind::Line { .. } if o.style.end_cap == Cap::Arrow || o.style.start_cap == Cap::Arrow => {
            Tool::Arrow
        }
        Kind::Line { .. } => Tool::Line,
        Kind::Shape { .. } => Tool::Shape,
        Kind::Text { .. } => Tool::Text,
        Kind::Callout { .. } => Tool::Callout,
        Kind::Step { .. } => Tool::Step,
        Kind::Pen { .. } => Tool::Pen,
        Kind::Highlight { .. } => Tool::Highlight,
        Kind::Blur { .. } => Tool::Blur,
        Kind::Magnify { .. } => Tool::Magnify,
        Kind::Spotlight { .. } => Tool::Spotlight,
        Kind::Stamp { .. } => Tool::Stamp,
        Kind::Image { .. } | Kind::Group { .. } => Tool::Select,
        Kind::Polygon { .. } => Tool::Shape,
    }
}

/// The variant name describing `o`, if its kind has variants.
pub fn variant_of(o: &Object) -> Option<String> {
    let name = match &o.kind {
        Kind::Callout { arrow: true, .. } => "Arrow",
        Kind::Shape { shape, .. } | Kind::Callout { shape, .. } => match shape {
            ShapeKind::Rectangle => "Rectangle",
            ShapeKind::Rounded => "Rounded",
            ShapeKind::Ellipse => "Ellipse",
            ShapeKind::Triangle => "Triangle",
        },
        Kind::Blur { mode, .. } => match mode {
            BlurMode::Blur => "Blur",
            BlurMode::Pixelate => "Pixelate",
            BlurMode::Redact => "Redact",
        },
        Kind::Magnify {
            ellipse, offset, ..
        } => match (*ellipse, offset.len() > 0.5) {
            (true, false) => "Circle",
            (false, false) => "Rectangle",
            (true, true) => "Circle with view",
            (false, true) => "Rectangle with view",
        },
        Kind::Spotlight {
            ellipse, rounded, ..
        } => {
            if *ellipse {
                "Ellipse"
            } else if *rounded {
                "Rounded"
            } else {
                "Rectangle"
            }
        }
        Kind::Highlight { shape, .. } => match shape {
            ShapeKind::Rounded => "Rounded",
            ShapeKind::Ellipse => "Ellipse",
            _ => "Rectangle",
        },
        Kind::Polygon { .. } => "Polygon",
        Kind::Stamp { stamp, .. } => stamp.name(),
        Kind::Pen { .. } => {
            if o.style.multiply {
                "Marker"
            } else {
                "Pen"
            }
        }
        _ => return None,
    };
    Some(name.to_owned())
}

fn apply_variant(o: &mut Object, v: &str) {
    match &mut o.kind {
        Kind::Callout { shape, arrow, .. } => {
            *arrow = v == "Arrow";
            *shape = shape_of(v);
        }
        Kind::Shape { shape, .. } => *shape = shape_of(v),
        Kind::Blur { mode, .. } => {
            *mode = match v {
                "Pixelate" => BlurMode::Pixelate,
                "Redact" => BlurMode::Redact,
                _ => BlurMode::Blur,
            }
        }
        Kind::Magnify {
            rect,
            ellipse,
            offset,
            ..
        } => {
            *ellipse = !v.starts_with("Rectangle");
            if v.ends_with("view") && offset.len() < 0.5 {
                *offset = Pt::new(rect.width() * 1.3, -rect.height() * 0.4);
            } else if !v.ends_with("view") {
                *offset = Pt::default();
            }
        }
        Kind::Spotlight {
            ellipse, rounded, ..
        } => {
            *ellipse = v == "Ellipse";
            *rounded = v == "Rounded";
        }
        Kind::Highlight { shape, .. } => *shape = shape_of(v),
        Kind::Step { shape, .. } => {
            *shape = if v.ends_with("square") {
                ShapeKind::Rounded
            } else {
                ShapeKind::Ellipse
            }
        }
        Kind::Stamp { stamp, .. } => *stamp = Stamp::from_name(v).unwrap_or(*stamp),
        Kind::Pen { .. } => o.style.multiply = v == "Marker",
        _ => {}
    }
}

/// Updates an object being drawn from `start` to `p`.
fn shape_drag(o: &mut Object, start: Pt, p: Pt, shift: bool) {
    let b = if shift {
        let s = (p.x - start.x).abs().max((p.y - start.y).abs());
        Box2::from_points(
            start,
            Pt::new(
                start.x + s * (p.x - start.x).signum(),
                start.y + s * (p.y - start.y).signum(),
            ),
        )
    } else {
        Box2::from_points(start, p)
    };
    match &mut o.kind {
        Kind::Line { b: end, .. } => *end = if shift { snap45(start, p) } else { p },
        Kind::Callout { rect, tail, .. } => {
            *rect = b;
            *tail = Pt::new(b.x0 + b.width() * 0.25, b.y1 + b.height().max(30.0) * 0.6);
        }
        Kind::Stamp { rect, .. } => {
            let s = (p.x - start.x).abs().max((p.y - start.y).abs()).max(16.0);
            *rect = Box2::around(start, s, s);
        }
        _ => {
            if let Some(r) = rect_mut_any(o) {
                *r = b;
            }
            o.fit_text();
        }
    }
}

fn rect_mut_any(o: &mut Object) -> Option<&mut Box2> {
    match &mut o.kind {
        Kind::Shape { rect, .. }
        | Kind::Text { rect, .. }
        | Kind::Highlight { rect, .. }
        | Kind::Blur { rect, .. }
        | Kind::Magnify { rect, .. }
        | Kind::Spotlight { rect, .. }
        | Kind::Image { rect, .. } => Some(rect),
        _ => None,
    }
}

fn default_size(o: &mut Object, at: Pt) {
    let fs = o.style.font_size;
    match &mut o.kind {
        Kind::Text { rect, .. } => {
            *rect = Box2::from_xywh(at.x, at.y, (fs * 10.0).max(160.0), fs * 1.8)
        }
        Kind::Callout { rect, tail, .. } => {
            *rect = Box2::from_xywh(at.x, at.y, (fs * 9.0).max(180.0), fs * 3.4);
            *tail = Pt::new(at.x + rect.width() * 0.25, rect.y1 + fs * 2.0);
        }
        Kind::Magnify { rect, .. } => *rect = Box2::around(at, 90.0, 90.0),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> Editor {
        let mut e = Editor::new(Document::blank(400, 300, Rgba::WHITE));
        e.take_damage();
        e
    }

    const NONE: Modifiers = Modifiers {
        shift: false,
        ctrl: false,
        alt: false,
    };

    #[test]
    fn marking_a_new_document_unsaved_does_not_change_history() {
        let mut e = editor();
        let (kind, style) = square(10.0, Rgba::RED);
        e.add_object("Add", kind, style);
        assert!(e.undo());
        e.mark_saved();
        e.take_events();
        assert_eq!(e.undo_label(), None);
        assert_eq!(e.redo_label(), Some("Add"));

        e.mark_unsaved();
        assert!(e.is_modified());
        assert_eq!(e.undo_label(), None);
        assert_eq!(e.redo_label(), Some("Add"));
        assert_ne!(e.take_events() & EVENT_DOCUMENT, 0);

        e.mark_saved();
        assert!(!e.is_modified());
    }

    #[test]
    fn select_all_selects_the_whole_image_without_changing_pixels() {
        let mut pixels = raster::solid(12, 8, Rgba::rgb(40, 80, 120)).unwrap();
        raster::set_pixel(&mut pixels, 3, 2, Rgba::new(200, 100, 50, 128));
        raster::set_pixel(&mut pixels, 4, 2, Rgba::TRANSPARENT);
        let mut e = Editor::new(Document::new(pixels.clone()));
        let tool = e.tool();
        e.select_all();
        assert_eq!(e.selection_box(), Some(Box2::new(0.0, 0.0, 12.0, 8.0)));
        assert_eq!(e.selected_region(), Some(IRect::new(0, 0, 12, 8)));
        assert!(e.selected_ids().is_empty());
        assert_eq!(e.document().flatten().data(), pixels.data());
        assert_eq!(e.tool(), tool, "Select All keeps the active tool");
        e.select_all();
        assert_eq!(e.selected_region(), Some(IRect::new(0, 0, 12, 8)));
        assert!(e.document().objects.is_empty());
        assert!(!e.is_modified());
        assert_eq!(e.undo_label(), None);
    }

    #[test]
    fn select_all_selects_the_whole_image_over_annotations() {
        let mut e = editor();
        let a = e.add_object(
            "First",
            Kind::Shape {
                rect: Box2::new(20.0, 20.0, 60.0, 60.0),
                shape: ShapeKind::Rectangle,
            },
            Style::default(),
        );
        let b = e.add_object(
            "Second",
            Kind::Shape {
                rect: Box2::new(80.0, 20.0, 120.0, 60.0),
                shape: ShapeKind::Ellipse,
            },
            Style::default(),
        );
        e.select(Some(a));
        let before = e.document().flatten();
        e.select_all();
        assert_eq!(e.selected_region(), Some(IRect::new(0, 0, 400, 300)));
        assert!(e.selected_ids().is_empty());
        assert_eq!(
            e.document().objects.len(),
            2,
            "do not lift the background with annotations"
        );
        assert_eq!(e.document().flatten().data(), before.data());

        e.invert_selection();
        assert_eq!(e.selected_region(), None);
        assert_eq!(
            e.selected_ids(),
            vec![b, a],
            "Invert Selection still picks every annotation"
        );
    }

    #[test]
    fn select_all_selects_the_whole_image_in_every_tool() {
        for tool in [Tool::Move, Tool::Select, Tool::Arrow, Tool::Text, Tool::Pen] {
            let mut e = editor();
            e.add_object(
                "Arrow",
                Kind::Line {
                    a: Pt::new(20.0, 20.0),
                    b: Pt::new(120.0, 90.0),
                    control: None,
                },
                Style::default(),
            );
            e.set_tool(tool);
            e.select_all();
            assert_eq!(e.tool(), tool, "Select All keeps the active tool");
            assert_eq!(
                e.selected_region(),
                Some(IRect::new(0, 0, 400, 300)),
                "{tool:?}"
            );
            assert!(e.selected_ids().is_empty(), "{tool:?}");
        }

        let mut empty = editor();
        empty.set_tool(Tool::Move);
        empty.select_all();
        assert_eq!(empty.selected_region(), Some(IRect::new(0, 0, 400, 300)));
        assert!(!empty.is_modified());
    }

    #[test]
    fn select_region_starts_at_canvas_edges_without_modifiers() {
        let mut e = editor();
        e.set_tool(Tool::Select);
        e.mark_saved();

        for (start, end, expected) in [
            ((0.0, 0.0), (40.0, 30.0), IRect::new(0, 0, 40, 30)),
            ((200.0, 0.0), (240.0, 30.0), IRect::new(200, 0, 40, 30)),
            ((400.0, 0.0), (360.0, 30.0), IRect::new(360, 0, 40, 30)),
            ((400.0, 150.0), (360.0, 180.0), IRect::new(360, 150, 40, 30)),
            ((400.0, 300.0), (360.0, 270.0), IRect::new(360, 270, 40, 30)),
            ((200.0, 300.0), (240.0, 270.0), IRect::new(200, 270, 40, 30)),
            ((0.0, 300.0), (40.0, 270.0), IRect::new(0, 270, 40, 30)),
            ((0.0, 150.0), (40.0, 180.0), IRect::new(0, 150, 40, 30)),
        ] {
            e.select(None);
            drag(&mut e, start, end);
            assert_eq!(e.selected_region(), Some(expected), "start {start:?}");
            assert_eq!(e.document().size(), (400, 300), "start {start:?}");
            assert!(!e.is_modified(), "start {start:?}");
        }
    }

    #[test]
    fn canvas_resize_handles_remain_available_in_move_mode() {
        let mut e = editor();
        e.set_tool(Tool::Move);

        assert_eq!(e.canvas_handles().len(), 8);
        drag(&mut e, (0.0, 150.0), (-20.0, 150.0));

        assert_eq!(e.document().size(), (420, 300));
        assert!(e.is_modified());
    }

    #[test]
    fn move_picks_an_annotation_under_a_canvas_handle() {
        let mut e = editor();
        let rect = Box2::new(0.0, 0.0, 60.0, 40.0);
        let id = e.add_object(
            "Edge annotation",
            Kind::Shape {
                rect,
                shape: ShapeKind::Rectangle,
            },
            Style {
                fill: Rgba::WHITE,
                ..Style::default()
            },
        );
        e.set_tool(Tool::Move);
        e.select(None);

        assert_eq!(e.hover(Pt::new(0.0, 0.0), 4.0), Cursor::Move);
        drag(&mut e, (0.0, 0.0), (20.0, 20.0));

        assert_eq!(e.document().size(), (400, 300));
        assert_eq!(e.doc.get(id).unwrap().rect(), Some(rect.offset(20.0, 20.0)));
    }

    #[test]
    fn alt_drag_canvas_handle_still_resizes_with_a_drawing_tool() {
        let mut e = editor();
        e.set_tool(Tool::Arrow);
        let alt = Modifiers { alt: true, ..NONE };

        e.pointer_down(Pt::new(0.0, 150.0), alt, 4.0);
        e.pointer_move(Pt::new(-20.0, 150.0), alt);
        e.pointer_up(Pt::new(-20.0, 150.0), alt);

        assert_eq!(e.document().size(), (420, 300));
        assert!(e.document().objects.is_empty());
    }

    #[test]
    fn move_picks_annotations_while_select_starts_an_image_region() {
        let mut e = editor();
        let id = e.add_object(
            "Shape",
            Kind::Shape {
                rect: Box2::new(40.0, 40.0, 80.0, 80.0),
                shape: ShapeKind::Rectangle,
            },
            Style {
                width: 0.0,
                fill: Rgba::RED,
                ..Style::default()
            },
        );

        e.set_tool(Tool::Move);
        drag(&mut e, (50.0, 50.0), (60.0, 60.0));
        assert_eq!(e.selected_ids(), vec![id]);
        assert_eq!(
            e.doc.get(id).unwrap().rect(),
            Some(Box2::new(50.0, 50.0, 90.0, 90.0))
        );

        e.set_tool(Tool::Select);
        drag(&mut e, (45.0, 45.0), (75.0, 75.0));
        assert_eq!(e.selected_ids(), Vec::<u64>::new());
        assert_eq!(e.selected_region(), Some(IRect::new(45, 45, 30, 30)));
        assert_eq!(
            e.doc.get(id).unwrap().rect(),
            Some(Box2::new(50.0, 50.0, 90.0, 90.0))
        );
    }

    #[test]
    fn move_marquee_replaces_selection_by_default_and_shift_adds_objects() {
        let mut e = editor();
        let add_shape = |e: &mut Editor, x| {
            e.add_object(
                "Shape",
                Kind::Shape {
                    rect: Box2::new(x, 10.0, x + 20.0, 30.0),
                    shape: ShapeKind::Rectangle,
                },
                Style::default(),
            )
        };
        let first = add_shape(&mut e, 10.0);
        let second = add_shape(&mut e, 50.0);
        let third = add_shape(&mut e, 90.0);
        e.set_tool(Tool::Move);
        e.set_selection(vec![first]);

        drag(&mut e, (45.0, 5.0), (65.0, 35.0));
        assert_eq!(e.selected_ids(), vec![second]);

        let shift = Modifiers { shift: true, ..NONE };
        e.pointer_down(Pt::new(85.0, 5.0), shift, 4.0);
        e.pointer_move(Pt::new(95.0, 20.0), shift);
        e.pointer_up(Pt::new(105.0, 35.0), shift);
        assert_eq!(e.selected_ids(), vec![second, third]);

        e.pointer_down(Pt::new(150.0, 150.0), NONE, 4.0);
        e.pointer_up(Pt::new(150.0, 150.0), NONE);
        assert!(e.selected_ids().is_empty());
    }

    #[test]
    fn selected_objects_image_renders_selected_objects_in_document_order() {
        let mut e = editor();
        let red = e.add_object(
            "Red",
            Kind::Shape {
                rect: Box2::new(10.0, 10.0, 30.0, 30.0),
                shape: ShapeKind::Rectangle,
            },
            Style {
                width: 0.0,
                fill: Rgba::RED,
                ..Style::default()
            },
        );
        let blue = e.add_object(
            "Blue",
            Kind::Shape {
                rect: Box2::new(20.0, 20.0, 40.0, 40.0),
                shape: ShapeKind::Rectangle,
            },
            Style {
                width: 0.0,
                fill: Rgba::rgb(20, 40, 200),
                ..Style::default()
            },
        );
        e.set_selection(vec![blue, red]);

        let image = e
            .selected_objects_image()
            .expect("selected annotations render");
        assert_eq!((image.width(), image.height()), (34, 34));
        assert_eq!(raster::pixel(&image, 5, 5), Rgba::RED);
        assert_eq!(
            raster::pixel(&image, 15, 15),
            Rgba::rgb(20, 40, 200),
            "the later object in document order is on top"
        );
        assert_eq!(raster::pixel(&image, 1, 1), Rgba::TRANSPARENT);
    }

    #[test]
    fn selected_plain_image_copy_keeps_its_pixel_bounds() {
        let mut e = editor();
        let image = raster::solid(20, 16, Rgba::RED).unwrap();
        e.add_object(
            "Image",
            Kind::Image {
                rect: Box2::from_xywh(30.0, 40.0, 20.0, 16.0),
                image: ImageRef::new("clipboard-image", image),
            },
            Style { width: 0.0, ..Style::default() },
        );

        let copied = e.selected_objects_image().expect("selected image renders");
        assert_eq!((copied.width(), copied.height()), (20, 16));
        assert_eq!(raster::pixel(&copied, 0, 0), Rgba::RED);
    }

    #[test]
    fn selected_spotlight_copy_is_bounded_by_the_document() {
        let mut e = editor();
        e.add_object(
            "Spotlight",
            Kind::Spotlight {
                rect: Box2::new(100.0, 100.0, 200.0, 200.0),
                ellipse: false,
                dim: 0.5,
                blur: 0.0,
                rounded: false,
            },
            Style { width: 0.0, ..Style::default() },
        );

        let copied = e.selected_objects_image().expect("spotlight renders within the document");
        assert_eq!((copied.width(), copied.height()), (400, 300));
        assert!(raster::pixel(&copied, 20, 20).a > 0);
        assert_eq!(raster::pixel(&copied, 150, 150), Rgba::TRANSPARENT);
    }

    #[test]
    fn paste_objects_inserts_the_group_in_one_undo_step() {
        let mut e = editor();
        let first = e.add_object(
            "First",
            Kind::Shape {
                rect: Box2::new(10.0, 10.0, 30.0, 30.0),
                shape: ShapeKind::Rectangle,
            },
            Style::default(),
        );
        let second = e.add_object(
            "Second",
            Kind::Shape {
                rect: Box2::new(40.0, 10.0, 60.0, 30.0),
                shape: ShapeKind::Ellipse,
            },
            Style::default(),
        );
        e.set_selection(vec![first, second]);
        let copies = e.selected_objects();

        let pasted = e.paste_objects(copies);
        assert_eq!(pasted.len(), 2);
        assert_eq!(e.document().objects.len(), 4);
        assert_eq!(e.selected_ids(), pasted);
        assert_eq!(
            e.doc.get(pasted[0]).unwrap().rect(),
            Some(Box2::new(26.0, 26.0, 46.0, 46.0))
        );
        assert_eq!(
            e.doc.get(pasted[1]).unwrap().rect(),
            Some(Box2::new(56.0, 26.0, 76.0, 46.0))
        );
        assert_eq!(e.undo_label(), Some("Paste"));

        assert!(e.undo(), "one undo removes the complete pasted selection");
        assert_eq!(e.document().objects.len(), 2);
        assert!(pasted.iter().all(|id| e.doc.get(*id).is_none()));
        assert!(e.doc.get(first).is_some());
        assert!(e.doc.get(second).is_some());
    }

    fn drag(e: &mut Editor, a: (f32, f32), b: (f32, f32)) {
        e.pointer_down(Pt::new(a.0, a.1), NONE, 4.0);
        e.pointer_move(Pt::new((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0), NONE);
        e.pointer_up(Pt::new(b.0, b.1), NONE);
    }

    #[test]
    fn selecting_transparent_pixels_does_not_change_the_document() {
        for fill in [
            BackgroundFill::Auto,
            BackgroundFill::Transparent,
            BackgroundFill::Color,
        ] {
            let mut original = raster::solid(100, 80, Rgba::rgb(20, 100, 200)).unwrap();
            raster::set_pixel(&mut original, 30, 30, Rgba::new(200, 80, 40, 128));
            raster::set_pixel(&mut original, 31, 30, Rgba::TRANSPARENT);
            let mut e = Editor::new(Document::new(original.clone()));
            e.background_fill = fill;
            e.set_tool(Tool::Select);
            drag(&mut e, (20.0, 20.0), (60.0, 50.0));

            assert_eq!(e.selection_box(), Some(Box2::new(20.0, 20.0, 60.0, 50.0)));
            assert!(
                e.document().flatten().data() == original.data(),
                "{fill:?} must not change pixels until the selection is edited"
            );
            assert!(e.document().objects.is_empty());
            assert!(!e.is_modified());
            assert_eq!(e.undo_label(), None);
        }
    }

    #[test]
    fn drawing_tool_selection_handles_remain_interactive() {
        for (tool, variant) in [(Tool::Pen, "Pen"), (Tool::Highlight, "Freehand")] {
            let mut e = editor();
            e.set_tool(tool);
            e.set_variant(variant);
            drag(&mut e, (60.0, 60.0), (140.0, 120.0));
            let before = e.selection_box().unwrap();
            let handle = e
                .selection_handles()
                .into_iter()
                .find(|(h, _)| *h == H_BR)
                .unwrap()
                .1;
            assert_eq!(e.hover(handle, 4.0), Cursor::ResizeNwse, "{tool:?}");
            drag(&mut e, (handle.x, handle.y), (handle.x + 30.0, handle.y + 20.0));
            assert_eq!(e.document().objects.len(), 1, "{tool:?}");
            assert_ne!(e.selection_box(), Some(before), "{tool:?} resize");
        }

        let mut e = editor();
        e.set_tool(Tool::Arrow);
        drag(&mut e, (60.0, 60.0), (140.0, 120.0));
        e.update_style("end-cap", |style| style.end_cap = Cap::OpenArrow);
        let end = Pt::new(140.0, 120.0);
        assert_eq!(e.hover(end, 4.0), Cursor::Point);
        drag(&mut e, (end.x, end.y), (180.0, 150.0));
        assert_eq!(e.document().objects.len(), 1);
        assert!(matches!(e.selected().map(|o| &o.kind),
            Some(Kind::Line { b, .. }) if *b == Pt::new(180.0, 150.0)));

        let mut e = editor();
        e.custom_stamps.push((
            "Custom".to_owned(),
            std::sync::Arc::new(raster::solid(32, 32, Rgba::RED).unwrap()),
        ));
        e.set_tool(Tool::Stamp);
        e.set_variant("Custom");
        e.pointer_down(Pt::new(100.0, 100.0), NONE, 4.0);
        e.pointer_up(Pt::new(100.0, 100.0), NONE);
        let before = e.selection_box().unwrap();
        let handle = e
            .selection_handles()
            .into_iter()
            .find(|(h, _)| *h == H_BR)
            .unwrap()
            .1;
        assert_eq!(e.hover(handle, 4.0), Cursor::ResizeNwse);
        drag(&mut e, (handle.x, handle.y), (handle.x + 20.0, handle.y + 20.0));
        assert_eq!(e.document().objects.len(), 1);
        assert_ne!(e.selection_box(), Some(before));
    }

    #[test]
    fn image_corner_handles_keep_proportions_unless_shift_is_held() {
        let resize = |m: Modifiers| {
            let mut e = editor();
            e.set_tool(Tool::Move);
            e.paste_image(raster::solid(200, 100, Rgba::RED).unwrap());
            assert_eq!(e.selection_box(), Some(Box2::new(100.0, 100.0, 300.0, 200.0)));
            e.pointer_down(Pt::new(300.0, 200.0), m, 4.0);
            // Passing close to and past the fixed corner must not lose the
            // ratio or move that corner.
            e.pointer_move(Pt::new(101.0, 100.5), m);
            e.pointer_move(Pt::new(95.0, 95.0), m);
            e.pointer_move(Pt::new(100.0, 200.0), m);
            e.pointer_move(Pt::new(340.0, 260.0), m);
            e.pointer_up(Pt::new(340.0, 260.0), m);
            e.selected().and_then(Object::rect).unwrap()
        };
        assert_eq!(resize(NONE), Box2::new(100.0, 100.0, 420.0, 260.0));
        let shift = Modifiers { shift: true, ..NONE };
        assert_eq!(resize(shift), Box2::new(100.0, 100.0, 340.0, 260.0));
    }

    #[test]
    fn select_drag_creates_a_movable_image_region() {
        let original = raster::solid(400, 300, Rgba::rgb(20, 100, 200)).unwrap();
        let mut e = Editor::new(Document::new(original.clone()));
        e.background_fill = BackgroundFill::Transparent;
        e.set_tool(Tool::Select);
        e.pointer_down(Pt::new(40.0, 30.0), NONE, 4.0);
        e.pointer_move(Pt::new(160.0, 120.0), NONE);
        assert_eq!(e.marquee(), Some(Box2::new(40.0, 30.0, 160.0, 120.0)));
        assert!(!e.marquee_is_box_selection(), "plain Select selects pixels");
        e.pointer_up(Pt::new(160.0, 120.0), NONE);
        assert_eq!(e.selection_box(), Some(Box2::new(40.0, 30.0, 160.0, 120.0)));
        assert_eq!(e.doc.flatten().data(), original.data());
        assert_eq!(e.selected_region(), Some(IRect::new(40, 30, 120, 90)));
        assert!(e.doc.objects.is_empty());
        assert!(!e.is_modified());

        drag(&mut e, (80.0, 60.0), (200.0, 160.0));
        assert_eq!(
            e.selection_box(),
            Some(Box2::new(160.0, 130.0, 280.0, 220.0))
        );
        assert_eq!(raster::pixel(&e.doc.flatten(), 80, 60), Rgba::TRANSPARENT);
        assert_eq!(
            raster::pixel(&e.doc.flatten(), 200, 160),
            Rgba::rgb(20, 100, 200)
        );
        assert!(e.undo(), "moving the region is undoable");
        assert_eq!(e.doc.flatten().data(), original.data());
        assert!(e.doc.objects.is_empty());
        assert_eq!(e.doc.background().data(), original.data());
        assert!(!e.undo(), "selecting a region does not add an undo step");
        assert!(e.redo());
        e.select(e.doc.objects.first().map(|o| o.id));
        assert_eq!(
            e.selection_box(),
            Some(Box2::new(160.0, 130.0, 280.0, 220.0))
        );
        assert!(e.delete_selected(), "the region can be cut independently");
        assert_eq!(raster::pixel(&e.doc.flatten(), 80, 60), Rgba::TRANSPARENT);
    }

    #[test]
    fn select_region_clips_reverse_drags_and_ignores_clicks() {
        let mut e = editor();
        e.set_tool(Tool::Select);
        drag(&mut e, (70.0, 60.0), (71.0, 61.0));
        assert!(e.doc.objects.is_empty());
        assert!(!e.is_modified());
        assert_eq!(e.undo_label(), None);
        drag(&mut e, (100.0, 80.0), (-20.0, -10.0));
        assert_eq!(e.selection_box(), Some(Box2::new(0.0, 0.0, 100.0, 80.0)));
    }

    #[test]
    fn region_move_applies_the_selected_fill_and_is_one_undo_step() {
        let surround = Rgba::rgb(10, 20, 30);
        let picked = Rgba::rgb(200, 80, 40);
        let color = Rgba::rgb(30, 180, 90);
        for fill in [
            BackgroundFill::Auto,
            BackgroundFill::Transparent,
            BackgroundFill::Color,
        ] {
            let mut original = raster::solid(100, 80, surround).unwrap();
            for y in 20..50 {
                for x in 20..50 {
                    raster::set_pixel(&mut original, x, y, picked);
                }
            }
            raster::set_pixel(&mut original, 25, 25, picked.with_alpha(128));
            raster::set_pixel(&mut original, 26, 25, Rgba::TRANSPARENT);
            let mut e = Editor::new(Document::new(original.clone()));
            e.set_tool(Tool::Select);
            e.update_style("fill", |style| style.fill = color);
            e.background_fill = fill;
            drag(&mut e, (20.0, 20.0), (50.0, 50.0));
            drag(&mut e, (30.0, 30.0), (70.0, 50.0));

            let expected_fill = match fill {
                BackgroundFill::Auto => surround,
                BackgroundFill::Transparent => Rgba::TRANSPARENT,
                BackgroundFill::Color => color,
            };
            assert_eq!(
                raster::pixel(e.document().background(), 30, 30),
                expected_fill,
                "{fill:?} fills the source only when it moves"
            );
            assert_eq!(raster::pixel(&e.document().flatten(), 70, 50), picked);
            let Some(Kind::Image { image, .. }) = e.selected().map(|o| &o.kind) else {
                panic!("moving {fill:?} must create a movable image");
            };
            let image = image.pixmap.as_ref().unwrap();
            assert_eq!(raster::pixel(image, 5, 5), raster::pixel(&original, 25, 25));
            assert_eq!(raster::pixel(image, 6, 5), Rgba::TRANSPARENT);
            assert!(e.undo());
            assert!(e.document().objects.is_empty());
            assert!(e.document().flatten().data() == original.data());
            assert!(!e.undo(), "{fill:?} selection must not add another undo step");
            assert!(e.redo());
            assert_eq!(raster::pixel(e.document().background(), 30, 30), expected_fill);
            assert_eq!(raster::pixel(&e.document().flatten(), 70, 50), picked);
        }
    }

    #[test]
    fn region_handles_resize_the_selection_without_editing_pixels() {
        let mut e = editor();
        let before = e.document().background().clone();
        e.set_tool(Tool::Select);
        drag(&mut e, (40.0, 30.0), (120.0, 100.0));
        assert_eq!(e.hover(Pt::new(120.0, 100.0), 4.0), Cursor::ResizeNwse);
        drag(&mut e, (120.0, 100.0), (160.0, 140.0));
        assert_eq!(e.selected_region(), Some(IRect::new(40, 30, 120, 110)));
        assert!(e.document().background().data() == before.data());
        assert!(e.document().objects.is_empty());
        assert!(!e.is_modified());
        assert_eq!(e.undo_label(), None);
    }

    #[test]
    fn selecting_and_cancelling_a_region_preserves_redo() {
        let mut e = editor();
        drag(&mut e, (200.0, 100.0), (300.0, 180.0));
        assert!(e.undo());
        e.mark_saved();
        e.set_tool(Tool::Select);
        drag(&mut e, (40.0, 30.0), (160.0, 120.0));
        assert_eq!(e.redo_label(), Some("Add Arrow"));
        assert!(!e.is_modified());
        e.set_tool(Tool::Arrow);
        assert_eq!(e.selected_region(), None);
        assert!(!e.is_modified());
        assert!(e.redo());
        assert_eq!(e.document().objects.len(), 1);
    }

    #[test]
    fn region_delete_duplicate_and_nudge_preserve_undo() {
        for action in ["delete", "duplicate", "nudge"] {
            let mut e = editor();
            let original = e.document().background().clone();
            e.set_tool(Tool::Select);
            e.background_fill = BackgroundFill::Transparent;
            drag(&mut e, (40.0, 30.0), (160.0, 120.0));
            match action {
                "delete" => assert!(e.delete_selected()),
                "duplicate" => assert!(e.duplicate_selected().is_some()),
                _ => e.nudge(15.0, 12.0),
            }
            assert!(e.is_modified(), "{action}");
            assert_eq!(e.selected_region(), None, "{action}");
            assert!(e.undo(), "{action}");
            assert!(e.document().background().data() == original.data(), "{action}");
            assert!(e.document().objects.is_empty(), "{action}");
            assert!(!e.undo(), "{action} must use one undo step");
        }
    }

    #[test]
    fn select_drag_keeps_overlapping_annotations_editable() {
        let mut e = editor();
        let rect = Box2::new(80.0, 60.0, 120.0, 100.0);
        let annotation = e.add_object(
            "Existing shape",
            Kind::Shape {
                rect,
                shape: ShapeKind::Rectangle,
            },
            Style::default(),
        );
        e.mark_saved();
        let before = e.doc.flatten();
        e.set_tool(Tool::Select);
        drag(&mut e, (40.0, 30.0), (160.0, 120.0));
        assert_eq!(e.selection_box(), Some(Box2::new(40.0, 30.0, 160.0, 120.0)));
        assert_eq!(e.selected_region(), Some(IRect::new(40, 30, 120, 90)));
        assert_eq!(e.doc.get(annotation).unwrap().rect(), Some(rect));
        assert!(e.selected_ids().is_empty());
        assert_eq!(e.doc.objects.len(), 1);
        assert!(
            e.doc.flatten().data() == before.data(),
            "selecting must not hide annotations"
        );

        e.background_fill = BackgroundFill::Transparent;
        drag(&mut e, (60.0, 50.0), (220.0, 180.0));
        assert_eq!(e.doc.get(annotation).unwrap().rect(), Some(rect));
        assert_eq!(e.selected_region(), Some(IRect::new(40, 30, 120, 90)));
        assert_eq!(e.doc.objects.len(), 1);
        assert!(
            !e.is_modified(),
            "a rejected overlapping move must not edit the document"
        );
        assert_eq!(e.doc.get(annotation).unwrap().rect(), Some(rect));
        assert!(e.doc.flatten().data() == before.data());
    }

    #[test]
    fn deleting_an_overlapping_image_region_preserves_the_annotation_and_pixels() {
        let mut e = editor();
        let annotation = e.add_object(
            "Existing shape",
            Kind::Shape {
                rect: Box2::new(80.0, 60.0, 120.0, 100.0),
                shape: ShapeKind::Rectangle,
            },
            Style {
                width: 0.0,
                fill: Rgba::rgb(20, 40, 200),
                ..Style::default()
            },
        );
        e.set_tool(Tool::Select);
        drag(&mut e, (40.0, 30.0), (160.0, 120.0));
        let before = e.document().flatten();
        let region = e.selected_region();
        let undo_label = e.undo_label().map(str::to_owned);
        assert!(e.region_overlaps_annotations());

        assert!(!e.delete_selected(), "overlapping region cuts are rejected");
        assert_eq!(e.selected_region(), region);
        assert_eq!(
            e.doc.get(annotation).unwrap().rect(),
            Some(Box2::new(80.0, 60.0, 120.0, 100.0))
        );
        assert_eq!(e.document().flatten().data(), before.data());
        assert_eq!(
            e.undo_label(),
            undo_label.as_deref(),
            "selection rejection does not add an undo step"
        );
    }

    #[test]
    fn alt_drag_starts_a_region_over_objects_and_existing_handles() {
        for image in [false, true] {
            let mut e = editor();
            let rect = Box2::new(50.0, 50.0, 180.0, 140.0);
            let kind = if image {
                Kind::Image {
                    rect,
                    image: ImageRef::new(
                        "test-image",
                        raster::solid(130, 90, Rgba::WHITE).unwrap(),
                    ),
                }
            } else {
                Kind::Shape {
                    rect,
                    shape: ShapeKind::Rectangle,
                }
            };
            let id = e.add_object(
                "Existing object",
                kind,
                Style {
                    fill: Rgba::WHITE,
                    ..Style::default()
                },
            );
            e.set_tool(Tool::Select);
            e.select(Some(id));
            e.mark_saved();
            let before = e.doc.flatten();
            let alt = Modifiers { alt: true, ..NONE };

            for (start, end, expected) in [
                (Pt::new(50.0, 50.0), Pt::new(100.0, 90.0), IRect::new(50, 50, 50, 40)),
                (Pt::new(70.0, 70.0), Pt::new(130.0, 120.0), IRect::new(70, 70, 60, 50)),
                (Pt::new(0.0, 150.0), Pt::new(60.0, 200.0), IRect::new(0, 150, 60, 50)),
            ] {
                e.select(None);
                e.pointer_down(start, alt, 4.0);
                e.pointer_move(end, alt);
                e.pointer_up(end, alt);
                assert_eq!(e.selected_region(), Some(expected));
                assert_eq!(e.doc.get(id).unwrap().rect(), Some(rect));
                assert_eq!(e.doc.size(), (400, 300));
                assert!(!e.is_modified());
                assert_eq!(e.doc.flatten().data(), before.data());
            }

            e.select(None);
            e.set_tool(Tool::Move);
            drag(&mut e, (90.0, 80.0), (110.0, 100.0));
            assert_eq!(e.doc.get(id).unwrap().rect(), Some(rect.offset(20.0, 20.0)));
        }
    }

    #[test]
    fn select_region_and_arrow_support_repeated_tool_switches() {
        let mut e = editor();
        for i in 0..3 {
            e.set_tool(Tool::Select);
            drag(&mut e, (40.0, 30.0), (160.0, 120.0));
            let region = Some(IRect::new(40, 30, 120, 90));
            assert_eq!(e.selected_region(), region);
            e.pointer_down(Pt::new(70.0, 60.0), NONE, 4.0);
            e.pointer_up(Pt::new(70.0, 60.0), NONE);
            assert_eq!(e.selected_region(), region);
            e.set_tool(Tool::Arrow);
            assert_eq!(e.selected_region(), None);
            let start = Pt::new(90.0, 80.0 + i as f32 * 15.0);
            let end = Pt::new(250.0, 180.0 + i as f32 * 15.0);
            e.pointer_down(start, NONE, 4.0);
            e.pointer_up(end, NONE);
            let arrow = e.selection().expect("Arrow must create an annotation");
            assert!(matches!(e.selected().map(|o| &o.kind),
                Some(Kind::Line { a, b, .. }) if *a == start && *b == end));
            assert_eq!(e.doc.objects.len(), i + 1);
            e.set_tool(Tool::Move);
            e.pointer_down(end, NONE, 4.0);
            e.pointer_up(end, NONE);
            assert_eq!(e.selection(), Some(arrow));
        }
    }

    #[test]
    fn arrow_after_select_draws_over_images_and_other_annotations() {
        for image in [false, true] {
            for start in [Pt::new(50.0, 50.0), Pt::new(90.0, 80.0)] {
                let mut e = editor();
                let rect = Box2::new(50.0, 50.0, 180.0, 140.0);
                let kind = if image {
                    Kind::Image {
                        rect,
                        image: ImageRef::new(
                            "test-image",
                            raster::solid(130, 90, Rgba::WHITE).unwrap(),
                        ),
                    }
                } else {
                    Kind::Shape {
                        rect,
                        shape: ShapeKind::Rectangle,
                    }
                };
                let id = e.add_object(
                    "Existing object",
                    kind,
                    Style {
                        fill: Rgba::WHITE,
                        ..Style::default()
                    },
                );
                e.set_tool(Tool::Select);
                e.select(Some(id));
                e.set_tool(Tool::Arrow);
                assert_eq!(e.hover(start, 4.0), Cursor::Cross);
                let end = Pt::new(240.0, 200.0);
                e.pointer_down(start, NONE, 4.0);
                e.pointer_up(end, NONE);
                assert_eq!(
                    e.doc.get(id).unwrap().rect(),
                    Some(rect),
                    "existing object must not move"
                );
                assert_eq!(e.doc.objects.len(), 2, "Arrow must create a new annotation");
                assert!(matches!(e.selected().map(|o| &o.kind),
                    Some(Kind::Line { a, b, .. }) if *a == start && *b == end));
                assert_eq!(e.selected().unwrap().style.end_cap, Cap::Arrow);
                e.set_tool(Tool::Arrow);
                let arrow = e.doc.objects.last().unwrap().id;
                let midpoint = Pt::new((start.x + end.x) / 2.0, (start.y + end.y) / 2.0);
                e.pointer_down(midpoint, NONE, 4.0);
                e.pointer_up(midpoint, NONE);
                assert_eq!(e.selection(), Some(arrow), "Move picks annotations");
            }
        }
    }

    #[test]
    fn move_then_arrow_starts_a_new_arrow_on_an_existing_arrow() {
        let mut e = editor();
        drag(&mut e, (50.0, 50.0), (200.0, 50.0));
        let first = e.selection().unwrap();
        e.set_tool(Tool::Move);
        e.pointer_down(Pt::new(100.0, 50.0), NONE, 4.0);
        e.pointer_up(Pt::new(100.0, 50.0), NONE);
        assert_eq!(e.selection(), Some(first));
        e.set_tool(Tool::Arrow);
        assert_eq!(e.hover(Pt::new(100.0, 50.0), 4.0), Cursor::Cross);
        drag(&mut e, (100.0, 50.0), (100.0, 180.0));
        assert_eq!(e.doc.objects.len(), 2);
        assert!(matches!(e.doc.get(first).map(|o| &o.kind),
            Some(Kind::Line { a, b, .. })
                if *a == Pt::new(50.0, 50.0) && *b == Pt::new(200.0, 50.0)));
        assert!(matches!(e.selected().map(|o| &o.kind),
            Some(Kind::Line { a, b, .. })
                if *a == Pt::new(100.0, 50.0) && *b == Pt::new(100.0, 180.0)));
    }

    #[test]
    fn switching_tools_cancels_pending_region_drags() {
        for tool in [Tool::Select, Tool::Selection, Tool::Crop] {
            let mut e = editor();
            e.set_tool(tool);
            e.pointer_down(Pt::new(40.0, 30.0), NONE, 4.0);
            e.pointer_move(Pt::new(160.0, 120.0), NONE);
            e.set_tool(Tool::Arrow);
            assert_eq!(e.marquee(), None);
            e.pointer_move(Pt::new(180.0, 150.0), NONE);
            assert_eq!(e.marquee(), None, "old {tool:?} drag must not resume");
            e.pointer_up(Pt::new(200.0, 180.0), NONE);
            assert!(
                e.doc.objects.is_empty(),
                "old {tool:?} gesture must not commit"
            );
            assert_eq!(e.doc.size(), (400, 300));
            assert_eq!(e.undo_label(), None);
            drag(&mut e, (60.0, 60.0), (180.0, 160.0));
            assert_eq!(e.doc.objects.len(), 1);
            assert_eq!(e.selected().map(tool_for), Some(Tool::Arrow));
        }
    }

    #[test]
    fn switching_tools_cancels_unfinished_annotations_without_losing_redo() {
        let mut e = editor();
        drag(&mut e, (40.0, 40.0), (160.0, 100.0));
        assert!(e.undo());
        e.mark_saved();
        e.pointer_down(Pt::new(60.0, 60.0), NONE, 4.0);
        e.pointer_move(Pt::new(180.0, 160.0), NONE);
        e.set_tool(Tool::Select);
        e.pointer_up(Pt::new(180.0, 160.0), NONE);
        assert!(
            e.doc.objects.is_empty(),
            "unfinished Arrow must be cancelled"
        );
        assert!(!e.is_modified());
        assert_eq!(e.undo_label(), None);
        assert_eq!(e.redo_label(), Some("Add Arrow"));
        assert!(e.redo());
        assert_eq!(e.doc.objects.len(), 1);
    }

    #[test]
    fn switching_tools_preserves_edits_made_during_annotation_creation() {
        let mut e = editor();
        let before = Box2::new(20.0, 20.0, 60.0, 60.0);
        let after = before.offset(10.0, 15.0);
        let shape = e.add_object(
            "Shape",
            Kind::Shape { rect: before, shape: ShapeKind::Rectangle },
            Style::default(),
        );
        e.set_tool(Tool::Arrow);
        e.pointer_down(Pt::new(100.0, 100.0), NONE, 4.0);
        e.pointer_move(Pt::new(200.0, 160.0), NONE);
        let arrow = e.selection().unwrap();
        e.edit(shape, "Edit shape", None, |object| {
            if let Kind::Shape { rect, .. } = &mut object.kind {
                *rect = after;
            }
        });

        e.set_tool(Tool::Select);
        assert_eq!(e.doc.get(shape).unwrap().rect(), Some(after));
        assert!(matches!(e.doc.get(arrow).map(|object| &object.kind),
            Some(Kind::Line { a, b, .. })
                if *a == Pt::new(100.0, 100.0) && *b == Pt::new(200.0, 160.0)));
        assert!(e.undo());
        assert_eq!(e.doc.get(shape).unwrap().rect(), Some(before));
        assert!(e.doc.get(arrow).is_some());
        assert!(e.undo());
        assert!(e.doc.get(arrow).is_none());
        assert_eq!(e.doc.get(shape).unwrap().rect(), Some(before));
    }

    #[test]
    fn draw_select_move_undo_redo() {
        let mut e = editor();
        e.set_tool(Tool::Arrow);
        drag(&mut e, (50.0, 50.0), (200.0, 50.0));
        assert_eq!(e.document().objects.len(), 1);
        let id = e.selection().unwrap();
        let damage = e.take_damage().unwrap();
        assert!(
            damage.w < 220 && damage.h < 80,
            "only the arrow was redrawn: {damage:?}"
        );
        // Click on the arrow with the same tool picks it up and moves it.
        drag(&mut e, (90.0, 50.0), (90.0, 150.0));
        assert_eq!(e.document().objects.len(), 1);
        let Kind::Line { a, .. } = e.document().get(id).unwrap().kind else {
            panic!()
        };
        assert_eq!(a, Pt::new(50.0, 150.0));
        assert!(e.undo());
        let Kind::Line { a, .. } = e.document().get(id).unwrap().kind else {
            panic!()
        };
        assert_eq!(a, Pt::new(50.0, 50.0));
        assert!(e.undo());
        assert!(e.document().objects.is_empty());
        assert!(e.redo() && e.redo());
        assert_eq!(e.document().objects.len(), 1);
        // A plain click with a drawing tool adds nothing.
        e.select(None);
        e.pointer_down(Pt::new(300.0, 250.0), NONE, 4.0);
        e.pointer_up(Pt::new(300.0, 250.0), NONE);
        assert_eq!(e.document().objects.len(), 1);
    }

    #[test]
    fn selecting_a_missing_object_clears_the_selection() {
        let mut e = editor();
        e.select(Some(u64::MAX));
        assert_eq!(e.selection(), None);
        assert!(e.selected_ids().is_empty());
        assert!(!e.delete_selected());
    }

    #[test]
    fn canvas_resize_rejects_dimensions_that_documents_cannot_save() {
        let mut e = editor();
        e.set_tool(Tool::Select);
        e.pointer_down(Pt::new(400.0, 300.0), NONE, 4.0);
        e.pointer_up(Pt::new(8_000.0, 5_000.0), NONE);
        assert_eq!(e.document().size(), (400, 300));
    }

    fn patterned_canvas() -> (Editor, u64) {
        let mut background = Pixmap::new(80, 60).unwrap();
        for y in 0..60 {
            for x in 0..80 {
                let color = match (x + y) % 3 {
                    0 => Rgba::TRANSPARENT,
                    1 => Rgba::new(x as u8, y as u8, 160, 128),
                    _ => Rgba::rgb(x as u8, y as u8, 160),
                };
                raster::set_pixel(&mut background, x, y, color);
            }
        }
        let mut doc = Document::new(background);
        let id = doc.add(
            Kind::Shape {
                rect: Box2::new(25.0, 20.0, 45.0, 40.0),
                shape: ShapeKind::Rectangle,
            },
            Style::default(),
        );
        let mut e = Editor::new(doc);
        e.set_tool(Tool::Arrow);
        e.canvas_color = Rgba::new(20, 40, 60, 192);
        e.take_events();
        e.take_damage();
        (e, id)
    }

    #[test]
    fn canvas_handles_cover_each_edge_and_corner() {
        let mut e = editor();
        e.set_tool(Tool::Move);
        let expected = [
            (H_TL, Pt::new(0.0, 0.0), Cursor::ResizeNwse),
            (H_T, Pt::new(200.0, 0.0), Cursor::ResizeNs),
            (H_TR, Pt::new(400.0, 0.0), Cursor::ResizeNesw),
            (H_R, Pt::new(400.0, 150.0), Cursor::ResizeEw),
            (H_BR, Pt::new(400.0, 300.0), Cursor::ResizeNwse),
            (H_B, Pt::new(200.0, 300.0), Cursor::ResizeNs),
            (H_BL, Pt::new(0.0, 300.0), Cursor::ResizeNesw),
            (H_L, Pt::new(0.0, 150.0), Cursor::ResizeEw),
        ];
        assert_eq!(
            e.canvas_handles(),
            expected
                .iter()
                .map(|(h, p, _)| (*h, *p))
                .collect::<Vec<_>>()
        );
        for (handle, point, cursor) in expected {
            assert_eq!(e.hover(point, 4.0), cursor, "handle {handle}");
        }

        for tool in [Tool::Move, Tool::Arrow, Tool::Shape, Tool::Line] {
            e.set_tool(tool);
            assert_eq!(e.canvas_handles().len(), 8, "tool {tool:?}");
        }
        for tool in [Tool::Select, Tool::Crop, Tool::Text, Tool::Pen] {
            e.set_tool(tool);
            assert!(e.canvas_handles().is_empty(), "tool {tool:?}");
        }
        e.set_tool(Tool::Move);
        let id = e.doc.add(
            Kind::Shape {
                rect: Box2::new(20.0, 20.0, 60.0, 60.0),
                shape: ShapeKind::Rectangle,
            },
            Style::default(),
        );
        e.select(Some(id));
        assert!(e.canvas_handles().is_empty(), "selection owns the handles");
    }

    #[test]
    fn canvas_handles_crop_and_expand_without_compositing_pixels() {
        let cases = [
            (
                "top_left_in",
                (0.0, 0.0),
                (10.0, 8.0),
                (10.0, 8.0, 80.0, 60.0),
                (-10, -8),
            ),
            (
                "top_in",
                (40.0, 0.0),
                (40.0, 8.0),
                (0.0, 8.0, 80.0, 60.0),
                (0, -8),
            ),
            (
                "top_right_in",
                (80.0, 0.0),
                (70.0, 8.0),
                (0.0, 8.0, 70.0, 60.0),
                (0, -8),
            ),
            (
                "right_in",
                (80.0, 30.0),
                (70.0, 30.0),
                (0.0, 0.0, 70.0, 60.0),
                (0, 0),
            ),
            (
                "bottom_right_in",
                (80.0, 60.0),
                (70.0, 52.0),
                (0.0, 0.0, 70.0, 52.0),
                (0, 0),
            ),
            (
                "bottom_in",
                (40.0, 60.0),
                (40.0, 52.0),
                (0.0, 0.0, 80.0, 52.0),
                (0, 0),
            ),
            (
                "bottom_left_in",
                (0.0, 60.0),
                (10.0, 52.0),
                (10.0, 0.0, 80.0, 52.0),
                (-10, 0),
            ),
            (
                "left_in",
                (0.0, 30.0),
                (10.0, 30.0),
                (10.0, 0.0, 80.0, 60.0),
                (-10, 0),
            ),
            (
                "top_left_out",
                (0.0, 0.0),
                (-10.0, -8.0),
                (-10.0, -8.0, 80.0, 60.0),
                (10, 8),
            ),
            (
                "top_out",
                (40.0, 0.0),
                (40.0, -8.0),
                (0.0, -8.0, 80.0, 60.0),
                (0, 8),
            ),
            (
                "top_right_out",
                (80.0, 0.0),
                (90.0, -8.0),
                (0.0, -8.0, 90.0, 60.0),
                (0, 8),
            ),
            (
                "right_out",
                (80.0, 30.0),
                (90.0, 30.0),
                (0.0, 0.0, 90.0, 60.0),
                (0, 0),
            ),
            (
                "bottom_right_out",
                (80.0, 60.0),
                (90.0, 68.0),
                (0.0, 0.0, 90.0, 68.0),
                (0, 0),
            ),
            (
                "bottom_out",
                (40.0, 60.0),
                (40.0, 68.0),
                (0.0, 0.0, 80.0, 68.0),
                (0, 0),
            ),
            (
                "bottom_left_out",
                (0.0, 60.0),
                (-10.0, 68.0),
                (-10.0, 0.0, 80.0, 68.0),
                (10, 0),
            ),
            (
                "left_out",
                (0.0, 30.0),
                (-10.0, 30.0),
                (-10.0, 0.0, 80.0, 60.0),
                (10, 0),
            ),
            (
                "top_right_mixed",
                (80.0, 0.0),
                (90.0, 8.0),
                (0.0, 8.0, 90.0, 60.0),
                (0, -8),
            ),
            (
                "bottom_left_mixed",
                (0.0, 60.0),
                (-10.0, 52.0),
                (-10.0, 0.0, 80.0, 52.0),
                (10, 0),
            ),
        ];
        for (name, start, end, bounds, (dx, dy)) in cases {
            let (mut e, id) = patterned_canvas();
            let original = e.document().background().clone();
            let original_rect = e.document().get(id).unwrap().rect().unwrap();
            let expected = Box2::new(bounds.0, bounds.1, bounds.2, bounds.3);
            e.pointer_down(Pt::new(start.0, start.1), NONE, 4.0);
            e.pointer_move(Pt::new(end.0, end.1), NONE);
            assert_eq!(e.marquee(), Some(expected), "preview for {name}");
            e.pointer_up(Pt::new(end.0, end.1), NONE);
            let (width, height) = (expected.width() as u32, expected.height() as u32);
            assert_eq!(e.document().size(), (width, height), "size for {name}");
            assert_eq!(
                e.document().get(id).unwrap().rect(),
                Some(original_rect.offset(dx as f32, dy as f32)),
                "annotation for {name}"
            );
            for y in 0..height {
                for x in 0..width {
                    let (sx, sy) = (x as i32 - dx, y as i32 - dy);
                    let expected_pixel = if (0..80).contains(&sx) && (0..60).contains(&sy) {
                        original.pixel(sx as u32, sy as u32).unwrap()
                    } else {
                        e.canvas_color.premultiplied()
                    };
                    assert_eq!(
                        e.document().background().pixel(x, y),
                        Some(expected_pixel),
                        "pixel ({x}, {y}) for {name}"
                    );
                }
            }
            let resized = e.document().background().data().to_vec();
            assert_eq!(e.undo_label(), Some("Resize canvas"), "history for {name}");
            assert!(e.undo(), "undo for {name}");
            assert_eq!(e.document().size(), (80, 60), "undo size for {name}");
            assert_eq!(
                e.document().background().data(),
                original.data(),
                "undo pixels for {name}"
            );
            assert_eq!(
                e.document().get(id).unwrap().rect(),
                Some(original_rect),
                "undo object for {name}"
            );
            assert!(e.redo(), "redo for {name}");
            assert_eq!(e.document().size(), (width, height), "redo size for {name}");
            assert_eq!(
                e.document().background().data(),
                resized,
                "redo pixels for {name}"
            );
            assert_eq!(
                e.document().get(id).unwrap().rect(),
                Some(original_rect.offset(dx as f32, dy as f32)),
                "redo object for {name}"
            );
        }
    }

    #[test]
    fn canvas_handles_choose_the_nearest_overlapping_hit() {
        let mut e = Editor::new(Document::blank(8, 8, Rgba::WHITE));
        e.set_tool(Tool::Arrow);
        for (point, cursor) in [
            (Pt::new(0.0, 0.0), Cursor::ResizeNwse),
            (Pt::new(4.0, 0.0), Cursor::ResizeNs),
            (Pt::new(8.0, 0.0), Cursor::ResizeNesw),
            (Pt::new(8.0, 4.0), Cursor::ResizeEw),
            (Pt::new(8.0, 8.0), Cursor::ResizeNwse),
            (Pt::new(4.0, 8.0), Cursor::ResizeNs),
            (Pt::new(0.0, 8.0), Cursor::ResizeNesw),
            (Pt::new(0.0, 4.0), Cursor::ResizeEw),
        ] {
            assert_eq!(e.hover(point, 4.0), cursor, "handle at {point:?}");
        }
        for (start, end, expected) in [
            ((4.0, 0.0), (4.0, -4.0), (8, 12)),
            ((0.0, 4.0), (-4.0, 4.0), (12, 8)),
        ] {
            let mut e = Editor::new(Document::blank(8, 8, Rgba::WHITE));
            e.set_tool(Tool::Arrow);
            drag(&mut e, start, end);
            assert_eq!(e.document().size(), expected, "handle at {start:?}");
        }
    }

    #[test]
    fn canvas_handles_preserve_pointer_offset_and_ignore_clicks() {
        for (start, end, expected) in [
            ((2.0, 30.0), (12.0, 30.0), (70, 60)),
            ((78.0, 30.0), (68.0, 30.0), (70, 60)),
            ((40.0, 2.0), (40.0, 10.0), (80, 52)),
            ((40.0, 58.0), (40.0, 50.0), (80, 52)),
        ] {
            let (mut e, _) = patterned_canvas();
            e.pointer_down(Pt::new(start.0, start.1), NONE, 4.0);
            e.pointer_up(Pt::new(start.0, start.1), NONE);
            assert_eq!(e.document().size(), (80, 60), "click at {start:?}");
            assert_eq!(e.undo_label(), None, "click at {start:?}");
            assert!(!e.is_modified(), "click at {start:?}");
            drag(&mut e, start, end);
            assert_eq!(e.document().size(), expected, "drag from {start:?}");
        }
    }

    #[test]
    fn canvas_handles_clamp_when_crossing_the_opposite_edge() {
        for (start, end, expected) in [
            ((0.0, 30.0), (100.0, 30.0), (8, 60)),
            ((40.0, 0.0), (40.0, 80.0), (80, 8)),
            ((80.0, 30.0), (-20.0, 30.0), (8, 60)),
            ((40.0, 60.0), (40.0, -20.0), (80, 8)),
            ((0.0, 0.0), (100.0, 80.0), (8, 8)),
        ] {
            let (mut e, _) = patterned_canvas();
            drag(&mut e, start, end);
            assert_eq!(e.document().size(), expected, "drag from {start:?}");
        }
    }

    #[test]
    fn canvas_handles_round_fractional_drags_without_spurious_history() {
        let (mut e, _) = patterned_canvas();
        drag(&mut e, (0.0, 30.0), (0.49, 30.0));
        assert_eq!(e.document().size(), (80, 60));
        assert_eq!(e.undo_label(), None);
        assert!(!e.is_modified());
        drag(&mut e, (0.0, 30.0), (0.51, 30.0));
        assert_eq!(e.document().size(), (79, 60));
        assert_eq!(e.undo_label(), Some("Resize canvas"));
    }

    #[test]
    fn canvas_resize_preview_matches_rounded_crop_and_expansion() {
        for (start, end, preview, size) in [
            ((0.0, 30.0), (0.5, 30.0), (1.0, 0.0, 80.0, 60.0), (79, 60)),
            ((0.0, 30.0), (-0.5, 30.0), (-1.0, 0.0, 80.0, 60.0), (81, 60)),
            ((80.0, 30.0), (79.5, 30.0), (0.0, 0.0, 80.0, 60.0), (80, 60)),
            ((0.0, 0.0), (0.5, -0.5), (1.0, -1.0, 80.0, 60.0), (79, 61)),
        ] {
            let (mut e, _) = patterned_canvas();
            e.pointer_down(Pt::new(start.0, start.1), NONE, 4.0);
            e.pointer_move(Pt::new(end.0, end.1), NONE);
            assert_eq!(
                e.marquee(),
                Some(Box2::new(preview.0, preview.1, preview.2, preview.3))
            );
            e.pointer_up(Pt::new(end.0, end.1), NONE);
            assert_eq!(e.document().size(), size);
        }
    }

    #[test]
    fn canvas_handle_geometry_preserves_large_odd_dimensions_on_click() {
        let width = 16_777_217;
        let point = Pt::new(0.0, 0.5);
        let bounds = canvas_drag_bounds(width, 1, H_L, point, point).unwrap();
        assert_eq!(canvas_resize_geometry(bounds), Ok((width, 1, 0, 0)));
    }

    #[test]
    fn canvas_handles_keep_existing_tiny_images_unchanged_on_click() {
        let mut e = Editor::new(Document::blank(1, 1, Rgba::WHITE));
        e.set_tool(Tool::Arrow);
        e.pointer_down(Pt::new(0.5, 0.0), NONE, 4.0);
        e.pointer_up(Pt::new(0.5, 0.0), NONE);
        assert_eq!(e.document().size(), (1, 1));
        assert_eq!(e.undo_label(), None);
        drag(&mut e, (0.5, 0.0), (0.5, -2.0));
        assert_eq!(e.document().size(), (1, 3));
    }

    #[test]
    fn canvas_handles_report_invalid_sizes_without_losing_redo() {
        for end in [-1_000_000.0, f32::NEG_INFINITY, f32::NAN] {
            let (mut e, _) = patterned_canvas();
            drag(&mut e, (80.0, 30.0), (70.0, 30.0));
            assert!(e.undo());
            assert_eq!(e.redo_label(), Some("Resize canvas"));
            e.take_events();
            e.pointer_down(Pt::new(0.0, 30.0), NONE, 4.0);
            e.pointer_up(Pt::new(end, 30.0), NONE);
            assert_eq!(e.document().size(), (80, 60), "invalid end {end}");
            assert_eq!(e.undo_label(), None, "invalid end {end}");
            assert_eq!(e.redo_label(), Some("Resize canvas"), "invalid end {end}");
            assert_eq!(e.marquee(), None, "invalid end {end}");
            assert_eq!(
                e.take_canvas_resize_error(),
                Some(CanvasResizeError::InvalidDimensions),
                "invalid end {end}"
            );
            assert_ne!(
                e.take_events() & EVENT_CANVAS_RESIZE_ERROR,
                0,
                "invalid end {end}"
            );
            assert!(e.redo(), "invalid end {end}");
            assert_eq!(e.document().size(), (70, 60), "invalid end {end}");
        }
    }

    #[test]
    fn successful_canvas_resize_supersedes_a_pending_failure() {
        let (mut e, _) = patterned_canvas();
        drag(&mut e, (0.0, 30.0), (-1_000_000.0, 30.0));
        drag(&mut e, (0.0, 30.0), (10.0, 30.0));
        assert_eq!(e.document().size(), (70, 60));
        assert_eq!((e.display().width(), e.display().height()), (70, 60));
        assert_eq!(e.take_canvas_resize_error(), None);
        let events = e.take_events();
        assert_ne!(events & EVENT_CANVAS_RESIZED, 0);
        assert_ne!(events & EVENT_RESIZED, 0);
    }

    #[test]
    fn canvas_resize_allocation_failures_preserve_document_and_history() {
        for (fail_at, with_redo) in [(0, false), (1, false), (0, true), (1, true)] {
            let (mut e, id) = patterned_canvas();
            if with_redo {
                drag(&mut e, (80.0, 30.0), (70.0, 30.0));
                assert!(e.undo());
            }
            e.take_damage();
            e.take_events();
            let pixels = e.document().background().data().to_vec();
            let display = e.display().data().to_vec();
            let annotation = e.document().get(id).unwrap().rect();
            let modified = e.is_modified();
            let mut attempts = 0;
            e.resize_canvas_from_drag(70, 60, -10, 0, |width, height| {
                let attempt = attempts;
                attempts += 1;
                if attempt == fail_at {
                    Err(CanvasResizeError::AllocationFailed)
                } else {
                    allocate_canvas_pixmap(width, height)
                }
            });
            assert_eq!(attempts, fail_at + 1);
            assert_eq!(
                e.document().size(),
                (80, 60),
                "failure at allocation {fail_at}"
            );
            assert_eq!(
                e.document().background().data(),
                pixels,
                "failure at allocation {fail_at}"
            );
            assert_eq!(
                e.display().data(),
                display,
                "failure at allocation {fail_at}"
            );
            assert_eq!(e.document().get(id).unwrap().rect(), annotation);
            assert_eq!(e.is_modified(), modified);
            assert_eq!(e.undo_label(), None);
            assert_eq!(e.redo_label(), with_redo.then_some("Resize canvas"));
            assert!(e.take_damage().is_none());
            assert_eq!(
                e.take_canvas_resize_error(),
                Some(CanvasResizeError::AllocationFailed)
            );
            assert_ne!(e.take_events() & EVENT_CANVAS_RESIZE_ERROR, 0);
            if with_redo {
                assert!(e.redo());
            }
        }
    }

    #[test]
    fn expand_to_fit_rejects_dimensions_that_documents_cannot_save() {
        let mut doc = Document::blank(10, 10, Rgba::WHITE);
        doc.add(
            Kind::Shape {
                rect: Box2::from_xywh(0.0, 0.0, 8_000.0, 5_000.0),
                shape: ShapeKind::Rectangle,
            },
            Style::default(),
        );
        let mut e = Editor::new(doc);
        assert!(!e.expand_to_fit());
        assert_eq!(e.document().size(), (10, 10));
    }

    #[test]
    fn expand_to_fit_does_not_trim_thin_objects_out_of_the_canvas() {
        let mut doc = Document::blank(200, 100, Rgba::WHITE);
        doc.add(
            Kind::Line {
                a: Pt::new(250.0, 40.0),
                b: Pt::new(250.0, 60.0),
                control: None,
            },
            Style {
                width: 0.0,
                ..Style::default()
            },
        );
        let mut editor = Editor::new(doc);
        assert!(editor.expand_to_fit());
        assert!(editor.document().width() >= 252);
    }

    #[test]
    fn text_steps_styles_and_crop() {
        let mut e = editor();
        e.set_tool(Tool::Text);
        e.pointer_down(Pt::new(10.0, 10.0), NONE, 4.0);
        e.pointer_up(Pt::new(10.0, 10.0), NONE);
        assert!(e.take_events() & EVENT_FOCUS_TEXT != 0);
        e.set_selected_text("Hello");
        e.set_selected_text("Hello there");
        assert_eq!(e.selected().unwrap().text(), Some("Hello there"));
        e.update_style("size", |s| s.font_size = 40.0);
        e.update_style("size", |s| s.font_size = 44.0);
        assert_eq!(e.tool_style(Tool::Text).font_size, 44.0);
        assert_eq!(e.undo.len(), 3, "add, text and size edits coalesce");
        // Leaving an empty text box removes it.
        e.set_tool(Tool::Text);
        e.select(None);
        e.pointer_down(Pt::new(200.0, 200.0), NONE, 4.0);
        e.pointer_up(Pt::new(200.0, 200.0), NONE);
        assert_eq!(e.document().objects.len(), 2);
        e.select(None);
        assert_eq!(e.document().objects.len(), 1);

        e.set_tool(Tool::Step);
        for x in [100.0, 150.0, 200.0] {
            e.select(None);
            e.pointer_down(Pt::new(x, 250.0), NONE, 4.0);
            e.pointer_up(Pt::new(x, 250.0), NONE);
        }
        let labels: Vec<_> = e
            .document()
            .objects
            .iter()
            .filter_map(|o| o.text().filter(|_| matches!(o.kind, Kind::Step { .. })))
            .collect();
        assert_eq!(labels, ["1", "2", "3"]);

        e.set_tool(Tool::Crop);
        drag(&mut e, (5.0, 5.0), (305.0, 205.0));
        assert_eq!(e.document().size(), (300, 200));
        assert!(e.take_events() & EVENT_RESIZED != 0);
        assert!(e.undo());
        assert_eq!(e.document().size(), (400, 300));
    }

    #[test]
    fn handles_resize_and_fill() {
        let mut e = editor();
        e.set_tool(Tool::Shape);
        drag(&mut e, (10.0, 10.0), (110.0, 60.0));
        let id = e.selection().unwrap();
        drag(&mut e, (110.0, 60.0), (210.0, 110.0));
        assert_eq!(
            e.document().get(id).unwrap().rect(),
            Some(Box2::new(10.0, 10.0, 210.0, 110.0))
        );
        e.set_tool(Tool::Fill);
        e.pointer_down(Pt::new(300.0, 250.0), NONE, 4.0);
        e.pointer_up(Pt::new(300.0, 250.0), NONE);
        assert_eq!(
            raster::pixel(e.document().background(), 399, 299),
            Rgba::YELLOW
        );
    }

    fn square(x: f32, color: Rgba) -> (Kind, Style) {
        (
            Kind::Shape {
                rect: Box2::from_xywh(x, 10.0, 30.0, 30.0),
                shape: ShapeKind::Rectangle,
            },
            Style {
                fill: color,
                width: 0.0,
                ..Style::default()
            },
        )
    }

    #[test]
    fn selection_discards_nonadjacent_duplicate_ids() {
        let mut e = editor();
        let (kind, style) = square(10.0, Rgba::RED);
        let a = e.add_object("Add", kind, style);
        let (kind, style) = square(60.0, Rgba::BLACK);
        let b = e.add_object("Add", kind, style);
        e.set_selection(vec![a, b, a, b]);
        assert_eq!(e.selected_ids(), vec![a, b]);
        e.duplicate_selected().unwrap();
        assert_eq!(e.document().objects.len(), 4);
    }

    #[test]
    fn duplicating_multiple_steps_assigns_distinct_next_labels() {
        let mut e = editor();
        let step = |label: &str, x| Kind::Step {
            center: Pt::new(x, 30.0),
            radius: 10.0,
            label: label.into(),
            shape: ShapeKind::Ellipse,
        };
        let first = e.add_object("Add", step("1", 30.0), Style::default());
        let second = e.add_object("Add", step("2", 60.0), Style::default());
        e.set_selection(vec![first, second]);
        e.duplicate_selected().unwrap();
        let labels: Vec<_> = e.document().objects.iter().filter_map(|object| {
            if let Kind::Step { label, .. } = &object.kind { Some(label.as_str()) } else { None }
        }).collect();
        assert_eq!(labels, ["1", "2", "3", "4"]);
    }

    #[test]
    fn duplicating_a_group_assigns_new_labels_to_nested_steps() {
        let mut doc = Document::blank(100, 100, Rgba::WHITE);
        let id = doc.add(
            Kind::Group {
                objects: vec![Object::new(
                    2,
                    Kind::Step {
                        center: Pt::new(20.0, 20.0),
                        radius: 10.0,
                        label: "1".into(),
                        shape: ShapeKind::Ellipse,
                    },
                    Style::default(),
                )],
            },
            Style::default(),
        );
        let mut editor = Editor::new(doc);
        editor.select(Some(id));
        editor.duplicate_selected();

        let mut labels = Vec::<String>::new();
        for object in &editor.document().objects {
            object.walk(&mut |nested| {
                if let Kind::Step { label, .. } = &nested.kind {
                    labels.push(label.clone());
                }
            });
        }
        assert_eq!(labels, ["1", "2"]);
    }

    #[test]
    fn duplicating_a_group_advances_each_lettered_step() {
        let mut doc = Document::blank(100, 100, Rgba::WHITE);
        let step = |id, label: &str| {
            Object::new(
                id,
                Kind::Step {
                    center: Pt::new(20.0, 20.0),
                    radius: 10.0,
                    label: label.into(),
                    shape: ShapeKind::Ellipse,
                },
                Style::default(),
            )
        };
        let id = doc.add(
            Kind::Group {
                objects: vec![step(2, "A"), step(3, "B")],
            },
            Style::default(),
        );
        let mut editor = Editor::new(doc);
        editor.select(Some(id));
        editor.duplicate_selected();

        let mut labels = Vec::<String>::new();
        for object in &editor.document().objects {
            object.walk(&mut |nested| {
                if let Kind::Step { label, .. } = &nested.kind {
                    labels.push(label.clone());
                }
            });
        }
        assert_eq!(labels, ["A", "B", "C", "D"]);
    }

    #[test]
    fn selection_drops_missing_objects_after_history_and_document_edits() {
        let mut e = editor();
        let (kind, style) = square(10.0, Rgba::RED);
        let a = e.add_object("Add", kind, style);
        let (kind, style) = square(60.0, Rgba::BLACK);
        let b = e.add_object("Add", kind, style);
        e.set_selection(vec![b, a]);
        assert!(e.undo());
        assert_eq!(e.selection(), Some(a));
        assert_eq!(e.selected_ids(), vec![a]);
        assert!(e.redo());
        e.set_selection(vec![a, b]);
        assert!(e.edit_document("Remove", |doc| doc.remove(b).is_some()));
        assert_eq!(e.selected_ids(), vec![a]);
    }

    #[test]
    fn editing_after_undo_starts_a_new_coalescing_branch() {
        let mut e = editor();
        let (kind, style) = square(10.0, Rgba::RED);
        let id = e.add_object("Add", kind, style);
        let key = Some("move".to_string());
        e.edit(id, "Move", key.clone(), |o| o.translate(10.0, 0.0));
        e.edit(id, "Other", None, |o| o.translate(20.0, 0.0));
        assert!(e.undo());
        e.edit(id, "Move", key, |o| o.translate(5.0, 0.0));
        assert_eq!(e.redo_label(), None);
        assert!(e.undo());
        assert_eq!(e.document().get(id).unwrap().rect().unwrap().x0, 20.0);
    }

    #[test]
    fn cancelled_creation_keeps_redo_and_saved_state() {
        let mut e = editor();
        let (kind, style) = square(10.0, Rgba::RED);
        e.add_object("Add", kind, style);
        assert!(e.undo());
        e.mark_saved();
        e.set_tool(Tool::Shape);
        e.pointer_down(Pt::new(200.0, 150.0), NONE, 4.0);
        e.pointer_up(Pt::new(200.0, 150.0), NONE);
        assert!(!e.is_modified());
        assert_eq!(e.redo_label(), Some("Add"));
        assert!(e.redo());
        assert_eq!(e.document().objects.len(), 1);
    }

    #[test]
    fn cancelled_creation_preserves_the_oldest_full_history_entry() {
        let mut e = editor();
        for i in 0..HISTORY_LIMIT {
            e.checkpoint(&format!("Edit {i}"), None);
        }
        assert!(e.is_modified());
        e.set_tool(Tool::Shape);
        e.pointer_down(Pt::new(200.0, 150.0), NONE, 4.0);
        e.pointer_up(Pt::new(200.0, 150.0), NONE);
        assert!(e.is_modified());
        assert_eq!(e.undo.len(), HISTORY_LIMIT);
        assert_eq!(e.undo[0].label, "Edit 0");
        assert_eq!(e.undo_label(), Some("Edit 99"));
    }

    #[test]
    fn completed_creation_still_discards_the_redo_branch() {
        let mut e = editor();
        let (kind, style) = square(10.0, Rgba::RED);
        e.add_object("Add", kind, style);
        assert!(e.undo());
        e.set_tool(Tool::Shape);
        drag(&mut e, (200.0, 150.0), (250.0, 180.0));
        assert_eq!(e.redo_label(), None);
        assert_eq!(e.document().objects.len(), 1);
        assert!(e.undo());
        assert!(e.document().objects.is_empty());
    }

    #[test]
    fn failed_document_edit_restores_the_original_document() {
        let mut e = editor();
        e.mark_saved();
        assert!(!e.edit_document("Failed", |doc| {
            raster::set_pixel(doc.background_mut(), 0, 0, Rgba::RED);
            false
        }));
        assert_eq!(raster::pixel(e.document().background(), 0, 0), Rgba::WHITE);
        assert!(!e.is_modified());
        assert_eq!(e.undo_label(), None);
        assert!(e.take_damage().is_none());
    }

    #[test]
    fn ungroup_invalidates_the_previously_rendered_group() {
        let mut e = editor();
        let (kind, style) = square(10.0, Rgba::RED);
        let a = e.add_object("Add", kind, style);
        let (kind, style) = square(20.0, Rgba::BLACK);
        let b = e.add_object("Add", kind, style);
        e.set_selection(vec![a, b]);
        let group = e.group_selected().unwrap();
        e.edit(group, "Opacity", None, |o| o.style.opacity = 0.25);
        e.take_damage();
        let before = e.display().data().to_vec();
        assert!(e.ungroup_selected());
        let (_, expected) = e.document().render_area(e.document().bounds(), true).unwrap();
        assert_ne!(before, expected.data());
        assert!(e.take_damage().is_some());
        assert_eq!(e.display().data(), expected.data());
    }
}

/// Evens out a freehand stroke: a few passes of neighbor averaging
/// (`strength` 1 to 10), ends kept, then points closer than a pixel
/// dropped.
pub fn smooth_stroke(points: &[Pt], strength: f32) -> Vec<Pt> {
    let passes = strength.round().clamp(1.0, 10.0) as usize;
    let mut cur = points.to_vec();
    for _ in 0..passes {
        let mut next = cur.clone();
        for i in 1..cur.len() - 1 {
            let (a, b, c) = (cur[i - 1], cur[i], cur[i + 1]);
            next[i] = Pt::new(
                a.x * 0.25 + b.x * 0.5 + c.x * 0.25,
                a.y * 0.25 + b.y * 0.5 + c.y * 0.25,
            );
        }
        cur = next;
    }
    let mut out = vec![cur[0]];
    for &p in &cur[1..] {
        if out.last().unwrap().dist(p) >= 1.0 {
            out.push(p);
        }
    }
    if out.last() != cur.last() {
        out.push(*cur.last().unwrap());
    }
    out
}
