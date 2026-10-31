# Architecture

Jiffshot is one executable and one process (`jiffshot.exe` on Windows,
`jiffshot` on Linux), written in Rust
only, with every window on Rustolonia's code-first API (Avalonia): the editor,
the tray icon, and the capture side's overlay, panels and dialogs.

## Platform boundaries

`app/src/main.rs` selects the backend. `app/src/win/` and `app/src/linux/`
contain platform-specific application, UI, capture and desktop integrations;
their `mod.rs` files declare modules and export the entry point. The document
engine, canvas, tracing and library-tag helpers remain shared, including the
canvas view under `app/src/ui/`.

Windows capture primitives are isolated in `crates/jiffshot-capture/src/win/`.
The capture crate root keeps the existing public module paths and shared frame,
geometry and stitching code. The Windows workflow engine remains under
`crates/jiffshot-workflow/`; Linux workflows live in `app/src/linux/`, using
XDG portals, PipeWire and GStreamer rather than Win32 capture APIs.

Shared document text is rasterized with `ab_glyph`, loading system font files
from explicit directories rather than through Fontconfig. Ubuntu and Fedora
DejaVu font locations are supported; installing a font that Fontconfig can find
is not sufficient if its directory is absent from the document font search.

## Windows UI and view models

`app/src/win/vm.rs` holds the view models: each window's values are signals the
model code publishes to (`app.rs` and friends call the sinks, from any
thread), and each window sends what the user does back through the model
traits. `app/src/win/ui/` builds the windows from those signals. Two Rustolonia
pieces cover what ordinary controls cannot: pixel surfaces (the editor
canvas, the selection overlay and Screen Draw draw Rust pixels and take the
raw pointer stream) and a Rust-built tray icon.

## Windows threads

- **UI thread (Avalonia).** Every window: the editor, the Capture window, the
  tray icon and its menu, toasts, the Capture Widget, Grab Text, the capture
  panels and outline, the selection overlay, Screen Draw and Preferences. The
  editor's `App` lives here (`app/src/win/app.rs`, reached with `app::post`).
- **Capture thread** (`jiffshot-capture`, `crates/jiffshot-workflow`). A
  message-only window for hotkeys, timers (delay, time-lapse, schedule), the
  Step Capture mouse hook, and the capture flows. It talks to the app through
  `Hooks` (open a capture in the editor, toasts, tray state, the HUD and the
  overlay as JSON) and takes work through `on_capture_thread`.
- **Workers.** Finishing captures, recording (`recorder::record_to`) and
  scrolling captures.
- **Control server** (`crates/jiffshot-workflow/src/control.rs`). A thread per
  `jiffshot-cli` connection on a named pipe only this user can open
  (`crates/jiffshot-control`). Commands run on that thread: screenshots,
  recordings, Step Capture guides, SendInput, OCR and UI Automation. Headless
  editable document sessions reuse `jiffshot-doc::Editor` without replacing the
  user's GUI document; live recording annotations are composited by the recorder.
  File-based video commands and the GUI share `jiffshot-capture::media::post`
  for trimming, joining, overlays and exports. See
  [cli.md](cli.md).

A second launch hands its request to the running one (`--capture`, a file to
open), passing on its right to take the foreground.

## Windows overlay

`overlay.rs` is the selection logic only; `Overlay::view` says what to show.
`ui_api.rs` is the table of calls the app's overlay windows use
(`app/src/win/ui/overlay/overlay_api.rs`): input in (desktop pixels), state, texts and
points out, and the snapshot copied straight into each window's pixel
surface. One window per monitor (`app/src/win/ui/overlay.rs`) is made at startup
and only hidden natively between captures; a capture loads the new
screenshot, waits about a frame, then shows the windows natively in physical
pixels.

Screen Draw's marks live in Rust and render to one image: its window shows
it (left out of captures) and the recorder blends it into every frame.

## Windows settings

One file, `%APPDATA%\Jiffshot\jiffshot.json`
(`crates/jiffshot-capture/src/win/config.rs`):

```json
{ "library": "D:\\Captures", "theme": "Dark", "capture": { }, "editor": { } }
```

Each side owns its section; the library folder and the color theme are shared
at the top, so captures are saved where the editor lists them and every
window has the same theme. Reads and writes share one lock.
`JIFFSHOT_LIBRARY` overrides the library for both sides without being saved.

## Windows preferences

One dialog (`app/src/win/ui/prefs_window.rs`): Capture, Editor, Library, Share and
Hotkeys. Rust describes each tab as rows (`app/src/win/prefs.rs`, and
`jiffshot_workflow::prefs` for the capture side's tabs) and applies the values
the dialog sends back. It opens from the editor's Edit menu (Ctrl+,), the
Capture window's File and Edit menus, and the tray.

## Checking it

See the README's Testing section; for live checks, `JIFFSHOT_CAPTURE_DRY_RUN`
runs the overlay without saving anything, and `JIFFSHOT_HUD_CAPTURABLE` lets
screenshots see the panels.
