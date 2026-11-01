//! Headless editable documents backed by the same editor as the desktop UI.

use super::{internal, invalid, not_found, Args, Reply};
use jiffshot_control::{Error, ErrorCode};
use jiffshot_doc::combine::{self, Arrangement};
use jiffshot_doc::editor::{Analyzer, Modifiers, TOOLS};
use jiffshot_doc::object::{BlurMode, Kind, Style};
use jiffshot_doc::{effects, io, smart, Box2, Document, Editor, IRect, Pixmap, Pt, Rgba, Tool};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Families substituted while rendering the document's text; macOS only.
fn font_substitutions(doc: &Document) -> Value {
    #[cfg(target_os = "macos")]
    return json!(doc.font_substitutions());
    #[cfg(not(target_os = "macos"))]
    {
        let _ = doc;
        json!([])
    }
}

fn object_font_substitutions<'a>(
    objects: impl IntoIterator<Item = &'a jiffshot_doc::object::Object>,
) -> Value {
    #[cfg(target_os = "macos")]
    return json!(jiffshot_doc::text::object_font_substitutions(objects));
    #[cfg(not(target_os = "macos"))]
    {
        let _ = objects;
        json!([])
    }
}

const MAX_DOCUMENTS: usize = 8;
const MAX_OBJECTS: usize = 4096;
const MAX_POINTS: usize = 10_000;
const MAX_PIXELS: u64 = 64_000_000;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static DOCUMENTS: Mutex<BTreeMap<String, Session>> = Mutex::new(BTreeMap::new());

struct Session {
    editor: Editor,
    path: Option<PathBuf>,
}

fn dimensions(width: u32, height: u32) -> Result<(), Error> {
    if width == 0
        || height == 0
        || width > 16_384
        || height > 16_384
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err(invalid(
            "canvas must be 1..16384 pixels per side and at most 64 million pixels",
        ));
    }
    Ok(())
}

fn color(a: &Args, key: &str, default: Rgba) -> Result<Rgba, Error> {
    match a.0.get(key) {
        None => Ok(default),
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| invalid(format!("{key}: {e}; use #RRGGBB or #AARRGGBB"))),
    }
}

fn required_path(a: &Args, key: &str, extensions: &[&str]) -> Result<PathBuf, Error> {
    a.out(key, extensions)?
        .ok_or_else(|| invalid(format!("{key} is required")))
}

/// Bounds serialized geometry and arrays before calling the raster engine.
pub(super) fn validate_json(v: &Value) -> Result<(), Error> {
    match v {
        Value::Number(n)
            if n.as_f64()
                .is_none_or(|n| !n.is_finite() || n.abs() > 1_000_000.0) =>
        {
            Err(invalid(
                "annotation numbers must be finite and within +/-1000000",
            ))
        }
        Value::Array(items) => {
            if items.len() > MAX_POINTS {
                return Err(invalid("annotation arrays are limited to 10000 entries"));
            }
            for item in items {
                validate_json(item)?;
            }
            Ok(())
        }
        Value::Object(fields) => {
            for item in fields.values() {
                validate_json(item)?;
            }
            Ok(())
        }
        Value::String(s) if s.len() > 65_536 => {
            Err(invalid("annotation text is limited to 65536 bytes"))
        }
        _ => Ok(()),
    }
}

pub(super) fn style(value: Option<&Value>, base: Style) -> Result<Style, Error> {
    let Some(value) = value else { return Ok(base) };
    validate_json(value)?;
    let fields = value
        .as_object()
        .ok_or_else(|| invalid("style must be an object"))?;
    let mut merged = serde_json::to_value(base).map_err(internal)?;
    let map = merged
        .as_object_mut()
        .ok_or_else(|| internal("style is not an object"))?;
    for (key, value) in fields {
        if !map.contains_key(key) {
            return Err(invalid(format!("unknown style property {key:?}")));
        }
        map.insert(key.clone(), value.clone());
    }
    let style: Style =
        serde_json::from_value(merged).map_err(|e| invalid(format!("style: {e}")))?;
    if !(0.0..=1.0).contains(&style.opacity)
        || !(0.0..=512.0).contains(&style.width)
        || !(1.0..=512.0).contains(&style.font_size)
        || !(0.0..=512.0).contains(&style.shadow_blur)
        || !(0.0..=512.0).contains(&style.outline_width)
        || !(0.0..=512.0).contains(&style.shadow_distance)
        || !(0.0..=16.0).contains(&style.head_size)
    {
        return Err(invalid("style opacity must be 0..1; width, shadow_blur, outline_width 0..512; font_size 1..512"));
    }
    Ok(style)
}

pub(super) fn kind(value: &Value) -> Result<Kind, Error> {
    validate_json(value)?;
    let kind: Kind =
        serde_json::from_value(value.clone()).map_err(|e| invalid(format!("kind: {e}")))?;
    validate_kind(&kind, 0)?;
    Ok(kind)
}

fn validate_kind(kind: &Kind, depth: usize) -> Result<(), Error> {
    if depth > 8 {
        return Err(invalid("groups are limited to eight nesting levels"));
    }
    let rect = match kind {
        Kind::Shape { rect, .. }
        | Kind::Text { rect, .. }
        | Kind::Callout { rect, .. }
        | Kind::Highlight { rect, .. }
        | Kind::Blur { rect, .. }
        | Kind::Magnify { rect, .. }
        | Kind::Spotlight { rect, .. }
        | Kind::Stamp { rect, .. }
        | Kind::Image { rect, .. } => Some(rect),
        _ => None,
    };
    if rect.is_some_and(|r| r.width() <= 0.0 || r.height() <= 0.0) {
        return Err(invalid(
            "annotation rectangles must have x1 > x0 and y1 > y0",
        ));
    }
    match kind {
        Kind::Image { .. } => return Err(invalid("use doc.image to load image pixels")),
        Kind::Group { objects } => {
            if objects.len() > MAX_OBJECTS {
                return Err(invalid("too many grouped objects"));
            }
            for object in objects {
                validate_kind(&object.kind, depth + 1)?;
                style(
                    Some(&serde_json::to_value(&object.style).map_err(internal)?),
                    Style::default(),
                )?;
            }
        }
        Kind::Blur { amount, .. } if !(0.0..=128.0).contains(amount) => {
            return Err(invalid("blur amount must be 0..128"))
        }
        Kind::Magnify { zoom, .. } if !(1.0..=16.0).contains(zoom) => {
            return Err(invalid("magnification must be 1..16"))
        }
        Kind::Spotlight { dim, blur, .. }
            if !(0.0..=1.0).contains(dim) || !(0.0..=128.0).contains(blur) =>
        {
            return Err(invalid("spotlight dim must be 0..1 and blur 0..128"))
        }
        Kind::Step { radius, .. } if !(1.0..=512.0).contains(radius) => {
            return Err(invalid("step radius must be 1..512"))
        }
        Kind::Pen { points } | Kind::Polygon { points } if points.len() < 2 => {
            return Err(invalid("a path needs at least two points"))
        }
        _ => {}
    }
    Ok(())
}

fn info(id: &str, session: &Session) -> Value {
    let e = &session.editor;
    json!({
        "id": id, "path": session.path, "width": e.document().width(), "height": e.document().height(),
        "modified": e.is_modified(), "objects": e.document().objects, "selected": e.selected_ids(),
        "undo": e.undo_label(), "redo": e.redo_label(),
        "font_substitutions": font_substitutions(e.document()),
    })
}

fn recognize(image: &Pixmap) -> Result<Vec<smart::TextLine>, Error> {
    let mut pixels = Vec::with_capacity(image.data().len());
    for p in image.pixels() {
        let white = 255 - u16::from(p.alpha());
        let channel = |v: u8| (u16::from(v) + white).min(255) as u8;
        pixels.extend_from_slice(&[channel(p.blue()), channel(p.green()), channel(p.red()), 255]);
    }
    Ok(
        jiffshot_capture::ocr::recognize(image.width(), image.height(), &pixels)
            .map_err(internal)?
            .into_iter()
            .map(|l| smart::TextLine {
                area: IRect::new(l.rect.x, l.rect.y, l.rect.width, l.rect.height),
                text: l.text,
                words: l
                    .words
                    .into_iter()
                    .map(|w| {
                        (
                            IRect::new(w.rect.x, w.rect.y, w.rect.width, w.rect.height),
                            w.text,
                        )
                    })
                    .collect(),
            })
            .collect(),
    )
}

struct RecognizedLines(Vec<smart::TextLine>);

impl Analyzer for RecognizedLines {
    fn text_in(&mut self, _image: &Pixmap, _area: IRect) -> Vec<smart::TextLine> {
        std::mem::take(&mut self.0)
    }
}

fn insert(doc: Document, path: Option<PathBuf>) -> Reply {
    dimensions(doc.width(), doc.height())?;
    let mut sessions = DOCUMENTS.lock().expect("document sessions lock poisoned");
    if sessions.len() >= MAX_DOCUMENTS {
        return Err(Error::new(
            ErrorCode::Conflict,
            "eight documents are open; close one with doc.close",
        ));
    }
    let id = format!("doc-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let session = Session {
        editor: Editor::new(doc),
        path,
    };
    let result = info(&id, &session);
    sessions.insert(id, session);
    Ok(result)
}

pub(super) fn dispatch(command: &str, a: &Args) -> Reply {
    match command {
        "doc.tools" => {
            a.only(&[])?;
            return Ok(json!({
                "tools": TOOLS.iter().map(|t| json!({"name": t.name(), "variants": t.variants(), "style": jiffshot_doc::editor::default_style(*t)})).collect::<Vec<_>>(),
                "effects": effects::catalog(Rgba::BLACK).iter().map(|(n, _)| *n).collect::<Vec<_>>(),
                "formats": ["jsd", "png", "jpg", "bmp", "gif", "webp", "pdf"],
                "coordinates": "document pixels",
                "templates": combine::TEMPLATES,
                "themes": combine::THEMES.iter().map(|t| t.name).collect::<Vec<_>>(),
            }));
        }
        "doc.list" => {
            a.only(&[])?;
            let sessions = DOCUMENTS.lock().expect("document sessions lock poisoned");
            return Ok(
                json!({"documents": sessions.iter().map(|(id, s)| info(id, s)).collect::<Vec<_>>()}),
            );
        }
        "doc.new" => {
            a.only(&["width", "height", "color"])?;
            let w = a
                .int("width", 1, 16_384)?
                .ok_or_else(|| invalid("width is required"))? as u32;
            let h = a
                .int("height", 1, 16_384)?
                .ok_or_else(|| invalid("height is required"))? as u32;
            dimensions(w, h)?;
            return insert(Document::blank(w, h, color(a, "color", Rgba::WHITE)?), None);
        }
        "doc.open" => {
            a.only(&["path"])?;
            let path = required_path(a, "path", io::OPEN_EXTENSIONS)?;
            return insert(io::open(&path).map_err(internal)?, Some(path));
        }
        "doc.combine" => return combine_documents(a),
        _ => {}
    }
    let id = a
        .string("id")?
        .ok_or_else(|| invalid("id is required (returned by doc.new or doc.open)"))?;
    let mut sessions = DOCUMENTS.lock().expect("document sessions lock poisoned");
    if command == "doc.close" {
        a.only(&["id"])?;
        sessions
            .remove(&id)
            .ok_or_else(|| not_found(format!("no document {id}")))?;
        return Ok(json!({"id": id, "closed": true}));
    }
    let session = sessions
        .get_mut(&id)
        .ok_or_else(|| not_found(format!("no document {id}")))?;
    let e = &mut session.editor;
    match command {
        "doc.status" => a.only(&["id"])?,
        "doc.add" => {
            a.only(&["id", "kind", "style"])?;
            if e.document().objects.len() >= MAX_OBJECTS {
                return Err(invalid("document has 4096 objects"));
            }
            let k = kind(a.0.get("kind").ok_or_else(|| invalid("kind is required"))?)?;
            let st = style(a.0.get("style"), Style::default())?;
            let object_id = e.add_object("Agent annotation", k, st);
            return Ok(
                json!({"id": id, "object_id": object_id, "font_substitutions": font_substitutions(e.document())}),
            );
        }
        "doc.update" => {
            a.only(&["id", "object_id", "kind", "style", "rotation"])?;
            let object_id =
                a.int("object_id", 1, i64::MAX)?
                    .ok_or_else(|| invalid("object_id is required"))? as u64;
            let old = e
                .document()
                .get(object_id)
                .ok_or_else(|| not_found("no such object"))?;
            let kind =
                a.0.get("kind")
                    .map(kind)
                    .transpose()?
                    .unwrap_or_else(|| old.kind.clone());
            let style = style(a.0.get("style"), old.style.clone())?;
            let rotation = a.number("rotation")?.unwrap_or(f64::from(old.rotation));
            if !rotation.is_finite() || rotation.abs() > 3600.0 {
                return Err(invalid(
                    "rotation must be finite and within +/-3600 degrees",
                ));
            }
            e.edit(object_id, "Agent edit", None, |o| {
                o.kind = kind;
                o.style = style;
                o.rotation = rotation as f32;
            });
        }
        "doc.draw" => {
            draw(e, a)?;
            let picked = e.picked_color();
            let mut result = info(&id, session);
            result["picked_color"] = json!(picked);
            return Ok(result);
        }
        "doc.ocr" => {
            a.only(&["id"])?;
            let lines = recognize(&e.document().flatten())?;
            return Ok(
                json!({"id":id,"lines":lines.iter().map(|l| json!({"text":l.text,"rect":l.area})).collect::<Vec<_>>(),
                    "font_substitutions": font_substitutions(e.document())}),
            );
        }
        "doc.simplify" => {
            a.only(&["id", "colors"])?;
            let colors = a.int("colors", 2, 64)?.unwrap_or(8) as usize;
            let lines = recognize(e.document().background())?;
            if lines.len() + e.document().objects.len() > MAX_OBJECTS {
                return Err(invalid("simplify would exceed 4096 objects"));
            }
            e.edit_document("Simplify", |d| {
                smart::simplify(d, &lines, colors);
                true
            });
        }
        "doc.redact" => {
            a.only(&["id"])?;
            let lines = recognize(e.document().background())?;
            let areas = jiffshot_doc::redact::areas(&lines);
            if areas.len() + e.document().objects.len() > MAX_OBJECTS {
                return Err(invalid("redact would exceed 4096 objects"));
            }
            let count = areas.len();
            if count != 0 {
                e.edit_document("Smart redact", |d| {
                    for (r, _) in areas {
                        d.add(
                            Kind::Blur {
                                rect: r.to_box(),
                                mode: BlurMode::Redact,
                                amount: 0.0,
                            },
                            Style {
                                fill: Rgba::BLACK,
                                width: 0.0,
                                ..Style::default()
                            },
                        );
                    }
                    true
                });
            }
            let mut result = info(&id, session);
            result["redactions"] = json!(count);
            return Ok(result);
        }
        "doc.image" => {
            a.only(&["id", "path", "at"])?;
            if e.document().objects.len() >= MAX_OBJECTS {
                return Err(invalid("document has 4096 objects"));
            }
            let path = required_path(a, "path", &["png", "jpg", "jpeg", "bmp", "gif", "webp"])?;
            let pm = io::load_image(&path).map_err(internal)?;
            dimensions(pm.width(), pm.height())?;
            let (x, y) = a.point("at")?.unwrap_or((0, 0));
            e.drop_image(Pt::new(x as f32, y as f32), pm);
        }
        "doc.select" => {
            a.only(&["id", "object_ids"])?;
            let ids: Vec<u64> = serde_json::from_value(
                a.0.get("object_ids")
                    .cloned()
                    .ok_or_else(|| invalid("object_ids array is required"))?,
            )
            .map_err(|e| invalid(e.to_string()))?;
            if ids.iter().any(|id| e.document().get(*id).is_none()) {
                return Err(not_found("selection contains an unknown object"));
            }
            e.set_selection(ids);
        }
        "doc.action" => {
            if a.string("name")?.as_deref() == Some("flatten") {
                let selected = e.selected_ids();
                let substitutions = object_font_substitutions(
                    e.document()
                        .objects
                        .iter()
                        .filter(|object| selected.contains(&object.id)),
                );
                action(e, a)?;
                let mut result = info(&id, session);
                result["flattened_font_substitutions"] = json!(substitutions);
                return Ok(result);
            }
            action(e, a)?;
        }
        "doc.crop" => {
            a.only(&["id", "x", "y", "width", "height"])?;
            let x = a
                .int("x", 0, i64::from(e.document().width()) - 1)?
                .unwrap_or(0) as i32;
            let y = a
                .int("y", 0, i64::from(e.document().height()) - 1)?
                .unwrap_or(0) as i32;
            let w = a
                .int("width", 1, i64::from(e.document().width()) - i64::from(x))?
                .ok_or_else(|| invalid("width is required"))? as i32;
            let h = a
                .int("height", 1, i64::from(e.document().height()) - i64::from(y))?
                .ok_or_else(|| invalid("height is required"))? as i32;
            e.edit_document("Crop", |d| d.crop(IRect::new(x, y, w, h)));
        }
        "doc.resize" => {
            a.only(&["id", "width", "height", "canvas", "x", "y", "color"])?;
            let w = a
                .int("width", 1, 16_384)?
                .ok_or_else(|| invalid("width is required"))? as u32;
            let h = a
                .int("height", 1, 16_384)?
                .ok_or_else(|| invalid("height is required"))? as u32;
            dimensions(w, h)?;
            let canvas = a.bool("canvas")?;
            let x = a.int("x", -16_384, 16_384)?.unwrap_or(0) as i32;
            let y = a.int("y", -16_384, 16_384)?.unwrap_or(0) as i32;
            let fill = color(a, "color", Rgba::TRANSPARENT)?;
            e.edit_document("Resize", |d| {
                if canvas {
                    d.resize_canvas(w, h, x, y, fill);
                } else {
                    d.scale(w, h);
                }
                true
            });
        }
        "doc.effect" => {
            a.only(&["id", "name", "color", "fill"])?;
            let name = a
                .string("name")?
                .ok_or_else(|| invalid("name is required; doc.tools lists effects"))?;
            let effect = effects::by_name_for(
                &name,
                color(a, "color", Rgba::BLACK)?,
                color(a, "fill", Rgba::WHITE)?,
            )
            .ok_or_else(|| invalid("unknown effect; doc.tools lists effects"))?;
            let (w, h) = e.document().size();
            let (w, h) = match &effect {
                effects::Effect::Border { width, .. } => (w + 2 * width, h + 2 * width),
                effects::Effect::Shadow { size } => {
                    let margin = (size / 2 + 1) * 2 + size;
                    (w + margin, h + margin)
                }
                effects::Effect::Reflection => (w, h + (h as f32 * 0.35) as u32),
                _ => (w, h),
            };
            dimensions(w, h)?;
            e.edit_document(&name, |d| effects::apply(d, effect));
        }
        "doc.save" | "doc.export" => {
            a.only(&["id", "out"])?;
            let extensions: &[&str] = if command == "doc.save" {
                &["jsd"]
            } else {
                &["png", "jpg", "jpeg", "bmp", "gif", "webp", "pdf"]
            };
            let path = required_path(a, "out", extensions)?;
            if command == "doc.save" {
                io::save_document(e.document(), &path).map_err(internal)?;
                e.mark_saved();
                session.path = Some(path.clone());
            } else {
                io::export(e.document(), &path).map_err(internal)?;
            }
            return Ok(
                json!({"id": id, "path": path, "width": e.document().width(), "height": e.document().height(),
                    "size_bytes": std::fs::metadata(&path).map_err(internal)?.len(), "font_substitutions": font_substitutions(e.document())}),
            );
        }
        _ => {
            return Err(Error::new(
                ErrorCode::Unsupported,
                format!("unknown document command {command}"),
            ))
        }
    }
    Ok(info(&id, session))
}

fn combine_documents(a: &Args) -> Reply {
    a.only(&[
        "document_ids",
        "layout",
        "gap",
        "color",
        "template",
        "theme",
        "captions",
        "numbers",
        "footer",
    ])?;
    let ids: Vec<String> = serde_json::from_value(
        a.0.get("document_ids")
            .cloned()
            .ok_or_else(|| invalid("document_ids array is required"))?,
    )
    .map_err(|e| invalid(e.to_string()))?;
    if ids.is_empty() || ids.len() > MAX_DOCUMENTS {
        return Err(invalid("combine needs 1..8 document ids"));
    }
    let (images, source_substitutions) = {
        let sessions = DOCUMENTS.lock().expect("document sessions lock poisoned");
        let mut images = Vec::with_capacity(ids.len());
        let mut substitutions = Vec::new();
        for id in &ids {
            let session = sessions
                .get(id)
                .ok_or_else(|| not_found(format!("no document {id}")))?;
            let doc = session.editor.document();
            let fonts = font_substitutions(doc);
            if fonts.as_array().is_some_and(|fonts| !fonts.is_empty()) {
                substitutions.push(json!({"id": id, "font_substitutions": fonts}));
            }
            images.push(doc.flatten());
        }
        (images, substitutions)
    };
    let doc = if let Some(template) = a.string("template")? {
        if !combine::TEMPLATES.contains(&template.as_str()) {
            return Err(invalid("unknown template; doc.tools lists templates"));
        }
        let theme_name = a.string("theme")?.unwrap_or_else(|| "Light".into());
        let theme = combine::THEMES
            .iter()
            .find(|t| t.name == theme_name)
            .ok_or_else(|| invalid("unknown theme; doc.tools lists themes"))?;
        let options = combine::Options {
            theme,
            captions: a.bool("captions")?,
            numbers: a.bool("numbers")?,
            footer: a.string("footer")?,
        };
        combine::template_with(&template, &images, &options)
            .ok_or_else(|| invalid("cannot create template"))?
    } else {
        let layout = a.string("layout")?.unwrap_or_else(|| "vertical".into());
        let arrangement = match layout.as_str() {
            "vertical" => Arrangement::Vertical,
            "horizontal" => Arrangement::Horizontal,
            "grid" => Arrangement::Grid,
            _ => return Err(invalid("layout must be vertical, horizontal or grid")),
        };
        let gap = a.int("gap", 0, 512)?.unwrap_or(16) as f32;
        // Check the layout before the shared engine allocates its raster.
        let cols = match arrangement {
            Arrangement::Vertical => 1,
            Arrangement::Horizontal => images.len(),
            Arrangement::Grid => (images.len() as f32).sqrt().ceil() as usize,
        };
        let rows = images.len().div_ceil(cols);
        let width = (0..cols)
            .map(|c| {
                images
                    .iter()
                    .skip(c)
                    .step_by(cols)
                    .map(Pixmap::width)
                    .max()
                    .unwrap_or(0)
            })
            .sum::<u32>()
            + gap as u32 * (cols as u32 + 1);
        let height = (0..rows)
            .map(|r| {
                images
                    .iter()
                    .skip(r * cols)
                    .take(cols)
                    .map(Pixmap::height)
                    .max()
                    .unwrap_or(0)
            })
            .sum::<u32>()
            + gap as u32 * (rows as u32 + 1);
        dimensions(width, height)?;
        combine::combine(&images, arrangement, gap, color(a, "color", Rgba::WHITE)?)
            .ok_or_else(|| invalid("cannot combine images"))?
    };
    let mut result = insert(doc, None)?;
    result["source_font_substitutions"] = json!(source_substitutions);
    Ok(result)
}

fn draw(e: &mut Editor, a: &Args) -> Result<(), Error> {
    a.only(&[
        "id", "tool", "variant", "from", "to", "points", "text", "style", "amount", "shift",
        "ctrl", "alt",
    ])?;
    if e.document().objects.len() >= MAX_OBJECTS {
        return Err(invalid("document has 4096 objects"));
    }
    let name = a
        .string("tool")?
        .ok_or_else(|| invalid("tool is required; doc.tools lists tools"))?;
    let tool = Tool::from_name(&name).ok_or_else(|| invalid("unknown tool"))?;
    let st = style(a.0.get("style"), jiffshot_doc::editor::default_style(tool))?;
    let variant = a.string("variant")?;
    if variant
        .as_ref()
        .is_some_and(|v| !tool.variants().contains(&v.as_str()))
    {
        return Err(invalid("unknown variant; doc.tools lists variants"));
    }
    let points: Vec<Pt> = if let Some(v) = a.0.get("points") {
        validate_json(v)?;
        serde_json::from_value(v.clone()).map_err(|e| invalid(format!("points: {e}")))?
    } else {
        let from = a
            .point("from")?
            .ok_or_else(|| invalid("from or points is required"))?;
        let to = a.point("to")?.unwrap_or(from);
        vec![
            Pt::new(from.0 as f32, from.1 as f32),
            Pt::new(to.0 as f32, to.1 as f32),
        ]
    };
    if points.is_empty()
        || points.len() > MAX_POINTS
        || points.iter().any(|p| {
            !p.x.is_finite() || !p.y.is_finite() || p.x.abs() > 16_384.0 || p.y.abs() > 16_384.0
        })
    {
        return Err(invalid(
            "provide 1..10000 finite points within +/-16384 document pixels",
        ));
    }
    let amount = a.number("amount")?;
    if amount.is_some_and(|n| !n.is_finite() || !(0.0..=128.0).contains(&n)) {
        return Err(invalid("amount must be 0..128"));
    }
    let text = a.string("text")?;
    if text.as_ref().is_some_and(|s| s.len() > 65_536) {
        return Err(invalid("annotation text is limited to 65536 bytes"));
    }
    if tool == Tool::TextReplace {
        let rect = Box2::from_points(
            points[0],
            *points.last().ok_or_else(|| invalid("no points"))?,
        )
        .to_irect();
        let rect = rect
            .intersect(&e.document().bounds())
            .ok_or_else(|| invalid("text replacement is outside the document"))?;
        let image = jiffshot_doc::raster::extract(e.document().background(), rect)
            .ok_or_else(|| internal("could not extract text area"))?;
        let mut lines = recognize(&image)?;
        if lines.is_empty() {
            return Err(not_found("no text recognized in the selected area"));
        }
        for line in &mut lines {
            line.area.x += rect.x;
            line.area.y += rect.y;
            for (area, _) in &mut line.words {
                area.x += rect.x;
                area.y += rect.y;
            }
        }
        e.analyzer = Some(Box::new(RecognizedLines(lines)));
    }
    let m = Modifiers {
        shift: a.bool("shift")?,
        ctrl: a.bool("ctrl")?,
        alt: a.bool("alt")?,
    };
    e.select(None);
    e.set_tool(tool);
    e.set_tool_style(tool, st);
    if let Some(variant) = variant {
        e.set_variant(&variant);
    }
    if let Some(amount) = amount {
        e.set_tool_amount(tool, amount as f32);
    }
    e.pointer_down_new(points[0], m);
    let polygon = matches!(tool, Tool::Shape | Tool::Highlight) && e.variant(tool) == "Polygon";
    for p in &points[1..] {
        e.pointer_move(*p, m);
        if polygon {
            e.pointer_down_new(*p, m);
        }
    }
    e.pointer_up(*points.last().ok_or_else(|| invalid("no points"))?, m);
    e.finish_polygon();
    e.analyzer = None;
    if let Some(text) = text {
        e.set_selected_text(&text);
    }
    Ok(())
}

fn action(e: &mut Editor, a: &Args) -> Result<(), Error> {
    a.only(&["id", "name", "text", "x", "y", "amount"])?;
    let name = a
        .string("name")?
        .ok_or_else(|| invalid("name is required"))?;
    let x = a.int("x", -16_384, 16_384)?.unwrap_or(0) as f32;
    let y = a.int("y", -16_384, 16_384)?.unwrap_or(0) as f32;
    let text = a.string("text")?;
    match name.as_str() {
        "undo" => if !e.undo() { return Err(not_found("nothing to undo")); },
        "redo" => if !e.redo() { return Err(not_found("nothing to redo")); },
        "select-all" => e.select_all(),
        "invert-selection" => e.invert_selection(),
        "delete" => if !e.delete_selected() { return Err(not_found("nothing selected")); },
        "duplicate" => { e.duplicate_selected().ok_or_else(|| not_found("nothing selected"))?; }
        "group" => { e.group_selected().ok_or_else(|| invalid("select at least two objects"))?; }
        "ungroup" => if !e.ungroup_selected() { return Err(invalid("select a group")); },
        "flatten" => if !e.flatten_selected() { return Err(not_found("nothing selected")); },
        "move" => { require_selection(e)?; e.nudge(x, y); }
        "text" => { require_selection(e)?; e.set_selected_text(text.as_deref().ok_or_else(|| invalid("text is required"))?); }
        "front" => { require_selection(e)?; e.arrange(2); }
        "back" => { require_selection(e)?; e.arrange(-2); }
        "forward" => { require_selection(e)?; e.arrange(1); }
        "backward" => { require_selection(e)?; e.arrange(-1); }
        "rotate-right" | "rotate-left" => { e.edit_document(&name, |d| { d.rotate(name == "rotate-right"); true }); }
        "flip-horizontal" | "flip-vertical" => { e.edit_document(&name, |d| { d.flip(name == "flip-horizontal"); true }); }
        "align-left" | "align-right" | "align-top" | "align-bottom" | "align-center" | "align-middle" => { require_selection(e)?; e.align_selected(name.strip_prefix("align-").unwrap_or(&name)); }
        _ => return Err(invalid("unknown action: undo, redo, select-all, invert-selection, delete, duplicate, group, ungroup, flatten, move, text, front, back, forward, backward, rotate-right, rotate-left, flip-horizontal, flip-vertical, align-left/right/top/bottom/center/middle")),
    }
    Ok(())
}

fn require_selection(e: &Editor) -> Result<(), Error> {
    if e.selected_ids().is_empty() {
        Err(not_found("nothing selected"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(value: Value) -> Args {
        Args::new(value).expect("test arguments")
    }

    #[test]
    fn annotations_are_editable_and_undoable() {
        let mut e = Editor::new(Document::blank(100, 80, Rgba::WHITE));
        draw(
            &mut e,
            &args(json!({"tool":"Arrow","from":"10,10","to":"60,40","style":{"stroke":"#0000FF"}})),
        )
        .expect("draw arrow");
        assert_eq!(e.document().objects.len(), 1);
        assert!(matches!(e.document().objects[0].kind, Kind::Line { .. }));
        assert_eq!(e.document().objects[0].style.stroke, Rgba::rgb(0, 0, 255));
        action(&mut e, &args(json!({"name":"undo"}))).expect("undo");
        assert!(e.document().objects.is_empty());
        action(&mut e, &args(json!({"name":"redo"}))).expect("redo");
        assert_eq!(e.document().objects.len(), 1);
        assert_ne!(
            e.document().flatten().data(),
            e.document().background().data()
        );
    }

    #[test]
    fn invalid_geometry_and_style_are_rejected_before_mutation() {
        let mut e = Editor::new(Document::blank(100, 80, Rgba::WHITE));
        assert!(draw(&mut e, &args(json!({"tool":"Arrow","points":[]}))).is_err());
        assert!(draw(
            &mut e,
            &args(json!({"tool":"Arrow","from":"1,1","style":{"opacity":2}}))
        )
        .is_err());
        assert!(kind(&json!({"type":"Blur","rect":{"x0":0,"y0":0,"x1":10,"y1":10},"mode":"Blur","amount":1000})).is_err());
        assert!(dimensions(16_384, 16_384).is_err());
        assert!(e.document().objects.is_empty());
    }

    #[test]
    fn overlapping_annotations_create_new_objects() {
        let mut e = Editor::new(Document::blank(100, 80, Rgba::WHITE));
        let request =
            args(json!({"tool":"Shape","from":"10,10","to":"60,40","style":{"fill":"#FF0000"}}));
        draw(&mut e, &request).expect("first shape");
        draw(&mut e, &request).expect("overlapping shape");
        assert_eq!(e.document().objects.len(), 2);
        assert_ne!(e.document().objects[0].id, e.document().objects[1].id);
    }

    #[test]
    fn every_annotation_tool_is_available_through_draw() {
        for (tool, kind_name) in [
            ("Arrow", "Line"),
            ("Line", "Line"),
            ("Shape", "Shape"),
            ("Text", "Text"),
            ("Callout", "Callout"),
            ("Step", "Step"),
            ("Pen", "Pen"),
            ("Highlight", "Highlight"),
            ("Blur", "Blur"),
            ("Magnify", "Magnify"),
            ("Spotlight", "Spotlight"),
            ("Stamp", "Stamp"),
        ] {
            let mut e = Editor::new(Document::blank(200, 150, Rgba::WHITE));
            draw(
                &mut e,
                &args(json!({"tool":tool,"from":"30,30","to":"120,100","text":"Demo"})),
            )
            .expect(tool);
            assert_eq!(e.document().objects.len(), 1, "{tool}");
            let kind = serde_json::to_value(&e.document().objects[0].kind).expect("serialize kind");
            assert_eq!(kind["type"], kind_name, "{tool}");
        }
    }

    #[test]
    fn raster_tools_and_transforms_change_the_document() {
        let mut e = Editor::new(Document::blank(100, 80, Rgba::WHITE));
        draw(
            &mut e,
            &args(json!({"tool":"Fill","from":"10,10","style":{"fill":"#123456"}})),
        )
        .expect("fill");
        assert_eq!(
            jiffshot_doc::raster::pixel(e.document().background(), 50, 50),
            Rgba::rgb(0x12, 0x34, 0x56)
        );
        draw(
            &mut e,
            &args(json!({"tool":"Eraser","from":"20,20","to":"40,20"})),
        )
        .expect("erase");
        assert_eq!(
            jiffshot_doc::raster::pixel(e.document().background(), 30, 20).a,
            0
        );
        draw(
            &mut e,
            &args(json!({"tool":"Crop","from":"10,10","to":"60,50"})),
        )
        .expect("crop");
        assert_eq!(e.document().size(), (50, 40));
        action(&mut e, &args(json!({"name":"rotate-right"}))).expect("rotate");
        assert_eq!(e.document().size(), (40, 50));
    }

    #[test]
    fn document_roundtrip_preserves_annotations_and_export_pixels() {
        let mut e = Editor::new(Document::blank(120, 90, Rgba::WHITE));
        draw(
            &mut e,
            &args(json!({"tool":"Text","from":"10,10","to":"110,60","text":"Demo"})),
        )
        .expect("draw text");
        let bytes = io::document_bytes(e.document()).expect("serialize");
        let loaded = io::document_from_bytes(bytes).expect("load");
        assert_eq!(loaded.objects, e.document().objects);
        assert_eq!(loaded.flatten().data(), e.document().flatten().data());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn document_commands_report_substitutions_before_rasterization() {
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).unwrap();
            }
        }
        let root = Scratch(std::env::temp_dir().join(format!(
            "jiffshot-font-control-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        )));
        std::fs::create_dir(&root.0).unwrap();
        let requested = "Jiffshot deliberately unavailable control face 86c8bc";
        let other = "Jiffshot deliberately unavailable second face 6f2a0a";
        let created = dispatch("doc.new", &args(json!({"width":120,"height":80}))).unwrap();
        let id = created["id"].as_str().unwrap();
        assert_eq!(created["font_substitutions"], json!([]));

        let first = dispatch(
            "doc.add",
            &args(json!({
                "id":id,
                "kind":{"type":"Text","rect":{"x0":2,"y0":2,"x1":110,"y1":35},"text":"First"},
                "style":{"font":requested,"italic":true}
            })),
        )
        .unwrap();
        let first_id = first["object_id"].as_u64().unwrap();
        let first_warning = serde_json::to_value(
            jiffshot_doc::text::font_substitution(requested, false, true).unwrap(),
        )
        .unwrap();
        assert_eq!(first_warning["reason"], "family");
        assert_eq!(first_warning["replacement"]["family"], "Helvetica Neue");
        assert_eq!(first["font_substitutions"], json!([first_warning]));
        let second = dispatch(
            "doc.add",
            &args(json!({
                "id":id,
                "kind":{"type":"Text","rect":{"x0":2,"y0":40,"x1":110,"y1":75},"text":"Second"},
                "style":{"font":other,"bold":true}
            })),
        )
        .unwrap();
        let second_warning = serde_json::to_value(
            jiffshot_doc::text::font_substitution(other, true, false).unwrap(),
        )
        .unwrap();
        assert_eq!(second_warning["reason"], "family");
        assert_eq!(second_warning["replacement"]["family"], "Helvetica Neue");
        assert_eq!(
            second["font_substitutions"],
            json!([first_warning, second_warning])
        );

        let saved_path = root.0.join("edited.jsd");
        let export_path = root.0.join("export.png");
        for (command, path) in [("doc.save", &saved_path), ("doc.export", &export_path)] {
            let result = dispatch(command, &args(json!({"id":id,"out":path}))).unwrap();
            assert_eq!(
                result["font_substitutions"],
                json!([first_warning, second_warning])
            );
        }
        let loaded = io::load_document(&saved_path).unwrap();
        assert_eq!(loaded.objects[0].style.font, requested);
        assert_eq!(loaded.objects[1].style.font, other);
        assert!(export_path.exists());

        let combined = dispatch("doc.combine", &args(json!({"document_ids":[id]}))).unwrap();
        let combined_id = combined["id"].as_str().unwrap();
        assert_eq!(combined["font_substitutions"], json!([]));
        assert_eq!(
            combined["source_font_substitutions"],
            json!([
                {"id":id,"font_substitutions":[first_warning, second_warning]}
            ])
        );

        dispatch(
            "doc.select",
            &args(json!({"id":id,"object_ids":[first_id]})),
        )
        .unwrap();
        let flattened = dispatch("doc.action", &args(json!({"id":id,"name":"flatten"}))).unwrap();
        assert_eq!(
            flattened["flattened_font_substitutions"],
            json!([first_warning])
        );
        assert_eq!(flattened["font_substitutions"], json!([second_warning]));
        let opened = dispatch("doc.open", &args(json!({"path":saved_path}))).unwrap();
        let opened_id = opened["id"].as_str().unwrap();
        assert_eq!(
            opened["font_substitutions"],
            json!([first_warning, second_warning])
        );
        for id in [id, combined_id, opened_id] {
            dispatch("doc.close", &args(json!({"id":id}))).unwrap();
        }
    }
}
