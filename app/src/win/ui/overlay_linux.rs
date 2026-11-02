//! The selection overlay on Linux: a frozen screenshot of the desktop in one
//! fullscreen window, drawn with the Windows/macOS overlay's renderer and
//! driven by `crate::linux::overlay`'s model. Wayland gives an application no
//! way to read the screen or place windows, so the portal screenshot is taken
//! first and the selection is made on it. UI thread only.

#![allow(clippy::chunks_exact_to_as_chunks)]

#[path = "overlay/overlay_view.rs"]
mod overlay_view;

use crate::linux::overlay::{self as model, Mode, Overlay, Selection};
use jiffshot_capture::{CaptureMapping, Frame, Rect};
use overlay_view::{MonitorWindow, OverlayMonitor, Scene};
use rustolonia::pixel_surface::{MouseButton, PointerKind, SurfaceInput};
use rustolonia::{AppScope, Window, WindowState};
use std::cell::RefCell;

/// The overlay's calls the window renderer makes.
mod overlay_api {
    pub const FLAG_DONE: i32 = 1;
    pub const FLAG_MAGNIFIER: i32 = 2;
    pub const FLAG_HELP: i32 = 4;
    pub const FLAG_GUIDES: i32 = 8;
    pub const FLAG_COLOR: i32 = 16;
    pub const FLAG_DRAGGING: i32 = 32;

    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    pub struct OverlayState {
        pub flags: i32,
        pub cursor: [i32; 2],
        pub lit: [i32; 4],
        pub lit_kind: i32,
        pub monitor: [i32; 4],
        pub scale: f32,
        pub color: [i32; 3],
        pub path_count: i32,
        pub region_count: i32,
    }

    impl OverlayState {
        pub fn has(&self, flag: i32) -> bool {
            self.flags & flag != 0
        }

        pub fn lit(&self) -> jiffshot_capture::Rect {
            let [x, y, w, h] = self.lit;
            jiffshot_capture::Rect::new(x, y, w, h)
        }

        pub fn monitor(&self) -> jiffshot_capture::Rect {
            let [x, y, w, h] = self.monitor;
            jiffshot_capture::Rect::new(x, y, w, h)
        }
    }

    /// The screenshot (BGRA) of `rect`, from the session on the UI thread.
    pub fn copy(_session: u64, rect: jiffshot_capture::Rect, w: u32, h: u32, buf: &mut [u8], stride: usize) -> Result<bool, String> {
        super::copy_snapshot(rect, w, h, buf, stride)
    }
}

/// Where the window's surface shows the screenshot: all of it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SurfaceViewport {
    frame: [f64; 4],
}

impl SurfaceViewport {
    fn placement(self) -> rustolonia::pixel_surface::Placement {
        rustolonia::pixel_surface::Placement::Fill
    }
}

type Done = Box<dyn FnOnce(Option<Selection>, Frame)>;

struct Host {
    model: Overlay,
    window: MonitorWindow,
    scene: Scene,
    done: Option<Done>,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

fn copy_snapshot(rect: Rect, w: u32, h: u32, buf: &mut [u8], stride: usize) -> Result<bool, String> {
    HOST.with(|host| {
        let host = host.borrow();
        let Some(host) = host.as_ref() else { return Ok(false) };
        let frame = &host.model.snapshot;
        if rect != frame.bounds() || (w, h) != (frame.width, frame.height) || stride != w as usize * 4 {
            return Err("the screenshot does not match the window".to_owned());
        }
        buf.copy_from_slice(&frame.pixels);
        Ok(true)
    })
}

/// Shows `frame` for a selection. `done` gets the selection (`None` when it
/// was cancelled) and the screenshot back, on the UI thread.
pub(crate) fn select(
    scope: &AppScope,
    frame: Frame,
    mode: Mode,
    done: impl FnOnce(Option<Selection>, Frame) + 'static,
) -> Result<(), String> {
    if HOST.with(|h| h.borrow().is_some()) {
        return Err("a selection is already open".to_owned());
    }
    let bounds = frame.bounds();
    let mapping = CaptureMapping::identity(bounds).ok_or("the screenshot is empty")?;
    let monitor = OverlayMonitor { rect: bounds, scale: 1.0, pixels: (frame.width, frame.height), mapping };
    let viewport = SurfaceViewport { frame: [0.0, 0.0, frame.width as f64, frame.height as f64] };
    let window = MonitorWindow::new(scope, 1, monitor, viewport, |input| Ok(on_input(input))).map_err(|e| format!("{e:?}"))?;
    let center = (bounds.width / 2, bounds.height / 2);
    let mut scene = Scene::default();
    let model = Overlay::new(frame, center, mode);
    refresh(&model, &mut scene);
    let mut window = window;
    window.load(1)?;
    HOST.with(|h| *h.borrow_mut() = Some(Host { model, window, scene, done: Some(Box::new(done)) }));
    let shown = HOST.with(|h| {
        let h = h.borrow();
        let host = h.as_ref().expect("host");
        let window: &Window = &host.window.window;
        window.set_window_state(WindowState::FullScreen).map_err(|e| format!("{e:?}"))?;
        window.show().map_err(|e| format!("{e:?}"))?;
        let _ = host.window.input.focus();
        Ok::<(), String>(())
    });
    if let Err(error) = shown {
        finish(None);
        return Err(error);
    }
    redraw();
    Ok(())
}

fn refresh(model: &Overlay, scene: &mut Scene) {
    let view = model.view();
    let path: Vec<i32> = model.path().iter().flat_map(|&(x, y)| [x, y]).collect();
    let regions: Vec<i32> = model.regions().iter().flat_map(|r| [r.x, r.y, r.width, r.height]).collect();
    let rect = |r: Rect| [r.x, r.y, r.width, r.height];
    let flag = |on: bool, f: i32| if on { f } else { 0 };
    scene.state = overlay_api::OverlayState {
        flags: flag(view.magnifier, overlay_api::FLAG_MAGNIFIER)
            | flag(view.help, overlay_api::FLAG_HELP)
            | flag(view.color.is_some(), overlay_api::FLAG_COLOR)
            | flag(view.dragging, overlay_api::FLAG_DRAGGING),
        cursor: [view.cursor.0, view.cursor.1],
        lit: rect(view.lit),
        lit_kind: view.lit_kind,
        monitor: rect(view.monitor),
        scale: 1.0,
        color: view.color.map_or([0; 3], |(r, g, b)| [r as i32, g as i32, b as i32]),
        path_count: (path.len() / 2) as i32,
        region_count: (regions.len() / 4) as i32,
    };
    scene.title = view.title.unwrap_or_default();
    scene.size = view.size;
    scene.hint = view.hint.to_owned();
    scene.help = model::HELP.iter().map(|line| (*line).to_owned()).collect();
    scene.path = path;
    scene.regions = regions;
}

fn redraw() {
    HOST.with(|h| {
        if let Some(host) = h.borrow_mut().as_mut() {
            let _ = host.window.draw(&host.scene);
        }
    });
}

/// Ends the overlay, handing `selection` and the screenshot to its caller.
fn finish(selection: Option<Selection>) {
    let Some(host) = HOST.with(|h| h.borrow_mut().take()) else { return };
    host.window.close();
    let Host { model, done, .. } = host;
    if let Some(done) = done {
        done(selection, model.snapshot);
    }
}

fn virtual_key(key: Option<rustolonia::Key>) -> u16 {
    use rustolonia::Key;
    match key {
        Some(Key::Escape) => model::VK_ESCAPE,
        Some(Key::Left) => model::VK_LEFT,
        Some(Key::Up) => model::VK_UP,
        Some(Key::Right) => model::VK_RIGHT,
        Some(Key::Down) => model::VK_DOWN,
        Some(Key::Return) => model::VK_RETURN,
        Some(Key::Space) => model::VK_SPACE,
        Some(Key::Back) => model::VK_BACK,
        Some(Key::F1) => model::VK_F1,
        Some(k) => {
            let n = k as i32;
            if (Key::A as i32..=Key::Z as i32).contains(&n) {
                (b'A' as i32 + n - Key::A as i32) as u16
            } else {
                0
            }
        }
        None => 0,
    }
}

fn on_input(input: &SurfaceInput) -> bool {
    let outcome = HOST.with(|h| {
        let Ok(mut h) = h.try_borrow_mut() else { return None };
        let host = h.as_mut()?;
        match input {
            SurfaceInput::Pointer(p) => {
                let (x, y) = host.window.desktop(p.x, p.y);
                let (x, y) = (x.round() as i32, y.round() as i32);
                host.model.set_modifiers(p.modifiers.shift(), p.modifiers.control());
                match p.kind {
                    PointerKind::Pressed if p.button == MouseButton::Right => host.model.cancel(),
                    PointerKind::Pressed if p.button == MouseButton::Left => host.model.pointer_pressed(x, y),
                    PointerKind::Moved => host.model.pointer_moved(x, y),
                    PointerKind::Released if p.button == MouseButton::Left => host.model.pointer_released(x, y),
                    PointerKind::Pressed | PointerKind::Released => return Some((true, None)),
                    _ => return Some((false, None)),
                }
            }
            SurfaceInput::Key { down, key, modifiers } => {
                host.model.set_modifiers(modifiers.shift(), modifiers.control());
                let vk = if *down { virtual_key(*key) } else { 0 };
                if vk == 0 {
                    return Some((false, None));
                }
                host.model.key(vk, modifiers.shift());
            }
            _ => return Some((false, None)),
        }
        let outcome = host.model.outcome.clone();
        refresh(&host.model, &mut host.scene);
        let _ = host.window.draw(&host.scene);
        Some((true, outcome))
    });
    match outcome {
        Some((handled, Some(selection))) => {
            // Closing the window from its own input handler waits for the handler to return.
            if let Some(scope) = crate::app::scope() {
                let _ = scope.post(move || finish(selection));
            }
            handled
        }
        Some((handled, None)) => handled,
        None => false,
    }
}
