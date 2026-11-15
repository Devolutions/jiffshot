//! The tool palette across the top (the tools that fit, in the user's
//! order, then More with the rest; right-click a tool to move or hide it)
//! and the Effects panel.

use super::main_common::{act, action, brand_accent, brand_text, chip, note, run, section, solid, with_context_menu, Ctx, Tone, TRANSPARENT, WHITE};
use super::super::icons::vector as vicons;
use crate::vm::MainViewModelSink;
use rustolonia::{
    AnyEl, AppScope, Border, Button, Dock, DockPanel, Effect, El, FontWeight, HorizontalAlignment, MenuFlyout, MenuItem, MenuItemToggleType, Orientation, Panel, Path, ScrollBarVisibility, ScrollViewer, Separator, Signal, StackPanel, TextBlock,
    VerticalAlignment, WrapPanel,
};

/// A palette tool: its view-model name, crisp vector path, caption, and tooltip summary.
pub struct ToolDef {
    pub name: &'static str,
    pub path: &'static str,
    pub caption: &'static str,
    pub description: &'static str,
}

const fn tool(name: &'static str, path: &'static str, caption: &'static str, description: &'static str) -> ToolDef {
    ToolDef { name, path, caption, description }
}

/// Every tool, in the usual order, using a unified stroke-vector icon set.
pub const TOOLS: [ToolDef; 23] = [
    tool("Select", vicons::SELECT_REGION, "Select Region", "Select a rectangular area of the image"),
    tool("Move", vicons::MOVE, "Move", "Select, move, and resize objects"),
    tool("Arrow", vicons::ARROW, "Arrow", "Draw an arrow annotation"),
    tool("Line", vicons::LINE, "Line", "Draw a straight line"),
    tool("Shape", vicons::SHAPE, "Shape", "Draw rectangles, ellipses, and polygons"),
    tool("Text", vicons::TEXT, "Text", "Add a text label"),
    tool("Callout", vicons::CALLOUT, "Callout", "Add a speech bubble callout with tails"),
    tool("Step", vicons::STEP, "Step", "Place numbered step markers"),
    tool("Pen", vicons::PEN, "Pen", "Draw freehand ink strokes"),
    tool("Highlight", vicons::HIGHLIGHT, "Highlight", "Highlight areas with translucent marker ink"),
    tool("Blur", vicons::BLUR, "Blur", "Blur or pixelate sensitive regions"),
    tool("Magnify", vicons::MAGNIFY, "Magnify", "Zoom into a region with a loupe"),
    tool("Spotlight", vicons::SPOTLIGHT, "Spotlight", "Dim the surrounding image to focus on an area"),
    tool("Stamp", vicons::STAMP, "Stamp", "Place stamps and symbols"),
    tool("Fill", vicons::FILL, "Fill", "Flood-fill an area with a color"),
    tool("Crop", vicons::CROP, "Crop", "Crop the canvas to a selected rectangle"),
    tool("Cut Out", vicons::CUT_OUT, "Cut Out", "Remove a horizontal or vertical strip and join the edges"),
    tool("Selection", vicons::LIFT_SELECTION, "Lift Selection", "Cut out a region into a movable object"),
    tool("Magic Wand", vicons::MAGIC_WAND, "Magic Wand", "Select connected pixels of a similar color"),
    tool("Eraser", vicons::ERASER, "Eraser", "Erase parts of annotations or the background"),
    tool("Eyedropper", vicons::EYEDROPPER, "Pick color", "Sample a color from the canvas"),
    tool("Smart Move", vicons::SMART_MOVE, "Smart Move", "Detect and move UI elements in screenshots"),
    tool("Text Replace", vicons::TEXT_REPLACE, "Text Replace", "Detect and edit text inside a screenshot"),
];

fn index_of(name: &str) -> Option<usize> {
    TOOLS.iter().position(|t| t.name == name)
}

/// The 1-based shortcut (`Ctrl+1` … `Ctrl+9`) when `index` is among the first
/// nine ordered tools.
fn shortcut_for_tool(order: &str, index: usize) -> Option<String> {
    let pos = ordered(order).iter().position(|&i| i == index)?;
    (pos < 9).then(|| crate::ui::shortcut(&format!("Ctrl+{}", pos + 1)))
}

fn format_tool_tooltip(order: &str, index: usize) -> String {
    let def = &TOOLS[index];
    match shortcut_for_tool(order, index) {
        Some(key) => format!("{} ({key}) — {}", def.caption, def.description),
        None => format!("{} — {}", def.caption, def.description),
    }
}

/// The tools in the user's order (all of them when none is set), as
/// indices into [`TOOLS`].
fn ordered(order: &str) -> Vec<usize> {
    let picked: Vec<usize> = order.split(',').filter(|s| !s.is_empty()).filter_map(index_of).collect();
    if picked.is_empty() && order.split(',').all(|s| s.is_empty()) {
        (0..TOOLS.len()).collect()
    } else {
        picked
    }
}

/// How many tools fit beside the More button in `width`.
fn capacity(width: f64, labels: bool) -> usize {
    let per = if labels { 72.0 } else { 42.0 };
    if width.is_finite() && width > per {
        ((width - per) / per) as usize
    } else {
        0
    }
}

/// The tools shown in the bar and those under More. `extra` (a tool picked
/// from More or chosen elsewhere) takes the last place that fits.
fn layout(order: &str, capacity: usize, extra: Option<usize>) -> (Vec<usize>, Vec<usize>) {
    let shown = ordered(order);
    let hidden = (0..TOOLS.len()).filter(|i| !shown.contains(i));
    let mut fits: Vec<usize> = shown.iter().copied().take(capacity).collect();
    let mut more: Vec<usize> = shown.iter().copied().skip(capacity).chain(hidden).collect();
    if let Some(extra) = extra.filter(|e| capacity > 0 && !fits.contains(e)) {
        if let Some(at) = more.iter().position(|&i| i == extra) {
            more.remove(at);
            if fits.len() == capacity {
                if let Some(last) = fits.pop() {
                    more.insert(0, last);
                }
            }
            fits.push(extra);
        }
    }
    (fits, more)
}

/// One place in the bar.
#[derive(Clone, PartialEq)]
enum Slot {
    Tool(usize),
    More(Vec<usize>),
}

impl Slot {
    fn key(&self) -> Option<usize> {
        match self {
            Slot::Tool(i) => Some(*i),
            Slot::More(_) => None,
        }
    }
}

/// The tool palette: fills the space between the toolbar's other buttons.
pub fn tool_bar(ctx: &Ctx) -> El<Panel> {
    let sink = ctx.sink;
    let width = Signal::new(0.0_f64);
    // A tool picked from More, shown in place of the last tool that fits.
    let extra: Signal<Option<usize>> = Signal::new(None);
    let slots = move || {
        let labels = sink.tool_labels.get();
        // Other signals are read before `with`, which must not nest reads.
        let (cap, extra) = (capacity(width.get(), labels), extra.get());
        let (fits, more) = sink.tool_order.with(|order| layout(order, cap, extra));
        let mut slots: Vec<Slot> = fits.into_iter().map(Slot::Tool).collect();
        slots.push(Slot::More(more));
        slots
    };
    let theme = ctx.theme;
    let scope = ctx.scope.clone();
    let bar = StackPanel::el()
        .orientation(Orientation::Horizontal)
        .horizontal_alignment(HorizontalAlignment::Left)
        .vertical_alignment(VerticalAlignment::Center)
        .children_for(slots, Slot::key, move |slot: Signal<Slot>| match slot.get_untracked() {
            Slot::Tool(i) => tool_button(sink, theme, i),
            Slot::More(_) => more_button(scope.clone(), sink, theme, slot, extra),
        });
    Panel::el()
        .background(solid(TRANSPARENT))
        .on_size_changed(move |args| width.set_if_changed(args.new_size.width))
        .child(bar)
        .with_op(move |_, _| {
            // A tool chosen from a menu or a shortcut shows in the bar.
            Effect::new(move || {
                let tool = sink.tool.get();
                let Some(index) = index_of(&tool) else { return Ok(()) };
                let cap = capacity(width.get(), sink.tool_labels.get());
                let current = extra.get_untracked();
                let (fits, _) = sink.tool_order.with(|order| layout(order, cap, current));
                if cap > 0 && !fits.contains(&index) {
                    extra.set(Some(index));
                }
                Ok(())
            })?;
            Ok(())
        })
}

/// A palette button's crisp 18×18 vector icon above its caption (hidden without labels).
fn tool_content(sink: MainViewModelSink, theme: super::main_common::Theme, path_data: &'static str, caption: &str, active: impl Fn() -> bool + Clone + 'static) -> El<StackPanel> {
    let glyph_active = active.clone();
    let icon = Border::el()
        .height(if cfg!(target_os = "linux") { 24.0 } else { 21.0 })
        .child(
            Path::el()
                .width(18.0)
                .height(18.0)
                .data(path_data)
                .stroke(move || Some(if glyph_active() { solid(WHITE) } else { theme.get(Tone::Text) }))
                .stroke_thickness(1.5)
                .horizontal_alignment(HorizontalAlignment::Center)
                .vertical_alignment(VerticalAlignment::Center),
        );
    StackPanel::el()
        .spacing(2.0)
        .child(icon)
        .child(
            TextBlock::el()
                .text(caption.to_owned())
                .font_size(10.0)
                .horizontal_alignment(HorizontalAlignment::Center)
                .text_trimming("CharacterEllipsis")
                .visible(sink.tool_labels)
                .foreground(move || if active() { solid(WHITE) } else { theme.get(Tone::Caption) }),
        )
}

/// Linux fonts have taller line boxes, so the icon and its caption need more room.
#[cfg(target_os = "linux")]
const LABELED_HEIGHT: f64 = 58.0;
#[cfg(not(target_os = "linux"))]
const LABELED_HEIGHT: f64 = 48.0;

/// A palette-sized button (`Button.tool`, `compact` without labels).
fn palette_button(sink: MainViewModelSink) -> El<Button> {
    Button::el()
        .width(move || if sink.tool_labels.get() { 68.0 } else { 38.0 })
        .height(move || if sink.tool_labels.get() { LABELED_HEIGHT } else { 34.0 })
        .padding((2.0, 0.0))
        .margin(2.0)
        .corner_radius(4.0)
        .border_thickness(1.0)
        .horizontal_content_alignment(HorizontalAlignment::Center)
        .vertical_content_alignment(VerticalAlignment::Center)
}

/// "Icon and text" or "Icon only", checked when it is the current choice.
fn labels_item(sink: MainViewModelSink, labels: bool) -> AnyEl {
    MenuItem::el()
        .header(if labels { "Icon and text" } else { "Icon only" })
        .toggle_type(MenuItemToggleType::Radio)
        .checked(move || sink.tool_labels.get() == labels)
        .command(act(move || sink.tool_labels.set_if_changed(labels)))
        .into()
}

fn tool_button(sink: MainViewModelSink, theme: super::main_common::Theme, index: usize) -> AnyEl {
    let def = &TOOLS[index];
    let name = def.name;
    let active = move || sink.tool.with(|t| t == name);
    let border_active = active;
    let button = palette_button(sink)
        .name(format!("Tool{}", name.replace(' ', "")))
        .tool_tip_tip(move || sink.tool_order.with(|order| format_tool_tooltip(order, index)))
        .background(move || {
            if active() {
                solid(if theme.dark.get() && cfg!(windows) { crate::ui::dark_blue::SELECTION } else { brand_accent(theme) })
            } else {
                solid(TRANSPARENT)
            }
        })
        .border_brush(move || {
            if border_active() {
                solid(brand_text(theme))
            } else {
                solid(TRANSPARENT)
            }
        })
        .content(tool_content(sink, theme, def.path, def.caption, active))
        .command(action(format!("tool:{name}")));
    with_context_menu(
        button,
        vec![
            MenuItem::el().header("Move left").command(action(format!("toolbar:up:{name}"))).into(),
            MenuItem::el().header("Move right").command(action(format!("toolbar:down:{name}"))).into(),
            MenuItem::el().header("Move to More").command(action(format!("toolbar:hide:{name}"))).into(),
            Separator::el().into(),
            labels_item(sink, true),
            labels_item(sink, false),
            Separator::el().into(),
            MenuItem::el().header("Show all tools").command(action("toolbar:reset")).into(),
        ],
    )
    .into()
}

fn more_button(scope: AppScope, sink: MainViewModelSink, theme: super::main_common::Theme, slot: Signal<Slot>, extra: Signal<Option<usize>>) -> AnyEl {
    palette_button(sink)
        .name("ToolMore")
        .tool_tip_tip("More tools — Choose hidden or overflow annotation tools")
        .background(solid(TRANSPARENT))
        .border_brush(solid(TRANSPARENT))
        .content(tool_content(sink, theme, vicons::MORE, "More", || false))
        .with_op(move |_, button| {
            let target = button.clone();
            button.clone().on_click(&scope.clone(), move |_| {
                let Slot::More(more) = slot.get_untracked() else { return };
                if let Err(error) = more_menu(sink, theme, &more, extra).build(&scope).and_then(|flyout| flyout.show_at_with_control(&target)) {
                    eprintln!("could not show More tools: {error}");
                }
            })?;
            Ok(())
        })
        .into()
}

/// More's menu: the tools that do not fit (with their matching vector icons and
/// keyboard shortcuts), then the label choices.
fn more_menu(sink: MainViewModelSink, theme: super::main_common::Theme, more: &[usize], extra: Signal<Option<usize>>) -> El<MenuFlyout> {
    let order = sink.tool_order.get_untracked();
    let icon_brush = theme.get(Tone::Text);
    let mut items: Vec<AnyEl> = more
        .iter()
        .map(|&i| {
            let def = &TOOLS[i];
            let name = def.name;
            let icon = Border::el().width(18.0).height(18.0).child(
                Path::el()
                    .width(16.0)
                    .height(16.0)
                    .data(def.path)
                    .stroke(Some(icon_brush))
                    .stroke_thickness(1.4)
                    .horizontal_alignment(HorizontalAlignment::Center)
                    .vertical_alignment(VerticalAlignment::Center),
            );
            let item = MenuItem::el()
                .header(def.caption)
                .icon(icon)
                .tool_tip_tip(def.description)
                .toggle_type(MenuItemToggleType::Radio)
                .checked(sink.tool.get_untracked() == name)
                .command(act(move || {
                    extra.set(Some(i));
                    run(format!("tool:{name}"));
                }));
            let item = match shortcut_for_tool(&order, i) {
                Some(gesture) => item.input_gesture(gesture),
                None => item,
            };
            item.into()
        })
        .collect();
    if !items.is_empty() {
        items.push(Separator::el().into());
    }
    items.push(labels_item(sink, true));
    items.push(labels_item(sink, false));
    items.push(MenuItem::el().header("Show all tools in the usual order").command(action("toolbar:reset")).into());
    items.push(MenuItem::el().header("Right-click a tool to move it or put it in More").enabled(false).into());
    MenuFlyout::el().items_iter(items)
}

// ---- effects ----------------------------------------------------------------

/// The Effects panel's groups; effects not listed go under More.
const EFFECT_GROUPS: [(&str, &[&str]); 9] = [
    ("Border", &["Border"]),
    ("Edges", &["Torn edge", "Torn bottom", "Sawtooth edge", "Wave edge", "Fade edges"]),
    ("Shadow", &["Shadow"]),
    ("Perspective", &["Perspective", "Tilt", "Page curl", "Reflection"]),
    ("Corners", &["Rounded corners"]),
    ("Filters", &["Sharpen", "Blur image", "Monochrome", "Grayscale", "Sepia", "Invert", "Vignette"]),
    ("Color adjustment", &["Brighten", "Darken", "More contrast", "Less contrast", "Saturate", "Desaturate", "Hue shift", "Gamma up", "Gamma down"]),
    ("Color replacement", &["Replace color"]),
    ("Size", &["Scale to 1920"]),
];

/// The effects the model offers, in their groups (those in no group under
/// More), leaving out empty groups.
pub fn grouped_effects(names: &[String]) -> Vec<(String, Vec<String>)> {
    let listed = |name: &str| EFFECT_GROUPS.iter().any(|(_, effects)| effects.contains(&name));
    EFFECT_GROUPS
        .iter()
        .map(|(group, effects)| (group.to_string(), effects.iter().filter(|e| names.iter().any(|n| n == *e)).map(|e| e.to_string()).collect::<Vec<_>>()))
        .chain(std::iter::once(("More".to_owned(), names.iter().filter(|n| !listed(n)).cloned().collect())))
        .filter(|(_, effects)| !effects.is_empty())
        .collect()
}

/// The Effects panel: each group's effects as buttons.
pub fn effects_panel(ctx: &Ctx) -> El<Border> {
    let (sink, theme) = (ctx.sink, ctx.theme);
    let groups = StackPanel::el().margin((14.0, 0.0, 14.0, 16.0)).children_for(
        move || sink.effects.with(|names| grouped_effects(names)),
        |(group, _): &(String, Vec<String>)| group.clone(),
        move |group: Signal<(String, Vec<String>)>| {
            StackPanel::el()
                .child(section(theme, &group.get_untracked().0.to_uppercase()))
                .child(WrapPanel::el().children_for(move || group.get().1, String::clone, |effect: Signal<String>| {
                    let name = effect.get_untracked();
                    chip(&name).command(action(format!("effect:{name}")))
                }))
        },
    );
    Border::el()
        .dock_panel_dock(Dock::Right)
        .width(230.0)
        .background(theme.brush(Tone::Panel))
        .border_brush(theme.brush(Tone::Border))
        .border_thickness((1.0, 0.0, 0.0, 0.0))
        .visible(sink.show_effects)
        .child(
            DockPanel::el()
                .child(
                    Border::el()
                        .dock_panel_dock(Dock::Top)
                        .border_brush(theme.brush(Tone::Border))
                        .border_thickness((0.0, 0.0, 0.0, 1.0))
                        .padding((14.0, 8.0, 8.0, 8.0))
                        .child(
                            DockPanel::el()
                                .child(
                                    super::main_common::bar(
                                        Path::el()
                                            .width(12.0)
                                            .height(12.0)
                                            .data(vicons::CLOSE)
                                            .stroke(move || Some(theme.get(Tone::Caption)))
                                            .stroke_thickness(1.5),
                                    )
                                    .name("CloseEffectsPanel")
                                    .dock_panel_dock(Dock::Right)
                                    .tool_tip_tip("Hide the Effects panel")
                                    .command(act(move || sink.show_effects.set(false))),
                                )
                                .child(TextBlock::el().text("Effects").font_size(15.0).font_weight(FontWeight::DemiBold).vertical_alignment(VerticalAlignment::Center)),
                        ),
                )
                .child(note(theme, "Applies to the whole image. Undo takes one back off.").dock_panel_dock(Dock::Top).margin((14.0, 8.0, 14.0, 0.0)))
                .child(ScrollViewer::el().vertical_scroll_bar_visibility(ScrollBarVisibility::Auto).content(groups)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_tool_uses_a_unique_vector_icon_and_accessible_description() {
        let mut seen = HashSet::new();
        for def in &TOOLS {
            assert!(def.path.starts_with("M "), "invalid SVG geometry for {}", def.name);
            assert!(!def.description.is_empty(), "missing description for {}", def.name);
            assert!(seen.insert(def.path), "duplicate icon path on {}", def.name);
        }
        assert!(seen.insert(vicons::MORE));
    }

    #[test]
    fn tooltips_include_shortcuts_for_first_nine_tools() {
        let first = format_tool_tooltip("", 0);
        assert!(first.contains("Select Region") && first.contains("1") && first.contains("Select a rectangular area"));
        let tenth = format_tool_tooltip("", 9);
        assert!(tenth.starts_with("Highlight — "));
    }

    #[test]
    fn layout_puts_the_rest_under_more() {
        let (fits, more) = layout("", 3, None);
        assert_eq!(fits, [0, 1, 2]);
        assert_eq!(more.len(), TOOLS.len() - 3);
    }

    #[test]
    fn an_extra_tool_takes_the_last_place() {
        let (fits, more) = layout("", 3, Some(10));
        assert_eq!(fits, [0, 1, 10]);
        assert_eq!(more[0], 2);
        assert!(!more.contains(&10));
    }

    #[test]
    fn hidden_tools_go_under_more() {
        let (fits, more) = layout("Arrow,Text", 5, None);
        assert_eq!(fits, [2, 5]);
        assert!(more.contains(&0));
    }

    #[test]
    fn effects_outside_the_groups_go_under_more() {
        let names = vec!["Border".to_owned(), "Sepia".to_owned(), "Sparkle".to_owned()];
        let groups = grouped_effects(&names);
        assert_eq!(groups.iter().map(|(g, _)| g.as_str()).collect::<Vec<_>>(), ["Border", "Filters", "More"]);
    }
}
