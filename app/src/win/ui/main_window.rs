//! The editor window:
//! the menu bar and toolbar on top, the library, status, narration and video
//! bars along the bottom, the properties and Effects panels on the right and
//! the canvas in the middle. The parts are built in the `main_*` modules.
//!
//! What the main view model asks of other windows (the Capture Widget, Grab
//! Text, toasts, Preferences, the overlay, Screen Draw and the capture HUD)
//! is handled by those windows' `install` functions, not here.

mod main_bars;
mod main_common;
mod main_library;
mod main_menu;
mod main_panel;
mod main_tools;
pub(super) mod menu_entry;
mod video_timeline;

use super::canvas_view::{CanvasView, FLAG_CHANGED};
use super::icons::vector as vicons;
use self::main_bars::{narration_bar, status_bar, video_bar, ZoomView};
use self::main_common::{brand_text, call, rule, run, solid, Ctx, Theme, Tone, RED, TRANSPARENT, WHITE};
use self::main_library::library_panel;
use self::main_menu::{create_flyout, share_flyout};
use self::main_panel::{side_bar, side_bar_splitter};
use self::main_tools::{effects_panel, tool_bar};
use super::{shortcut, UiOnly};
use crate::vm::{MainBinding, MainViewModelSink};
use rustolonia::{
    AnyEl, AppScope, Border, Button, Dock, DockPanel, DragDropEffects, Effect, El, FileDropEvent, FlyoutBase, FontWeight, HorizontalAlignment, Key,
    MenuFlyout, NodeRef, Orientation, Path, Result, StackPanel, TextBlock, TextBox, ThemeVariant, ToggleButton, Upcast, VerticalAlignment, Window,
};
use std::cell::Cell;

/// `WindowCloseReason.ApplicationShutdown` and `OSShutdown`.
const CLOSE_APPLICATION_SHUTDOWN: rustolonia::WindowCloseReason = rustolonia::WindowCloseReason::ApplicationShutdown;
const CLOSE_OS_SHUTDOWN: rustolonia::WindowCloseReason = rustolonia::WindowCloseReason::OSShutdown;

/// Builds the editor window for `binding` (not yet shown).
pub fn build(scope: &AppScope, binding: &MainBinding) -> Result<Window> {
    let sink = binding.sink;
    let canvas = CanvasView::new(scope)?;
    super::set_canvas(canvas.clone());
    let ctx = Ctx {
        scope: scope.clone(),
        sink,
        theme: Theme::new(),
        canvas: canvas.clone(),
        window_height: rustolonia::Signal::new(960.0),
        text_editor: NodeRef::new(),
    };
    let zoom = ZoomView::new();
    let theme = ctx.theme;
    // The actual variant, which the "System" theme changes with the system.
    canvas.follow_theme(theme.dark)?;
    let window_height = ctx.window_height;
    let live_side_bar_width = rustolonia::Signal::new(crate::settings::clamp_side_bar_width(sink.side_bar_width.get_untracked()));
    let content = DockPanel::el();
    #[cfg(not(target_os = "macos"))]
    let content = content.child(main_menu::menu_bar(&ctx).dock_panel_dock(Dock::Top));
    let window = Window::el()
        .title(sink.title)
        .width(1500.0)
        .height(960.0)
        .min_width(900.0)
        .min_height(600.0)
        .icon(super::icon_file())
        .background(theme.brush(Tone::Window))
        .on_size_changed(move |args| window_height.set_if_changed(args.new_size.height))
        .content(
            content
                .child(top_bar(&ctx))
                .child(library_panel(&ctx))
                .child(status_bar(&ctx, zoom))
                .child(narration_bar(sink))
                .child(video_bar(&ctx))
                .child(side_bar(&ctx, live_side_bar_width))
                .child(side_bar_splitter(&ctx, live_side_bar_width))
                .child(effects_panel(&ctx))
                .child(AnyEl::from_control(canvas.control().clone())),
        )
        .build(scope)?;
    #[cfg(target_os = "macos")]
    main_menu::install_native(&ctx, &window)?;
    follow_theme(scope, &window, sink, theme)?;
    wire_canvas(&ctx, zoom)?;
    wire_window(&ctx, &window)?;
    #[cfg(windows)]
    {
        // Give Avalonia the saved position, size and maximized state before the window
        // is shown: `Show` applies its own `Width`, `Height` and `WindowState`, so these
        // cannot be restored natively during `Loaded`.
        if let Some(startup) = super::editor_placement::startup() {
            let applied = window
                .set_width(startup.width)
                .and_then(|()| window.set_height(startup.height))
                .and_then(|()| window.set_position((startup.x, startup.y)))
                .and_then(|()| {
                    if startup.maximized {
                        window.set_window_state(rustolonia::WindowState::Maximized)
                    } else {
                        Ok(())
                    }
                });
            if let Err(error) = applied {
                eprintln!("Could not restore the editor placement: {error}");
            }
        }
        let app = scope.clone();
        let editor = UiOnly::new(window.clone());
        window.clone().on_loaded(scope, move |_| {
            let result = app.platform_handle(editor.get()).map_err(std::io::Error::other).and_then(|handle| {
                super::editor_placement::install(handle as windows_sys::Win32::Foundation::HWND)
            });
            if let Err(error) = result {
                eprintln!("Could not position the editor: {error}");
            }
        })?;
    }
    Ok(window)
}

#[cfg(target_os = "macos")]
pub(super) fn library_tile_for_selftest(scope: &AppScope, key: u64) -> Result<Window> {
    let tile = main_library::tile(super::main().sink, Theme::new(), key);
    let window = Window::el().title("Library drag fixture").width(300.0).height(220.0).content(tile).build(scope)?;
    scope.mount(window.clone())?;
    Ok(window)
}

/// The Editor and Library tabs, capture and creation actions, the tool palette,
/// the Properties and Effects toggles, Copy All and Share ▾.
fn top_bar(ctx: &Ctx) -> El<Border> {
    let (s, theme) = (ctx.sink, ctx.theme);
    let expanded = s.library_expanded;
    // Measured directly on the bar so initial X11/Xvfb layouts at 800–900 px
    // keep the exact 3-tool compact toolbar budget (`Select`, `Move`, `Arrow`, `More`).
    let bar_width = rustolonia::Signal::new(0.0_f64);
    let wide_chrome = move || bar_width.get() >= 1100.0;
    let tab = move |label: &str, library: bool, tip: &str| {
        let on = move || expanded.get() == library;
        let underline_on = on;
        let weight_on = on;
        Button::el()
            .name(if library { "TabLibrary" } else { "TabEditor" })
            .tool_tip_tip(tip.to_owned())
            .padding((12.0, 5.0, 12.0, 4.0))
            .corner_radius(4.0)
            .background(move || if on() { theme.get(Tone::Well) } else { solid(TRANSPARENT) })
            .content(
                StackPanel::el()
                    .spacing(3.0)
                    .child(
                        TextBlock::el()
                            .text(label.to_owned())
                            .horizontal_alignment(HorizontalAlignment::Center)
                            .font_weight(move || if weight_on() { FontWeight::DemiBold } else { FontWeight::Normal }),
                    )
                    .child(
                        Border::el()
                            .height(2.0)
                            .corner_radius(1.0)
                            .background(move || if underline_on() { solid(brand_text(theme)) } else { solid(TRANSPARENT) }),
                    ),
            )
            .command(main_common::act(move || expanded.set_if_changed(library)))
    };
    // Panel toggles (`Properties`, `Effects`) keep their icon on narrow windows and
    // show their text label when wide; action buttons (`Create`, `Copy All`, `Share`)
    // keep their text label on narrow windows and add their leading icon when wide.
    let toggle_content = move |path_data: &'static str, label: &str, active: rustolonia::Signal<bool>| {
        let icon = Path::el()
            .width(14.0)
            .height(14.0)
            .data(path_data)
            .stroke(move || Some(if active.get() { solid(WHITE) } else { theme.get(Tone::Text) }))
            .stroke_thickness(1.4)
            .vertical_alignment(VerticalAlignment::Center);
        let text = TextBlock::el()
            .text(label.to_owned())
            .margin((6.0, 0.0, 0.0, 0.0))
            .vertical_alignment(VerticalAlignment::Center)
            .visible(wide_chrome)
            .foreground(move || if active.get() { solid(WHITE) } else { theme.get(Tone::Text) });
        StackPanel::el()
            .orientation(Orientation::Horizontal)
            .vertical_alignment(VerticalAlignment::Center)
            .child(icon)
            .child(text)
    };
    let action_content = move |path_data: &'static str, label: &str, chevron: bool| {
        let icon = Path::el()
            .width(14.0)
            .height(14.0)
            .margin((0.0, 0.0, 6.0, 0.0))
            .data(path_data)
            .stroke(move || Some(theme.get(Tone::Text)))
            .stroke_thickness(1.4)
            .vertical_alignment(VerticalAlignment::Center)
            .visible(wide_chrome);
        let text = TextBlock::el()
            .text(label.to_owned())
            .vertical_alignment(VerticalAlignment::Center);
        let row = StackPanel::el()
            .orientation(Orientation::Horizontal)
            .vertical_alignment(VerticalAlignment::Center)
            .child(icon)
            .child(text);
        if chevron {
            row.child(
                Path::el()
                    .width(11.0)
                    .height(11.0)
                    .margin((5.0, 0.0, 0.0, 0.0))
                    .data(vicons::CHEVRON_DOWN)
                    .stroke(move || Some(theme.get(Tone::Caption)))
                    .stroke_thickness(1.4)
                    .vertical_alignment(VerticalAlignment::Center),
            )
        } else {
            row
        }
    };
    let capture_content = StackPanel::el()
        .orientation(Orientation::Horizontal)
        .spacing(6.0)
        .vertical_alignment(VerticalAlignment::Center)
        .child(
            Path::el()
                .width(12.0)
                .height(12.0)
                .data(vicons::CAPTURE_DOT)
                .fill(solid(WHITE))
                .vertical_alignment(VerticalAlignment::Center),
        )
        .child(TextBlock::el().text("Capture").foreground(solid(WHITE)).font_weight(FontWeight::DemiBold).vertical_alignment(VerticalAlignment::Center));

    let left = StackPanel::el()
        .dock_panel_dock(Dock::Left)
        .orientation(Orientation::Horizontal)
        .spacing(6.0)
        .vertical_alignment(VerticalAlignment::Center)
        .child(StackPanel::el().orientation(Orientation::Horizontal).spacing(2.0).child(tab("Editor", false, "Edit the current capture")).child(tab("Library", true, &shortcut("Browse all captures (Ctrl+L)"))))
        .child(rule(theme, (4.0, 6.0)))
        .child(
            Button::el()
                .name("OpenCaptureWindow")
                .content(capture_content)
                .padding((11.0, 5.0))
                .corner_radius(4.0)
                .tool_tip_tip(shortcut("Open the Capture window (Ctrl+Shift+N)"))
                .background(solid(RED))
                .foreground(solid(WHITE))
                .command(main_common::command(s.open_capture_window_enabled.local(), |m| m.open_capture_window())),
        )
        .child(
            with_flyout(
                Button::el()
                    .name("CreateMenu")
                    .content(action_content(vicons::CREATE_PLUS, "Create", true))
                    .padding((10.0, 5.0))
                    .corner_radius(4.0)
                    .tool_tip_tip("Create an image from a template, combine images, or build a video from images"),
                create_flyout(ctx),
            ),
        )
        .child(rule(theme, (4.0, 6.0)));
    let right = StackPanel::el()
        .dock_panel_dock(Dock::Right)
        .orientation(Orientation::Horizontal)
        .spacing(6.0)
        .vertical_alignment(VerticalAlignment::Center)
        .child(rule(theme, (4.0, 6.0)))
        .child(
            ToggleButton::el()
                .name("TogglePropertiesPanel")
                .content(toggle_content(vicons::PROPERTIES_SLIDERS, "Properties", s.show_side_bar.local()))
                .checked(s.show_side_bar.two_way())
                .padding((10.0, 5.0))
                .corner_radius(4.0)
                .tool_tip_tip("Show or hide the Properties panel"),
        )
        .child(
            ToggleButton::el()
                .name("ToggleEffectsPanel")
                .content(toggle_content(vicons::EFFECTS_SPARKLE, "Effects", s.show_effects.local()))
                .checked(s.show_effects.two_way())
                .padding((10.0, 5.0))
                .corner_radius(4.0)
                .tool_tip_tip("Show or hide the Effects panel"),
        )
        .child(
            Button::el()
                .name("CopyAllImage")
                .content(action_content(vicons::COPY_SHEETS, "Copy All", false))
                .padding((10.0, 5.0))
                .corner_radius(4.0)
                .tool_tip_tip(shortcut("Copy the whole image (Ctrl+Shift+C)"))
                .command(main_common::command(s.copy_image_enabled.local(), |m| m.copy_image())),
        )
        .child(
            with_flyout(
                Button::el()
                    .name("ShareMenu")
                    .content(action_content(vicons::SHARE_NODE, "Share", true))
                    .padding((10.0, 5.0))
                    .corner_radius(4.0)
                    .tool_tip_tip("Send the capture to another app, export to a file, or print"),
                share_flyout(ctx),
            ),
        );
    Border::el()
        .dock_panel_dock(Dock::Top)
        .background(theme.brush(Tone::Panel))
        .padding((8.0, 4.0))
        .border_brush(theme.brush(Tone::Border))
        .border_thickness((0.0, 0.0, 0.0, 1.0))
        .on_size_changed(move |args| bar_width.set_if_changed(args.new_size.width))
        .child(StackPanel::el().spacing(4.0).child(DockPanel::el().child(left).child(right).child(tool_bar(ctx))))
}

/// A button that opens `flyout` when clicked.
fn with_flyout(button: El<Button>, flyout: El<MenuFlyout>) -> El<Button> {
    button.with_op(move |scope, button| {
        let flyout: FlyoutBase = flyout.build(scope)?.upcast();
        button.set_flyout(Some(&flyout))
    })
}

/// The one color theme, for every window: `theme` ("Dark", "Light" or
/// "System") sets the application's theme variant, and the window's own
/// colors follow the variant it ends up with.
fn follow_theme(scope: &AppScope, window: &Window, sink: MainViewModelSink, theme: Theme) -> Result<()> {
    #[cfg(windows)]
    {
        let app = scope.clone();
        let native_window = UiOnly::new(window.clone());
        window.clone().on_loaded(scope, move |_| super::native_theme::apply(&app, native_window.get()))?;
    }
    let app = scope.clone();
    #[cfg(windows)]
    let native_window = UiOnly::new(window.clone());
    Effect::new(move || {
        let variant = match sink.theme.get().as_str() {
            "Light" => ThemeVariant::Light,
            "System" => ThemeVariant::Default,
            _ => ThemeVariant::Dark,
        };
        app.set_requested_theme_variant(variant)?;
        theme.dark.set_if_changed(super::capture_view::parts::dark_theme(&app));
        #[cfg(windows)]
        super::native_theme::apply(&app, native_window.get());
        Ok(())
    })?;
    // The system's theme can change under "System".
    let app = scope.clone();
    #[cfg(windows)]
    let native_window = UiOnly::new(window.clone());
    window.clone().on_actual_theme_variant_changed(scope, move |_| {
        theme.dark.set_if_changed(super::capture_view::parts::dark_theme(&app));
        #[cfg(windows)]
        super::native_theme::apply(&app, native_window.get());
    })?;
    Ok(())
}

/// The canvas follows the model (frames, zoom requests, the library view)
/// and reports edits, double-clicks, Escape, zoom changes and dropped files.
fn wire_canvas(ctx: &Ctx, zoom: ZoomView) -> Result<()> {
    let (s, canvas) = (ctx.sink, ctx.canvas.clone());
    let refresh = canvas.clone();
    Effect::new(move || {
        s.frame_id.get();
        refresh.refresh();
        Ok(())
    })?;
    let zoomer = ctx.canvas_handle();
    let scope = ctx.scope.clone();
    Effect::new(move || {
        let request = s.zoom_request.get();
        let verb = request.split(':').next().unwrap_or_default().to_owned();
        if !verb.is_empty() {
            // After layout, so the canvas has its new size.
            let canvas = zoomer.clone();
            scope.post(move || canvas.get().zoom_to(&verb))?;
        }
        Ok(())
    })?;
    let control = canvas.control().clone();
    Effect::new(move || control.set_visible(!s.library_expanded.get()))?;
    canvas.on_changed(|flags| {
        if flags & FLAG_CHANGED != 0 {
            call(|m| m.canvas_changed());
        }
    });
    let (scope, editor) = (ctx.scope.clone(), ctx.text_editor);
    canvas.on_edit_requested(move || {
        run("finish");
        focus_text(&scope, editor);
    });
    canvas.on_cancel_requested(|| run("deselect"));
    #[cfg(target_os = "macos")]
    canvas.on_delete_requested(move || {
        if !s.delete_enabled.get_untracked() {
            return false;
        }
        call(|m| m.delete());
        true
    });
    canvas.on_zoom_changed(move |z| zoom.show(z));
    // Files dropped on the canvas go where they were dropped; the drop
    // carries no position, so that is the middle of the image.
    let dropped_on = ctx.canvas_handle();
    ctx.scope.on_file_drop(canvas.control(), DragDropEffects::COPY, move |event| {
        let FileDropEvent::Drop { items, .. } = event else { return };
        let paths: Vec<Vec<u8>> = items
            .iter()
            .filter_map(|item| item.local_path())
            .map(crate::app::App::encode_drop_path)
            .collect();
        if paths.is_empty() {
            return;
        }
        let (w, h) = dropped_on.get().document_size();
        let paths = serde_json::to_string(&paths).expect("serializing file paths cannot fail");
        run(format!("drop:{},{}|{paths}", f64::from(w) / 2.0, f64::from(h) / 2.0));
    })?;
    // The properties' text box takes focus when the model asks.
    let (scope, editor, first) = (ctx.scope.clone(), ctx.text_editor, Cell::new(true));
    Effect::new(move || {
        s.focus_text.get();
        if !first.replace(false) {
            focus_text(&scope, editor);
        }
        Ok(())
    })?;
    Ok(())
}

/// Focuses the properties' text box and selects its text, once the panel
/// shows it.
fn focus_text(scope: &AppScope, editor: NodeRef<TextBox>) {
    super::capture_view::parts::post(scope, "Focus editor text", move || {
        let Some(editor) = editor.get() else { return Ok(()) };
        if editor.is_visible().unwrap_or(false) {
            let _ = editor.focus_with_navigation_method_and_key_modifiers(rustolonia::NavigationMethod::Unspecified, rustolonia::KeyModifiers::NONE);
            let _ = editor.select_all();
        }
        Ok(())
    });
}

/// Escape deselects; closing the window hides it (Jiffshot keeps running in
/// the tray) except when Windows or the application is shutting down.
fn wire_window(ctx: &Ctx, window: &Window) -> Result<()> {
    window.clone().on_key_down(&ctx.scope, |args| {
        if !args.handled && args.key == Key::Escape {
            run("deselect");
            args.handled = true;
        }
    })?;
    // The UI self-test's window really closes, which ends it.
    if std::env::var_os("JIFFSHOT_UITEST").is_none() {
        let (scope, hidden) = (ctx.scope.clone(), UiOnly::new(window.clone()));
        window.clone().on_closing(&ctx.scope, move |args| {
            if matches!(args.close_reason, CLOSE_APPLICATION_SHUTDOWN | CLOSE_OS_SHUTDOWN) {
                return;
            }
            args.cancel = true;
            let window = hidden.clone();
            let _ = scope.post(move || {
                let _ = window.get().hide();
                run("hidden");
            });
        })?;
    }
    Ok(())
}
