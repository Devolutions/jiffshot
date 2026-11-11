//! A draggable video selection with independent playhead and visible cut joins.

use super::main_common::{Theme, Tone};
use crate::ui::UiOnly;
use crate::vm::MainViewModelSink;
use rustolonia::pixel_surface::{
    Interpolation, MouseButton, PixelFormat, Placement, PointerKind, SurfaceInput, SurfaceMetrics,
};
use rustolonia::{Border, Effect, El, PixelSurface, Result};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

const INSET: f64 = 10.0;
const HANDLE_HIT: f64 = 8.0;

#[derive(Clone, Copy)]
enum Drag {
    Selection(f64),
    Playhead,
}

impl Drag {
    fn previews(self, kind: PointerKind) -> bool {
        matches!(self, Self::Playhead) || kind == PointerKind::Released
    }
}

struct TimelineView {
    surface: PixelSurface,
    sink: MainViewModelSink,
    theme: Theme,
    metrics: Cell<SurfaceMetrics>,
    layer: Cell<(u32, u32)>,
    drag: Cell<Option<Drag>>,
}

pub(super) fn timeline(sink: MainViewModelSink, theme: Theme) -> El<Border> {
    Border::el()
        .height(54.0)
        .tool_tip_tip("Drag the track to select a section; drag the top strip to scrub. Delete cuts the selection; Ctrl+Z undoes a cut.")
        .with_op(move |scope, border| {
            let slot: Arc<OnceLock<UiOnly<Rc<TimelineView>>>> = Arc::default();
            let input = slot.clone();
            let surface = scope.create_pixel_surface(move |event| {
                match input.get() {
                    Some(view) => view.get().input(event),
                    None => Ok(false),
                }
            })?;
            border.set_child(surface.control())?;
            let view = Rc::new(TimelineView {
                surface,
                sink,
                theme,
                metrics: Cell::new(SurfaceMetrics::default()),
                layer: Cell::new((0, 0)),
                drag: Cell::new(None),
            });
            let _ = slot.set(UiOnly::new(view.clone()));
            Effect::new(move || view.draw())?;
            Ok(())
        })
}

impl TimelineView {
    fn x(&self, time: f64) -> f64 {
        let width = (self.metrics.get().width - 2.0 * INSET).max(1.0);
        INSET + time / self.sink.video_duration.get().max(0.001) * width
    }

    fn time(&self, x: f64) -> f64 {
        let width = (self.metrics.get().width - 2.0 * INSET).max(1.0);
        ((x - INSET) / width).clamp(0.0, 1.0) * self.sink.video_duration.get()
    }

    fn input(&self, input: &SurfaceInput) -> Result<bool> {
        match input {
            SurfaceInput::Resize(metrics) => {
                self.metrics.set(*metrics);
                self.draw()?;
                Ok(true)
            }
            SurfaceInput::Pointer(pointer) => {
                if self.sink.video_saving.get() {
                    return Ok(false);
                }
                let at = self.time(pointer.x);
                match pointer.kind {
                    PointerKind::Pressed if pointer.button == MouseButton::Left => {
                        let start = self.sink.trim_start.get().min(self.sink.trim_end.get());
                        let end = self.sink.trim_start.get().max(self.sink.trim_end.get());
                        let drag = if pointer.y < 12.0 {
                            Drag::Playhead
                        } else if end > start && (pointer.x - self.x(start)).abs() <= HANDLE_HIT {
                            Drag::Selection(end)
                        } else if end > start && (pointer.x - self.x(end)).abs() <= HANDLE_HIT {
                            Drag::Selection(start)
                        } else {
                            Drag::Selection(at)
                        };
                        self.drag.set(Some(drag));
                        self.move_to(drag, at, pointer.kind);
                        Ok(true)
                    }
                    PointerKind::Moved | PointerKind::Released => {
                        if let Some(drag) = self.drag.get() {
                            self.move_to(drag, at, pointer.kind);
                            if pointer.kind == PointerKind::Released {
                                self.drag.set(None);
                            }
                            Ok(true)
                        } else {
                            Ok(false)
                        }
                    }
                    PointerKind::CaptureLost => {
                        self.drag.set(None);
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            }
            _ => Ok(false),
        }
    }

    fn move_to(&self, drag: Drag, at: f64, kind: PointerKind) {
        crate::app::post(move |app| {
            if let Drag::Selection(anchor) = drag {
                crate::video::select(app, anchor, at);
            }
            if drag.previews(kind) {
                crate::video::seek(app, at);
            }
        });
    }

    fn draw(&self) -> Result<()> {
        let duration = self.sink.video_duration.get();
        let position = self.sink.video_position.get();
        let (start, end) = (self.sink.trim_start.get(), self.sink.trim_end.get());
        let joins = self.sink.video_joins.get();
        let dark = self.theme.dark.get();
        let metrics = self.metrics.get();
        let (width, height) = (
            (metrics.width * metrics.scaling).ceil() as u32,
            (metrics.height * metrics.scaling).ceil() as u32,
        );
        if width == 0 || height == 0 {
            return Ok(());
        }
        let Some(mut image) = tiny_skia::Pixmap::new(width, height) else {
            return Err(rustolonia::Error::Load(
                "Could not allocate the video timeline.".into(),
            ));
        };
        let scale = metrics.scaling as f32;
        let color = |argb: u32| {
            tiny_skia::Color::from_rgba8(
                (argb >> 16) as u8,
                (argb >> 8) as u8,
                argb as u8,
                (argb >> 24) as u8,
            )
        };
        image.fill(color(Tone::Panel.argb(dark)));
        let mut rect = |x: f64, y: f64, w: f64, h: f64, argb: u32| {
            if let Some(rect) = tiny_skia::Rect::from_xywh(
                x as f32 * scale,
                y as f32 * scale,
                w.max(1.0) as f32 * scale,
                h as f32 * scale,
            ) {
                let mut paint = tiny_skia::Paint {
                    anti_alias: false,
                    ..Default::default()
                };
                paint.set_color(color(argb));
                image.fill_rect(rect, &paint, tiny_skia::Transform::identity(), None);
            }
        };
        let track_width = (metrics.width - 2.0 * INSET).max(1.0);
        rect(INSET, 15.0, track_width, 24.0, Tone::Well.argb(dark));
        if duration > 0.0 {
            let left = self.x(start.min(end));
            let right = self.x(start.max(end));
            if end != start {
                rect(left, 15.0, right - left, 24.0, 0xFF245B80);
                rect(left - 3.0, 12.0, 6.0, 30.0, 0xFF37A9F5);
                rect(right - 3.0, 12.0, 6.0, 30.0, 0xFF37A9F5);
            }
            for join in joins {
                rect(self.x(join) - 1.0, 17.0, 2.0, 20.0, 0xFFF9A825);
            }
            let playhead = self.x(position);
            rect(playhead - 1.0, 3.0, 2.0, 40.0, Tone::Text.argb(dark));
            rect(playhead - 4.0, 3.0, 8.0, 5.0, Tone::Text.argb(dark));
            for tick in 0..=10 {
                rect(
                    INSET + track_width * f64::from(tick) / 10.0,
                    44.0,
                    1.0,
                    5.0,
                    Tone::Muted.argb(dark),
                );
            }
        }
        if self.layer.get() != (width, height) {
            self.surface
                .set_layer(0, width, height, PixelFormat::Rgba8)?;
            self.layer.set((width, height));
        }
        self.surface
            .write_layer(0, 0, 0, width, height, image.data(), width as usize * 4)?;
        self.surface
            .place_layer(0, Placement::Fill, Interpolation::None, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_handles_only_request_a_preview_on_release() {
        for anchor in [0.0, 5.0, 10.0] {
            let drag = Drag::Selection(anchor);
            assert!(!drag.previews(PointerKind::Pressed));
            for _ in 0..200 {
                assert!(!drag.previews(PointerKind::Moved));
            }
            assert!(drag.previews(PointerKind::Released));
        }
    }

    #[test]
    fn playhead_scrubbing_keeps_requesting_live_previews() {
        for kind in [
            PointerKind::Pressed,
            PointerKind::Moved,
            PointerKind::Released,
        ] {
            assert!(Drag::Playhead.previews(kind));
        }
    }
}
