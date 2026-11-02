//! Text layout and rasterization with system fonts (`ab_glyph`), plus
//! per-character fallback fonts for symbols and CJK.

use crate::color::Rgba;
use crate::raster::blend;
use ab_glyph::{Font, FontArc, GlyphId, PxScale, ScaleFont};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(not(target_os = "macos"))]
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tiny_skia::Pixmap;

/// Families offered in the font list. Others fall back to the default.
pub const FAMILIES: &[&str] = &[
    "Segoe UI",
    "Arial",
    "Calibri",
    "Cambria",
    "Comic Sans MS",
    "Consolas",
    "Courier New",
    "Georgia",
    "Impact",
    "Tahoma",
    "Times New Roman",
    "Trebuchet MS",
    "Verdana",
];

pub const DEFAULT_FAMILY: &str = "Segoe UI";

/// Font families available to the editor, including installed macOS fonts.
pub fn families() -> Vec<String> {
    let mut families: Vec<String> = FAMILIES.iter().map(|name| (*name).to_owned()).collect();
    #[cfg(target_os = "macos")]
    for face in mac_fonts::database().faces() {
        for (name, _) in &face.families {
            if !name.starts_with('.')
                && !families
                    .iter()
                    .any(|existing| existing.eq_ignore_ascii_case(name))
            {
                families.push(name.clone());
            }
        }
    }
    families.sort_unstable_by_key(|name| name.to_ascii_lowercase());
    families
}

/// The actual face selected by the macOS font database.
#[cfg(target_os = "macos")]
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FontFace {
    pub family: String,
    pub post_script_name: String,
    pub weight: u16,
    pub style: &'static str,
    /// OpenType width class; 5 is normal.
    pub stretch: u16,
}

#[cfg(target_os = "macos")]
mod mac_fonts {
    use super::FontFace;
    use ab_glyph::{FontArc, FontVec};
    use fontdb::{Database, Family, Query, Stretch, Style, Weight};
    use std::sync::OnceLock;

    pub fn database() -> &'static Database {
        static DATABASE: OnceLock<Database> = OnceLock::new();
        DATABASE.get_or_init(|| {
            let mut db = Database::new();
            db.load_system_fonts();
            db
        })
    }

    pub fn load(family: &str, bold: bool, italic: bool) -> Option<(FontArc, FontFace)> {
        let query = Query {
            families: &[Family::Name(family)],
            weight: if bold { Weight::BOLD } else { Weight::NORMAL },
            stretch: Stretch::Normal,
            style: if italic { Style::Italic } else { Style::Normal },
        };
        let db = database();
        let id = db.query(&query)?;
        let face = db.face(id)?;
        let selected = FontFace {
            family: face
                .families
                .first()
                .map_or_else(|| family.to_owned(), |(name, _)| name.clone()),
            post_script_name: face.post_script_name.clone(),
            weight: face.weight.0,
            style: match face.style {
                Style::Normal => "normal",
                Style::Italic => "italic",
                Style::Oblique => "oblique",
            },
            stretch: face.stretch.to_number(),
        };
        let font = db.with_face_data(id, |data, index| {
            FontVec::try_from_vec_and_index(data.to_vec(), index)
                .ok()
                .map(FontArc::new)
        })??;
        Some((font, selected))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// Regular, bold, italic and bold-italic file names for a family.
#[cfg(not(target_os = "macos"))]
fn files(family: &str) -> Option<[&'static str; 4]> {
    Some(match family.to_ascii_lowercase().as_str() {
        "segoe ui" => [
            "segoeui.ttf",
            "segoeuib.ttf",
            "segoeuii.ttf",
            "segoeuiz.ttf",
        ],
        "arial" => ["arial.ttf", "arialbd.ttf", "ariali.ttf", "arialbi.ttf"],
        "calibri" => [
            "calibri.ttf",
            "calibrib.ttf",
            "calibrii.ttf",
            "calibriz.ttf",
        ],
        "cambria" => [
            "cambria.ttc",
            "cambriab.ttf",
            "cambriai.ttf",
            "cambriaz.ttf",
        ],
        "comic sans ms" => ["comic.ttf", "comicbd.ttf", "comici.ttf", "comicz.ttf"],
        "consolas" => [
            "consola.ttf",
            "consolab.ttf",
            "consolai.ttf",
            "consolaz.ttf",
        ],
        "courier new" => ["cour.ttf", "courbd.ttf", "couri.ttf", "courbi.ttf"],
        "georgia" => [
            "georgia.ttf",
            "georgiab.ttf",
            "georgiai.ttf",
            "georgiaz.ttf",
        ],
        "impact" => ["impact.ttf", "impact.ttf", "impact.ttf", "impact.ttf"],
        "tahoma" => ["tahoma.ttf", "tahomabd.ttf", "tahoma.ttf", "tahomabd.ttf"],
        "times new roman" => ["times.ttf", "timesbd.ttf", "timesi.ttf", "timesbi.ttf"],
        "trebuchet ms" => ["trebuc.ttf", "trebucbd.ttf", "trebucit.ttf", "trebucbi.ttf"],
        "verdana" => [
            "verdana.ttf",
            "verdanab.ttf",
            "verdanai.ttf",
            "verdanaz.ttf",
        ],
        // macOS families in imported files, mapped to the closest Windows font.
        "helvetica" | "helvetica neue" => return files("arial"),
        "proxima nova" | "sf pro" | "sf pro text" | ".sf ns" | "avenir" | "avenir next" => {
            return files("segoe ui")
        }
        "menlo" | "monaco" | "sf mono" => return files("consolas"),
        // Common Linux fonts, so tests and exports work off Windows too.
        "dejavu sans" => [
            "DejaVuSans.ttf",
            "DejaVuSans-Bold.ttf",
            "DejaVuSans-Oblique.ttf",
            "DejaVuSans-BoldOblique.ttf",
        ],
        _ => return None,
    })
}

#[cfg(not(target_os = "macos"))]
const FALLBACK_FILES: &[&str] = &[
    "seguisym.ttf",
    "msyh.ttc",
    "YuGothR.ttc",
    "malgun.ttf",
    "Nirmala.ttc",
    "seguiemj.ttf",
    "DejaVuSans.ttf",
];

#[cfg(not(target_os = "macos"))]
fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(windir) = std::env::var_os("WINDIR") {
        dirs.push(PathBuf::from(windir).join("Fonts"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(
            PathBuf::from(local)
                .join("Microsoft")
                .join("Windows")
                .join("Fonts"),
        );
    }
    for d in [
        "/usr/share/fonts/truetype/dejavu",
        "/usr/share/fonts/dejavu-sans-fonts",
        "/usr/share/fonts/dejavu",
        "/usr/share/fonts/TTF",
        "/usr/share/fonts",
        "/Library/Fonts",
        "/System/Library/Fonts",
    ] {
        dirs.push(PathBuf::from(d));
    }
    dirs
}

#[cfg(not(target_os = "macos"))]
fn load_file(name: &str) -> Option<FontArc> {
    font_dirs().into_iter().find_map(|dir| {
        let bytes = std::fs::read(dir.join(name)).ok()?;
        FontArc::try_from_vec(bytes).ok()
    })
}

#[cfg(target_os = "macos")]
#[derive(Clone)]
struct ResolvedFont {
    font: FontArc,
    face: FontFace,
    family_fallback: bool,
}

#[cfg(target_os = "macos")]
type CachedFont = ResolvedFont;
#[cfg(not(target_os = "macos"))]
type CachedFont = FontArc;

type Cache = Mutex<HashMap<(String, bool, bool), Option<CachedFont>>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Loads a family's face, using platform fallbacks when it is unavailable.
/// Returns `None` when no usable primary or fallback face can be loaded.
#[cfg(not(target_os = "macos"))]
pub fn font(family: &str, bold: bool, italic: bool) -> Option<FontArc> {
    let key = (family.to_ascii_lowercase(), bold, italic);
    if let Some(found) = cache().lock().unwrap().get(&key) {
        return found.clone();
    }
    let found = {
        let index = bold as usize + 2 * italic as usize;
        files(family)
            .and_then(|f| load_file(f[index]).or_else(|| load_file(f[0])))
            .or_else(|| {
                (family != DEFAULT_FAMILY)
                    .then(|| font(DEFAULT_FAMILY, bold, italic))
                    .flatten()
            })
            .or_else(|| font_from_list(&["DejaVuSans.ttf", "arial.ttf"]))
            .or_else(|| fallbacks().first().cloned())
    };
    cache().lock().unwrap().insert(key, found.clone());
    found
}

#[cfg(target_os = "macos")]
fn select_mac_font<T>(
    family: &str,
    mut load: impl FnMut(&str) -> Option<T>,
) -> Option<(T, Option<&'static str>)> {
    load(family)
        .map(|font| (font, None))
        .or_else(|| load("Helvetica Neue").map(|font| (font, Some("Helvetica Neue"))))
        .or_else(|| load("Arial Unicode MS").map(|font| (font, Some("Arial Unicode MS"))))
}

#[cfg(target_os = "macos")]
fn resolved_font(family: &str, bold: bool, italic: bool) -> Option<ResolvedFont> {
    let key = (family.to_ascii_lowercase(), bold, italic);
    if let Some(found) = cache().lock().unwrap().get(&key) {
        return found.clone();
    }
    let found = select_mac_font(family, |name| mac_fonts::load(name, bold, italic)).map(
        |((font, face), replacement)| ResolvedFont {
            font,
            face,
            family_fallback: replacement.is_some(),
        },
    );
    cache().lock().unwrap().insert(key, found.clone());
    found
}

/// Loads the closest face in the requested family, or a macOS fallback family.
#[cfg(target_os = "macos")]
pub fn font(family: &str, bold: bool, italic: bool) -> Option<FontArc> {
    resolved_font(family, bold, italic).map(|resolved| resolved.font)
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FontSubstitutionReason {
    Family,
    Style,
    Unavailable,
}

/// A family or face substitution made by the renderer. A missing replacement
/// means that no usable primary or fallback face was found.
#[cfg(target_os = "macos")]
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FontSubstitution {
    pub requested: String,
    pub bold: bool,
    pub italic: bool,
    pub reason: FontSubstitutionReason,
    pub replacement: Option<FontFace>,
}

#[cfg(target_os = "macos")]
fn substitution_for_face(
    family: &str,
    bold: bool,
    italic: bool,
    selected: Option<(&FontFace, bool)>,
) -> Option<FontSubstitution> {
    let reason = match selected {
        None => FontSubstitutionReason::Unavailable,
        Some((_, true)) => FontSubstitutionReason::Family,
        Some((face, false))
            if face.weight != if bold { 700 } else { 400 }
                || face.style != if italic { "italic" } else { "normal" }
                || face.stretch != 5 =>
        {
            FontSubstitutionReason::Style
        }
        Some(_) => return None,
    };
    Some(FontSubstitution {
        requested: family.to_owned(),
        bold,
        italic,
        reason,
        replacement: selected.map(|(face, _)| face.clone()),
    })
}

/// Reports the same face used by [`font`], without changing the requested
/// family stored in the document.
#[cfg(target_os = "macos")]
pub fn font_substitution(family: &str, bold: bool, italic: bool) -> Option<FontSubstitution> {
    let resolved = resolved_font(family, bold, italic);
    substitution_for_face(
        family,
        bold,
        italic,
        resolved
            .as_ref()
            .map(|font| (&font.face, font.family_fallback)),
    )
}

/// Reports substitutions for text objects and nested groups in document order.
#[cfg(target_os = "macos")]
pub fn object_font_substitutions<'a>(
    objects: impl IntoIterator<Item = &'a crate::object::Object>,
) -> Vec<FontSubstitution> {
    use crate::object::Kind;
    let mut seen = std::collections::HashSet::new();
    let mut substitutions = Vec::new();
    for object in objects {
        object.walk(&mut |object| {
            let (content, bold, italic) = match &object.kind {
                Kind::Text { text, .. } | Kind::Callout { text, .. } => {
                    (text, object.style.bold, object.style.italic)
                }
                Kind::Step { label, .. } => (label, true, false),
                _ => return,
            };
            if content.is_empty()
                || !seen.insert((object.style.font.to_ascii_lowercase(), bold, italic))
            {
                return;
            }
            if let Some(substitution) = font_substitution(&object.style.font, bold, italic) {
                substitutions.push(substitution);
            }
        });
    }
    substitutions
}

/// Formats a bounded notice suitable for the editor and capture notifications.
#[cfg(target_os = "macos")]
pub fn describe_font_substitutions(substitutions: &[FontSubstitution]) -> Option<String> {
    fn name(value: &str) -> String {
        let mut chars = value.chars();
        let mut label: String = chars
            .by_ref()
            .take(60)
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if chars.next().is_some() {
            label.push('…');
        }
        if label.trim().is_empty() {
            "(empty family)".into()
        } else {
            label
        }
    }

    if substitutions.is_empty() {
        return None;
    }
    let mut descriptions: Vec<_> = substitutions
        .iter()
        .take(3)
        .map(|substitution| {
            let style = match (substitution.bold, substitution.italic) {
                (true, true) => " (bold italic)",
                (true, false) => " (bold)",
                (false, true) => " (italic)",
                (false, false) => "",
            };
            let requested = name(&substitution.requested);
            match &substitution.replacement {
                Some(replacement) => format!(
                    "{requested}{style}: using {} (weight {}, {}, width {})",
                    name(&replacement.family),
                    replacement.weight,
                    replacement.style,
                    replacement.stretch,
                ),
                None => format!("{requested}{style}: no usable replacement; text will not render"),
            }
        })
        .collect();
    if substitutions.len() > descriptions.len() {
        descriptions.push(format!("{} more", substitutions.len() - descriptions.len()));
    }
    Some(format!("Font substitution: {}", descriptions.join("; ")))
}

#[cfg(not(target_os = "macos"))]
fn font_from_list(names: &[&str]) -> Option<FontArc> {
    names.iter().find_map(|n| load_file(n))
}

fn fallbacks() -> &'static [FontArc] {
    static FALLBACKS: OnceLock<Vec<FontArc>> = OnceLock::new();
    FALLBACKS.get_or_init(|| {
        #[cfg(target_os = "macos")]
        {
            [
                "Arial Unicode MS",
                "PingFang SC",
                "Hiragino Sans",
                "Apple SD Gothic Neo",
                "Apple Symbols",
            ]
            .iter()
            .filter_map(|family| mac_fonts::load(family, false, false).map(|(font, _)| font))
            .collect()
        }
        #[cfg(not(target_os = "macos"))]
        {
            FALLBACK_FILES.iter().filter_map(|n| load_file(n)).collect()
        }
    })
}

/// One positioned glyph: the font it comes from (index into
/// [`Layout::fonts`]), its id and its x offset within the line.
#[derive(Clone, Debug)]
struct Placed {
    font: usize,
    id: GlyphId,
    x: f32,
}

#[derive(Clone, Debug, Default)]
struct Line {
    glyphs: Vec<Placed>,
    width: f32,
}

/// Laid-out text: lines of glyphs at a pixel size.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    fonts: Vec<FontArc>,
    scales: Vec<PxScale>,
    lines: Vec<Line>,
    ascent: f32,
    line_height: f32,
}

impl Layout {
    pub fn width(&self) -> f32 {
        self.lines.iter().map(|l| l.width).fold(0.0, f32::max)
    }

    pub fn height(&self) -> f32 {
        self.lines.len() as f32 * self.line_height
    }

    pub fn line_height(&self) -> f32 {
        self.line_height
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
}

#[derive(Clone, Debug)]
pub struct TextStyle<'a> {
    pub family: &'a str,
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
}

/// Lays `text` out, wrapping at `max_width` when given. Words longer than
/// a line are broken between characters.
pub fn layout(text: &str, style: &TextStyle, max_width: Option<f32>) -> Layout {
    let Some(primary) = font(style.family, style.bold, style.italic) else {
        return Layout::default();
    };
    let mut fonts = vec![primary];
    fonts.extend(fallbacks().iter().cloned());
    // Font sizes are em sizes; ab_glyph scales by ascent-to-descent height.
    let scales: Vec<PxScale> = fonts.iter().map(|f| em_scale(f, style.size)).collect();
    let s0 = fonts[0].as_scaled(scales[0]);
    let ascent = s0.ascent();
    let line_height = (s0.ascent() - s0.descent() + s0.line_gap()).max(style.size);

    let pick = |c: char| -> (usize, GlyphId) {
        for (i, f) in fonts.iter().enumerate() {
            let id = f.glyph_id(c);
            if id.0 != 0 {
                return (i, id);
            }
        }
        (0, fonts[0].glyph_id(c))
    };
    let advance = |f: usize, id: GlyphId| fonts[f].as_scaled(scales[f]).h_advance(id);
    let max = max_width.unwrap_or(f32::INFINITY).max(style.size);

    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let paragraph = paragraph.trim_end_matches('\r');
        let mut line = Line::default();
        let mut prev: Option<(usize, GlyphId)> = None;
        // Split into words that keep their trailing spaces.
        let mut words: Vec<&str> = Vec::new();
        let mut start = 0;
        for (i, c) in paragraph.char_indices() {
            if c == ' ' {
                let next = i + 1;
                if paragraph[next..].starts_with(' ') {
                    continue;
                }
                words.push(&paragraph[start..next]);
                start = next;
            }
        }
        if start < paragraph.len() {
            words.push(&paragraph[start..]);
        }
        for word in words {
            let glyphs: Vec<(usize, GlyphId, f32)> = word
                .chars()
                .map(|c| {
                    let (f, id) = pick(c);
                    (f, id, advance(f, id))
                })
                .collect();
            let visible: f32 = word.trim_end().chars().zip(&glyphs).map(|(_, g)| g.2).sum();
            if line.width > 0.0 && line.width + visible > max {
                lines.push(std::mem::take(&mut line));
                prev = None;
            }
            for (f, id, adv) in glyphs {
                if line.width > 0.0
                    && line.width + adv > max
                    && word.trim_end().len() > 1
                    && visible > max
                {
                    lines.push(std::mem::take(&mut line));
                    prev = None;
                }
                let kern = match prev {
                    Some((pf, pid)) if pf == f => fonts[f].as_scaled(scales[f]).kern(pid, id),
                    _ => 0.0,
                };
                line.glyphs.push(Placed {
                    font: f,
                    id,
                    x: line.width + kern,
                });
                line.width += adv + kern;
                prev = Some((f, id));
            }
        }
        // Trailing spaces do not count toward alignment.
        let trailing: f32 = paragraph
            .chars()
            .rev()
            .take_while(|c| *c == ' ')
            .map(|c| advance(pick(c).0, pick(c).1))
            .sum();
        if line.glyphs.len() > 0 && trailing > 0.0 && trailing < line.width {
            line.width -= trailing;
        }
        lines.push(line);
    }
    Layout {
        fonts,
        scales,
        lines,
        ascent,
        line_height,
    }
}

/// The ab_glyph scale that renders `font` at an em size of `size` pixels.
fn em_scale(font: &FontArc, size: f32) -> PxScale {
    let em = font.units_per_em().unwrap_or(1000.0);
    PxScale::from(size.max(1.0) * font.height_unscaled() / em)
}

/// For each line: its left edge, width and baseline, as `draw` places it.
pub fn line_extents(
    layout: &Layout,
    x: f32,
    y: f32,
    box_width: f32,
    align: Align,
) -> Vec<(f32, f32, f32)> {
    layout
        .lines
        .iter()
        .enumerate()
        .map(|(row, line)| {
            let dx = match align {
                Align::Left => 0.0,
                Align::Center => (box_width - line.width) / 2.0,
                Align::Right => box_width - line.width,
            };
            (
                x + dx,
                line.width,
                y + layout.ascent + row as f32 * layout.line_height,
            )
        })
        .collect()
}

/// Draws `layout` with its top-left at (`x`, `y`) in document pixels, each
/// line aligned within `box_width`. `origin` is the document position of the
/// target pixmap's top-left pixel.
pub fn draw(
    target: &mut Pixmap,
    origin: (i32, i32),
    layout: &Layout,
    x: f32,
    y: f32,
    box_width: f32,
    align: Align,
    color: Rgba,
) {
    for (row, line) in layout.lines.iter().enumerate() {
        let dx = match align {
            Align::Left => 0.0,
            Align::Center => (box_width - line.width) / 2.0,
            Align::Right => box_width - line.width,
        };
        let baseline = y + layout.ascent + row as f32 * layout.line_height - origin.1 as f32;
        for g in &line.glyphs {
            let font = &layout.fonts[g.font];
            let glyph = g.id.with_scale_and_position(
                layout.scales[g.font],
                ab_glyph::point(x + dx + g.x - origin.0 as f32, baseline),
            );
            if let Some(outlined) = font.outline_glyph(glyph) {
                let b = outlined.px_bounds();
                let (bx, by) = (b.min.x as i32, b.min.y as i32);
                outlined.draw(|px, py, coverage| {
                    blend(target, bx + px as i32, by + py as i32, color, coverage)
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style(size: f32) -> TextStyle<'static> {
        TextStyle {
            family: DEFAULT_FAMILY,
            size,
            bold: false,
            italic: false,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_system_fonts_render_default_and_dejavu_text() {
        for family in [DEFAULT_FAMILY, "DejaVu Sans"] {
            let face = font(family, false, false).unwrap_or_else(|| {
                panic!("the installed Linux font provider must supply {family}")
            });
            assert_ne!(
                face.glyph_id('A'),
                GlyphId(0),
                "{family} must contain Latin glyphs"
            );
            let text_style = TextStyle {
                family,
                ..style(20.0)
            };
            let text = layout("Capture information", &text_style, None);
            assert!(text.width() > 60.0, "{family} must measure visible text");
            let mut pixels = Pixmap::new(240, 40).expect("test raster dimensions are valid");
            draw(
                &mut pixels,
                (0, 0),
                &text,
                0.0,
                0.0,
                240.0,
                Align::Left,
                Rgba::BLACK,
            );
            assert!(
                pixels.data().chunks(4).any(|pixel| pixel[3] > 200),
                "{family} must rasterize visible text from the installed font"
            );
        }
    }

    #[test]
    fn wraps_words_and_measures() {
        if font(DEFAULT_FAMILY, false, false).is_none() {
            return; // No fonts on this machine.
        }
        let one = layout("Hello world", &style(20.0), None);
        assert_eq!(one.line_count(), 1);
        assert!(one.width() > 60.0);
        let wrapped = layout("Hello world", &style(20.0), Some(one.width() * 0.7));
        assert_eq!(wrapped.line_count(), 2);
        let lines = layout("a\nb\n", &style(20.0), None);
        assert_eq!(lines.line_count(), 3);
        let mut p = Pixmap::new(200, 40).unwrap();
        draw(
            &mut p,
            (0, 0),
            &one,
            0.0,
            0.0,
            200.0,
            Align::Left,
            Rgba::BLACK,
        );
        assert!(
            p.data().chunks(4).any(|px| px[3] > 200),
            "glyphs were drawn"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn resolves_installed_families_and_collection_faces() {
        let families = families();
        assert!(families.iter().any(|name| name == "Helvetica Neue"));
        let (regular, _) =
            mac_fonts::load("Helvetica Neue", false, false).expect("system regular face");
        let (bold, _) = mac_fonts::load("Helvetica Neue", true, false).expect("system bold face");
        assert_ne!(regular.glyph_id('A').0, 0);
        assert_ne!(bold.glyph_id('A').0, 0);
        assert!(
            font(DEFAULT_FAMILY, false, false).is_some(),
            "documents with Windows fonts need a substitute"
        );
        let text = layout("Hello 世界", &style(20.0), None);
        assert!(text.width() > 0.0);
        assert!(
            text.lines[0].glyphs.iter().all(|glyph| glyph.id.0 != 0),
            "CJK glyphs need a fallback"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_cjk_fallback_renders_visible_glyphs() {
        let layout = layout(
            "漢字",
            &TextStyle {
                family: "Helvetica Neue",
                size: 28.0,
                bold: false,
                italic: false,
            },
            None,
        );
        assert_eq!(layout.lines[0].glyphs.len(), 2);
        for glyph in &layout.lines[0].glyphs {
            assert_ne!(glyph.id.0, 0, "CJK character resolved to a missing glyph");
            let font = &layout.fonts[glyph.font];
            assert!(
                font.outline_glyph(glyph.id.with_scale(layout.scales[glyph.font]))
                    .is_some(),
                "CJK fallback has no renderable outline"
            );
        }
        let mut pixels = Pixmap::new(120, 60).unwrap();
        draw(
            &mut pixels,
            (0, 0),
            &layout,
            0.0,
            0.0,
            120.0,
            Align::Left,
            Rgba::BLACK,
        );
        assert!(
            pixels.data().chunks_exact(4).any(|pixel| pixel[3] != 0),
            "CJK fallback drew no pixels"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_font_choice_reports_exact_and_fallback_faces() {
        assert!(font_substitution("Helvetica Neue", false, false).is_none());
        let missing = "Jiffshot deliberately unavailable family 8f29b6";
        for (bold, italic) in [(false, false), (true, false), (false, true), (true, true)] {
            let substitution =
                font_substitution(missing, bold, italic).expect("missing family is reported");
            assert_eq!(substitution.requested, missing);
            assert_eq!((substitution.bold, substitution.italic), (bold, italic));
            assert_eq!(substitution.reason, FontSubstitutionReason::Family);
            assert_eq!(
                substitution
                    .replacement
                    .as_ref()
                    .map(|face| face.family.as_str()),
                Some("Helvetica Neue")
            );
            assert_eq!(
                substitution.replacement,
                Some(mac_fonts::load("Helvetica Neue", bold, italic).unwrap().1)
            );
            let requested = layout(
                "Sample",
                &TextStyle {
                    family: missing,
                    size: 23.0,
                    bold,
                    italic,
                },
                None,
            );
            let fallback = layout(
                "Sample",
                &TextStyle {
                    family: "Helvetica Neue",
                    size: 23.0,
                    bold,
                    italic,
                },
                None,
            );
            assert_eq!(
                (requested.width(), requested.height()),
                (fallback.width(), fallback.height())
            );
            assert_eq!(
                requested.lines[0]
                    .glyphs
                    .iter()
                    .map(|glyph| glyph.id)
                    .collect::<Vec<_>>(),
                fallback.lines[0]
                    .glyphs
                    .iter()
                    .map(|glyph| glyph.id)
                    .collect::<Vec<_>>(),
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_font_choice_handles_each_fallback_and_no_font() {
        let mut attempted = Vec::new();
        let result = select_mac_font("missing", |name| {
            attempted.push(name.to_owned());
            (name == "Arial Unicode MS").then_some(7)
        });
        assert_eq!(result, Some((7, Some("Arial Unicode MS"))));
        assert_eq!(attempted, ["missing", "Helvetica Neue", "Arial Unicode MS"]);
        assert_eq!(select_mac_font::<()>("missing", |_| None), None);

        let missing = "Jiffshot simulated unavailable fallback 1f320a";
        let key = (missing.to_ascii_lowercase(), false, true);
        cache().lock().unwrap().insert(key.clone(), None);
        assert!(font(missing, false, true).is_none());
        assert_eq!(
            font_substitution(missing, false, true),
            Some(FontSubstitution {
                requested: missing.to_owned(),
                bold: false,
                italic: true,
                reason: FontSubstitutionReason::Unavailable,
                replacement: None,
            }),
        );
        cache().lock().unwrap().remove(&key);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_font_choice_reports_closest_weight_slant_and_width() {
        let regular = FontFace {
            family: "Fixture".into(),
            post_script_name: "Fixture-Regular".into(),
            weight: 400,
            style: "normal",
            stretch: 5,
        };
        assert_eq!(
            substitution_for_face("Fixture", false, false, Some((&regular, false))),
            None
        );
        for (face, bold, italic) in [
            (regular.clone(), true, false),
            (regular.clone(), false, true),
            (
                FontFace {
                    weight: 500,
                    ..regular.clone()
                },
                false,
                false,
            ),
            (
                FontFace {
                    style: "oblique",
                    ..regular.clone()
                },
                false,
                true,
            ),
            (
                FontFace {
                    stretch: 3,
                    ..regular.clone()
                },
                false,
                false,
            ),
        ] {
            let substitution =
                substitution_for_face("Fixture", bold, italic, Some((&face, false))).unwrap();
            assert_eq!(substitution.reason, FontSubstitutionReason::Style);
            assert_eq!(substitution.replacement, Some(face));
        }
        let fallback =
            substitution_for_face("Missing", false, false, Some((&regular, true))).unwrap();
        assert_eq!(fallback.reason, FontSubstitutionReason::Family);
        assert_eq!(fallback.replacement, Some(regular));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_font_notice_uses_the_same_cached_face_as_rendering() {
        let family = "Jiffshot simulated regular-only family fd242a";
        let key = (family.to_ascii_lowercase(), true, false);
        let (font, face) = mac_fonts::load("Helvetica Neue", false, false).unwrap();
        assert_eq!((face.weight, face.style, face.stretch), (400, "normal", 5));
        cache().lock().unwrap().insert(
            key.clone(),
            Some(ResolvedFont {
                font,
                face: face.clone(),
                family_fallback: false,
            }),
        );
        let notice = font_substitution(family, true, false).unwrap();
        assert_eq!(notice.reason, FontSubstitutionReason::Style);
        assert_eq!(notice.replacement, Some(face));
        let message = describe_font_substitutions(&[notice]).unwrap();
        assert!(message.contains("(bold): using Helvetica Neue (weight 400, normal, width 5)"));
        let requested = layout(
            "Sample",
            &TextStyle {
                family,
                size: 23.0,
                bold: true,
                italic: false,
            },
            None,
        );
        let selected = layout(
            "Sample",
            &TextStyle {
                family: "Helvetica Neue",
                size: 23.0,
                bold: false,
                italic: false,
            },
            None,
        );
        assert_eq!(
            (requested.width(), requested.height()),
            (selected.width(), selected.height())
        );
        assert_eq!(
            requested.lines[0]
                .glyphs
                .iter()
                .map(|glyph| glyph.id)
                .collect::<Vec<_>>(),
            selected.lines[0]
                .glyphs
                .iter()
                .map(|glyph| glyph.id)
                .collect::<Vec<_>>(),
        );
        cache().lock().unwrap().remove(&key);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_font_notice_reports_the_second_fallback_face() {
        let family = "Jiffshot simulated second fallback 1524e1";
        let key = (family.to_ascii_lowercase(), false, false);
        let (font, face) =
            mac_fonts::load("Arial Unicode MS", false, false).expect("system Unicode fallback");
        cache().lock().unwrap().insert(
            key.clone(),
            Some(ResolvedFont {
                font,
                face: face.clone(),
                family_fallback: true,
            }),
        );
        let notice = font_substitution(family, false, false).unwrap();
        assert_eq!(notice.reason, FontSubstitutionReason::Family);
        assert_eq!(notice.replacement, Some(face));
        let requested = layout(
            "Sample",
            &TextStyle {
                family,
                size: 23.0,
                bold: false,
                italic: false,
            },
            None,
        );
        let selected = layout(
            "Sample",
            &TextStyle {
                family: "Arial Unicode MS",
                size: 23.0,
                bold: false,
                italic: false,
            },
            None,
        );
        assert_eq!(
            (requested.width(), requested.height()),
            (selected.width(), selected.height())
        );
        assert_eq!(
            requested.lines[0]
                .glyphs
                .iter()
                .map(|glyph| glyph.id)
                .collect::<Vec<_>>(),
            selected.lines[0]
                .glyphs
                .iter()
                .map(|glyph| glyph.id)
                .collect::<Vec<_>>(),
        );
        cache().lock().unwrap().remove(&key);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn font_notice_is_bounded_and_safe_for_the_notification_protocol() {
        assert_eq!(describe_font_substitutions(&[]), None);
        let face = |family: &str| FontFace {
            family: family.into(),
            post_script_name: "Fixture-Regular".into(),
            weight: 400,
            style: "normal",
            stretch: 5,
        };
        let mut substitutions = vec![
            FontSubstitution {
                requested: "Missing\u{1f}family".into(),
                bold: true,
                italic: true,
                reason: FontSubstitutionReason::Family,
                replacement: Some(face("Helvetica Neue")),
            },
            FontSubstitution {
                requested: "\n".into(),
                bold: false,
                italic: false,
                reason: FontSubstitutionReason::Unavailable,
                replacement: None,
            },
            FontSubstitution {
                requested: "x".repeat(90),
                bold: false,
                italic: true,
                reason: FontSubstitutionReason::Family,
                replacement: Some(face("Arial Unicode MS")),
            },
        ];
        substitutions.push(substitutions[0].clone());
        let description = describe_font_substitutions(&substitutions).unwrap();
        assert!(description.contains("Missing family (bold italic): using Helvetica Neue"));
        assert!(description.contains("(empty family): no usable replacement; text will not render"));
        assert!(description.contains(&format!(
            "{}… (italic): using Arial Unicode MS",
            "x".repeat(60)
        )));
        assert!(description.ends_with("1 more"));
        assert!(!description.chars().any(char::is_control));
    }
}
