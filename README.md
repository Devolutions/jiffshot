<p align="center">
  <img src="assets/jiffshot.svg" width="96" height="96" alt="Jiffshot logo" />
</p>

<h1 align="center">Jiffshot</h1>

<p align="center">
  <strong>Fast screen capture, recording, and image &amp; video editing for Windows and Linux.</strong>
</p>

<p align="center">
  <a href="https://github.com/Devolutions/jiffshot/releases/latest"><strong>Download Latest Release</strong></a>
  ·
  <a href="https://github.com/Devolutions/jiffshot/releases">All Releases</a>
  ·
  <a href="#features">Features</a>
  ·
  <a href="#keyboard-shortcuts">Shortcuts</a>
  ·
  <a href="#command-line-usage">CLI</a>
  ·
  <a href="#building-from-source">Build from Source</a>
</p>

---

**Jiffshot** is a native screen capture tool, screen recorder, and non-destructive image and video editor for Windows 10/11 and Linux. The Linux build supports opening, annotating, and exporting images and `.jsd` documents, portal-based monitor/window and desktop-selected screenshots, screen text capture, delayed, interval and daily scheduled captures, manual scrolling and panoramic capture, MP4 screen recording with pause/resume, restart/cancel and optional system sound, microphone audio and webcam overlay, video trim/middle-cut/combining/frame import/GIF and silent MP4 export, Tesseract-backed OCR, and opt-in tray operation and portal-based shortcuts. The full Windows video editor, additional global hotkeys, and some desktop integrations remain unavailable on Linux.

<p align="center">
  <img src="docs/images/editor.png" alt="Jiffshot Editor annotating a screenshot" width="900" />
</p>

## Download & Installation

Prebuilt, Authenticode-signed packages for **Windows 10 and Windows 11** (`x64` and `ARM64`) are available on the [**GitHub Releases**](https://github.com/Devolutions/jiffshot/releases) page:

👉 **[Download Jiffshot from GitHub Releases](https://github.com/Devolutions/jiffshot/releases/latest)**

| Package | Architecture | Description |
|---|---|---|
| **`Devolutions.Jiffshot-x64-<version>.msi`** | Windows x64 (Intel / AMD) | Standard Windows Installer. Installs to `C:\Program Files\Devolutions\Jiffshot`, adds a Start Menu shortcut, and associates `.jsd` Jiffshot Document files. |
| **`Devolutions.Jiffshot-arm64-<version>.msi`** | Windows ARM64 | Standard Windows Installer for Windows on ARM devices. |
| **`Devolutions.Jiffshot-x64-<version>.msix`** | Windows x64 (Intel / AMD) | Signed MSIX package. Install it to add Jiffshot to Windows; uninstall it from Installed apps. |
| **`Devolutions.Jiffshot-arm64-<version>.msix`** | Windows ARM64 | Signed MSIX package for Windows on ARM devices. |
| **`Devolutions.Jiffshot-x64-<version>.zip`** | Windows x64 (Intel / AMD) | Portable archive—unzip anywhere and run `jiffshot.exe`. Nothing is installed in the system. |
| **`Devolutions.Jiffshot-arm64-<version>.zip`** | Windows ARM64 | Portable archive for Windows on ARM devices. |

Linux x64 and ARM64 release builds are configured to produce a portable `.tar.gz`
and Ubuntu 24.04 `.deb`, plus a separately built native Fedora 43 `.rpm` for each
architecture. See
[Building and Installing Linux Packages](#building-and-installing-linux-packages)
for their dependencies and current limitations. The Linux GUI requires X11 or
XWayland; a Wayland-only session without XWayland is not supported by the
current Rustolonia host.

> **Note for Windows "N" editions:** Video recording uses Windows Media Foundation, which is omitted from Windows "N" SKUs by default. Install Microsoft's *Media Feature Pack* in Windows Optional Features to enable MP4 recording and playback.

## Quick Start

### Linux

1. **Launch Jiffshot:** Open Jiffshot from the application menu or run `jiffshot`.
2. **Take a capture:** Choose **Capture screen or window…** or **Desktop screenshot chooser…**, then select a source in the desktop portal. A working XDG portal and PipeWire session are required.
3. **Annotate & export:** Captures open in the editor, are saved under `Pictures/Jiffshot` by default, and are copied to the clipboard when available. Use **Export…** for a flattened image or save an editable `.jsd` document.
4. **Record or use shortcuts:** Start and stop recording with **Record screen… (start/stop)**. Optionally enable a global capture shortcut through the desktop portal. Closing the window exits unless tray operation is enabled and a tray host is available.

### Windows

1. **Launch Jiffshot:** Start **Devolutions Jiffshot** from the Start Menu (or run `jiffshot.exe` from the portable folder). Jiffshot places its icon in the Windows notification area (system tray).
2. **Take a capture:** Press **Print Screen** (`PrtScn`) at any time, or left-click the tray icon to open the **Capture Window** and choose a capture mode.
   - **Capture a window or UI element:** Move the pointer over any window and click. Roll the **mouse wheel** up or down while hovering to step between the top-level window and individual child UI controls.
   - **Capture a region:** Click and drag a rectangle. Hold `Shift` for a square, `Ctrl` for `16:9`, or `Ctrl+Shift` for `4:3`.
   - **Capture a full monitor or desktop:** Press `F` for the current screen or `A` for all screens.
3. **Annotate & share:** By default, captures are saved to `Pictures\Jiffshot`, copied to the clipboard, and opened in the **Jiffshot Editor** where you can add arrows, callouts, numbered steps, blur, or effects, then copy (`Ctrl+Shift+C`), export (`Ctrl+E`), or share directly to other apps.
4. **Stay ready in the tray:** Closing the editor window keeps Jiffshot running quietly in the notification area so your hotkeys stay active. To quit completely, right-click the tray icon and choose **Exit**.

---

## Features

The detailed feature list below describes the Windows application. The Linux build currently supports the subset described in [Building from Source](#building-from-source).

### Flexible Screen Capture

<p align="center">
  <img src="docs/images/capture-window.png" alt="Jiffshot Capture Window" width="760" />
</p>

- **All-in-One, Image & Video Tabs:** Use the dedicated Capture Window (or the compact top-of-screen **Capture Widget**) to configure selection modes, automatic effects, share destinations, cursor capture, and timers before starting.
- **Smart Selection Overlay:** Captures from an instant frozen desktop snapshot so transient UI states never move while you aim. Includes a live pixel magnifier with coordinates and `#RRGGBB` color readout, dimension labels, and on-screen help (`F1`).
- **Specialized Capture Modes:**
  - **Scrolling Capture:** Automatically scrolls and stitches vertical, horizontal, or two-axis scrolling windows and web pages.
  - **Panoramic Capture:** Pan across a scrollable area at your own pace and stitch it live.
  - **Freehand, Multi-Area & Fixed Size:** Lasso an arbitrary shape (transparent outside), select multiple distinct rectangles in one shot, or stamp a fixed-pixel rectangle.
  - **Open Menus:** Delayed capture mode designed to grab open context menus and dropdowns.
  - **Grab Text (Windows OCR):** Select any area on screen to extract selectable, copyable text instantly.
  - **Step Capture:** Record a numbered click-by-click guide automatically—every click captures a screenshot and places a sequential step badge at the cursor.
- **Timers, Schedules & Presets:** Delay captures by seconds, run an automatic **time-lapse** (skipping identical frames), schedule a daily capture at a set time (`HH:MM`), or save custom **Presets** with their own global hotkeys.
- **Post-Capture Sharing:** Send captures automatically to the clipboard, the editor, a file, email, Microsoft Word, PowerPoint, Excel, OneNote, Paint, any configured external program, an FTP server (using Windows Credential Manager), or a printer.

<p align="center">
  <img src="docs/images/capture-overlay.png" alt="Jiffshot Selection Overlay with window detection and magnifier" width="820" />
</p>

### Non-Destructive Image Editor & Annotations

- **Vector Annotation Tools:**
  - **Arrows & Lines:** Straight or curved paths with customizable arrowheads/caps, dashed/dotted strokes, drop shadows, and opacity.
  - **Shapes & Callouts:** Rectangles, rounded boxes, ellipses, and multi-point polygons; speech/thought callouts with one or multiple draggable tails and gradient fills.
  - **Text & Step Badges:** Rich text boxes with outlines, fills, alignment, and formatting; auto-incrementing numbered, lettered, or Roman-numeral step markers.
  - **Pen, Highlighter & Stamps:** Freehand drawing, multiply-blend highlighters, built-in icon stamps, flood fill, eraser, magic wand background removal, and eyedropper.
  - **Blur, Magnify & Spotlight:** Pixelate, Gaussian-blur, or solid-redact sensitive regions; add circular/rectangular zoom loupes; dim or blur the background outside one or more spotlight areas.
- **Movable Cursor Object:** Optionally captures the mouse pointer as a separate layer you can move, resize, or delete in the editor.
- **Quick Styles & Themes:** Save custom tool styles, switch Quick Style palettes, customize the toolbar order, and switch between **Dark**, **Light**, and **System** themes across every Jiffshot window.

### Smart Editing Tools

<p align="center">
  <img src="docs/images/editor-smart-tools.png" alt="Jiffshot Smart Tools: Text Replace, Smart Move, Fill, and Spotlight" width="900" />
</p>

- **Text Replace:** Drag over existing text in a bitmap screenshot to recognize it via OCR, cleanly fill the background behind it, and replace it with an editable text object in place.
- **Smart Move:** Drag around a button, card, or UI element to lift it off its background (automatically filling the hole left behind) so you can reposition it anywhere.
- **Smart Redact:** Scan the screenshot with one click to automatically detect and cover sensitive patterns such as email addresses and phone numbers with removable redaction boxes.
- **Simplify UI:** Convert dense UI text across a screenshot into clean, color-matched placeholder bars for simplified documentation graphics.
- **Cut Out & Auto-Trim:** Drag horizontally or vertically across a section to slice it out and join the remaining halves (with a straight seam or torn edge), or auto-trim solid-colored margins in one click.

### Effects, Templates & Image Combining

<p align="center">
  <img src="docs/images/editor-templates-effects.png" alt="Jiffshot Templates and Effects Panel" width="900" />
</p>

- **Whole-Image Effects:** Apply Borders, Torn / Wave / Sawtooth / Faded edges, Drop Shadows, 3D Perspective & Tilt, Page Curl, Reflection, Rounded Corners, Sharpen, Blur, Monochrome, Grayscale, Sepia, Invert, Vignette, Brightness/Contrast, Hue Shift, and Capture Info watermarks.
- **Multi-Section Templates:** Build side-by-side comparisons and multi-step guides with drop zones, automatic section numbers, captions, themes (Light, Dark, Ocean, Warm), and custom footers. Drag images directly from the Recent Captures tray or File Explorer into any drop zone.
- **Combine Images:** Stitch multiple captures horizontally or vertically with configurable spacing and background fill.

### Screen Recording & Built-In Video / GIF Editor

<p align="center">
  <img src="docs/images/recording-toolbar.png" alt="Jiffshot Floating Recording Toolbar" width="680" />
</p>

- **Area, Window & Fullscreen Recording:** Record MP4 video at 15, 24, 30, or 60 FPS. Press `G` while selecting to snap to standard `1920×1080`, `1280×720`, `640×480`, or `320×240` guides.
- **System Audio, Microphone & Webcam:** Record system audio ("what the computer plays"), your microphone (with an optional between-words noise gate), or both mixed together. Add a live circular or rectangular **webcam picture-in-picture** overlay in any corner.
- **Pointer Effects & Live Screen Draw:** Highlight the cursor with a halo, show animated click rings, and draw live arrows, boxes, highlights, and freehand marks directly over the screen while recording (with configurable auto-fade).
- **Live Recording Toolbar:** Pause, resume, restart, minimize, or cancel the recording, and toggle cursor effects, webcam, microphone, system sound, and Screen Draw on the fly—all from a floating toolbar that stays invisible to the recording itself.
- **Video Editing & Animated GIF Export:** Trim the start and end, cut out unwanted middle sections, step frame-by-frame, extract any frame as a still image, save a silent copy, combine videos, record narrated slideshows from still images, or export optimized **Animated GIFs** with presets (*Screen recording*, *High motion*, *Small file*), looping, cross-fade, and dithering.

<p align="center">
  <img src="docs/images/video-editor.png" alt="Jiffshot Video Editor and GIF Export" width="900" />
</p>

### Capture Library & Unified Preferences

<p align="center">
  <img src="docs/images/library.png" alt="Jiffshot Capture Library" width="900" />
</p>

- **Searchable Capture Library:** Every capture is stored in `Pictures\Jiffshot` (grouped into `YYYY-MM` folders) alongside a metadata sidecar recording the window title, application process, browser URL, dimensions, timestamp, tags, and pin status.
- **Instant Filtering & Organization:** Filter by *Images*, *Videos*, *Pinned*, *Today*, *Last 7 days*, *Tagged*, specific applications (`App: ...`), websites (`Site: ...`), or tags (`Tag: ...`). Pin important items to the front, tag or rename captures in place, and select multiple items to combine them into a single image or slideshow.
- **Copy & Drag Out:** Right-click any capture in the Library or the Recent Captures tray and choose **Copy** to put the image (with its edits) on the clipboard; videos and multiple picked captures are copied as files. Drag captures from the tray or Library straight into other applications such as Explorer, Outlook, Teams, or a browser.
- **Editable `.jsd` Documents & Multi-Format Export:** Edits to library captures are saved automatically as non-destructive `.jsd` companion files, so annotations stay movable and editable whenever you reopen a capture. Export flat files to **PNG**, **JPEG**, **PDF**, **BMP**, **GIF**, and **WebP**.
- **One Preferences Dialog:** Configure Capture behavior, Editor layout and theme, Library location, Share destinations, and global Hotkeys from a single window (`Ctrl+,`).

<p align="center">
  <img src="docs/images/preferences.png" alt="Jiffshot Preferences Dialog" width="600" />
</p>

---

## Keyboard Shortcuts

### Global Hotkeys

| Shortcut | Action |
|---|---|
| `Print Screen` (`PrtScn`) | Start an interactive capture (falls back to `Ctrl+Shift+X` or `Ctrl+Alt+PrtScn` if another app holds `Print Screen`) |
| `Ctrl+Print Screen` | Capture the full screen immediately |
| `Shift+Print Screen` | Repeat the last region capture |
| Custom (e.g. `Ctrl+Shift+1`) | Run any saved Capture Preset (**Preferences ▸ Hotkeys**) |

### Selection Overlay Controls

| Input / Key | Action |
|---|---|
| **Left Click** | Capture the highlighted window or UI element under the cursor |
| **Mouse Wheel** | Walk up and down between parent windows and child UI elements |
| **Click + Drag** | Select a custom rectangular region |
| `Shift` / `Ctrl` / `Ctrl+Shift` (while dragging) | Lock aspect ratio to **1:1 square**, **16:9**, or **4:3** |
| **Arrow Keys** (`Shift+Arrows`) | Nudge the pointer by `1 px` (`10 px` with `Shift`) for pixel-exact edges |
| `Shift+Enter` | Start or finish a rectangular selection at the pointer using the keyboard |
| `Enter` / `Space` | Confirm the current selection |
| `F` | Capture the monitor under the cursor |
| `A` | Capture the entire multi-monitor desktop |
| `M` | Show or hide the pixel magnifier |
| `G` | Show or hide standard video resolution guides (`1080p`, `720p`, `480p`, `240p`) |
| `Backspace` | Remove the last rectangle (in Multi-Area capture mode) |
| `F1` | Show or hide the on-screen keyboard help |
| `Esc` | Cancel capture |

### Recording & Screen Draw Shortcuts

| Shortcut | Action |
|---|---|
| `Shift+F9` | Pause or resume recording |
| `Shift+F10` | Stop and save recording |
| `Shift+F8` | Toggle live **Screen Draw** mode while recording |
| `P` / `A` / `L` / `S` / `E` / `H` (in Screen Draw) | Select **Pen**, **Arrow**, **Line**, **Rectangle**, **Ellipse**, or **Highlighter** |
| `1` … `6` (in Screen Draw) | Choose drawing color |
| `Ctrl+Z` / `C` (in Screen Draw) | Undo last mark / clear all Screen Draw marks |
| `Esc` | Exit Screen Draw, or cancel the recording |

### Editor Shortcuts

| Shortcut | Action |
|---|---|
| `Ctrl+Shift+N` | New screen capture |
| `Ctrl+N` / `Ctrl+Shift+V` | New blank image / New image from clipboard |
| `Ctrl+O` | Open file (`.png`, `.jpg`, `.bmp`, `.gif`, `.webp`, `.mp4`, `.jsd`) |
| `Ctrl+S` / `Ctrl+Shift+S` | Save / Save as (`.jsd` document) |
| `Ctrl+E` | Export flattened image (`PNG`, `JPEG`, `PDF`, `BMP`, `GIF`, `WebP`) |
| `Ctrl+Shift+C` | Copy flattened image to clipboard |
| `Ctrl+P` / `Ctrl+W` | Print / Close current capture |
| `Ctrl+Z` / `Ctrl+Y` | Undo / Redo |
| `Ctrl+C` / `Ctrl+X` / `Ctrl+V` | Copy / Cut / Paste selected annotation objects |
| `Ctrl+A` / `Ctrl+D` / `Ctrl+Shift+I` | Select all / Deselect / Invert selection |
| `Ctrl+Shift+G` / `Ctrl+Shift+U` | Group / Ungroup selected objects |
| `Ctrl+Shift+F` / `Ctrl+Shift+B` | Bring forward / Send backward |
| `Ctrl+T` / `Ctrl+Shift+T` | Flatten selected objects / Flatten all objects into background |
| `Ctrl+1` … `Ctrl+9` | Switch between the first nine toolbar tools |
| `Ctrl++` / `Ctrl+-` / `Ctrl+0` / `Ctrl+Alt+0` | Zoom in / Zoom out / Actual size (`100%`) / Fit to window |
| `Ctrl+G` | Toggle pixel grid (when zoomed in) |
| `Ctrl+L` | Toggle full Library view |
| `Ctrl+,` | Open Preferences |
| `F1` | Show Keyboard Shortcuts dialog |

---

## Command-Line Usage

`jiffshot.exe` supports command-line arguments for launching specific capture modes, opening files, or scripting captures from a terminal. If Jiffshot is already running in the tray, launching `jiffshot.exe` a second time forwards the command to the running instance.

```powershell
# Start Jiffshot in the system tray (or open the Capture Window if already running)
jiffshot.exe

# Open one or more image, video, or .jsd files in the editor
jiffshot.exe screenshot.png recording.mp4

# Trigger an interactive capture mode immediately
jiffshot.exe --capture        # Region, window, or screen capture
jiffshot.exe --scrolling      # Scrolling window/page capture
jiffshot.exe --panoramic      # Panoramic capture
jiffshot.exe --text           # Grab Text (OCR) capture
jiffshot.exe --record         # Video screen recording
jiffshot.exe --menu           # Delayed capture for open menus
jiffshot.exe --steps          # Step Capture (numbered click guide)
jiffshot.exe --freehand       # Freehand lasso capture
jiffshot.exe --window         # Open the Capture Window

# Scripted / headless utilities
jiffshot.exe --grab 0,0,1920,1080 out.png   # Capture a screen rectangle (X,Y,W,H) to PNG
jiffshot.exe --grab screen out.png          # Capture the monitor under the cursor to PNG
jiffshot.exe --ocr image.png                # Print recognized OCR text lines to stdout
jiffshot.exe --monitors                     # List connected monitors, bounds, and DPI scale
jiffshot.exe --list-windows                 # List detected window and child-element capture targets
jiffshot.exe --quit                         # Stop the running Jiffshot instance
```

### Automation and AI agents: `jiffshot-cli` (Windows)

`jiffshot-cli.exe`, installed beside `jiffshot.exe`, drives the running Jiffshot (starting it if needed) so a script or an AI agent can screenshot, record and annotate a desktop software demo without anyone clicking through it by hand. Every command prints one JSON object and returns a meaningful exit code. It can click, type, press keys, find text by OCR and controls by UI Automation, and wait for windows or text.

The CLI also supports bounded scrolling captures, live recording annotations,
webcam/audio controls, headless editable image documents (all annotation tools,
undo/redo, transforms, effects and templates), and video trimming/joining,
timed overlays and MP4/GIF exports. Scripted sequences can reuse typed results
between steps and always execute cleanup. See the
[capability coverage and boundaries](docs/cli.md#capability-review-and-boundaries)
for what is supported and what still needs an external tool.

```powershell
jiffshot-cli windows --process notepad
jiffshot-cli record start --window 0x86070A --out C:\demo\demo.mp4 --clicks
jiffshot-cli type "Hello from an agent\n" --window 0x86070A
jiffshot-cli click --window 0x86070A --element File
jiffshot-cli record stop                       # waits for the finished MP4
jiffshot-cli run demo.json                     # a whole scripted sequence
jiffshot-cli help agent                        # the guide for AI agents
```

See [docs/cli.md](docs/cli.md) for every command, the JSON output and scripted sequences.

## Where Captures & Settings Are Stored

- **Captures Library:** `Pictures\Jiffshot` by default (follows Windows Pictures folder redirection such as OneDrive; configurable in **Preferences ▸ Library** or overridden via the `JIFFSHOT_LIBRARY` environment variable).
- **Settings File:** `%APPDATA%\Jiffshot\jiffshot.json` (shared between the capture engine and editor).
- **Thumbnail Cache & Logs:** `%LOCALAPPDATA%\Jiffshot\thumbs` and `%LOCALAPPDATA%\Jiffshot\capture.log`.

---

## Building from Source

Jiffshot is written in Rust with its UI built on [Rustolonia](https://github.com/Devolutions/rustolonia)'s code-first API (Avalonia, with no AXAML or C# in the application). See [docs/architecture.md](docs/architecture.md) for an overview of how the crates and threads fit together.

### Repository Layout

| Path | Contents |
|---|---|
| `crates/jiffshot-capture/` | Monitors and DPI, screen grabs, window targets, clipboard, OCR, media (recording and playback), the library, settings |
| `crates/jiffshot-doc/` | Documents: annotation objects, rendering, editing and undo, effects, smart tools, `.jsd` and image files |
| `crates/jiffshot-workflow/` | The capture side: tray, hotkeys, selection overlay, recording, capture panels, post-capture workflow, command-line modes, the control server for `jiffshot-cli` |
| `crates/jiffshot-control/` | The local control protocol: JSON requests over a per-user named pipe |
| `crates/jiffshot-cli/` | Windows `jiffshot-cli.exe`: the command-line client for scripts and AI agents |
| `app/` | Windows and Linux application entry points, editors, platform integrations, and UI |
| `package/` | Windows MSI/MSIX packaging and Linux tar.gz, DEB, and RPM package builders |

Platform implementations are isolated in `app/src/win/` and `app/src/linux/`,
with suffix-free module names and a thin `app/src/main.rs` dispatcher. The canvas,
document engine, tracing, and library-tag helpers stay shared; the shared canvas
view remains under `app/src/ui/`. Windows capture backends live in
`crates/jiffshot-capture/src/win/`, while the crate root retains its existing
public API and shared frame, geometry, and stitching primitives. The Windows
workflow engine remains in `jiffshot-workflow`; Linux workflows live under
`app/src/linux/`.

### Compiling

Jiffshot requires **Rust 1.88+** and depends on Rustolonia's `v12.1.2` release. The first build downloads the platform's prebuilt host (verified against the checksums pinned at the tag). No .NET SDK or Rustolonia checkout is required to build the application.

```powershell
cargo run --release -p jiffshot
```

The build copies the host next to `jiffshot.exe`, so `target/release` runs as it is; zip `jiffshot.exe` and the host DLLs beside it, with `app/README-portable.txt`, for the portable app.

On Linux (including WSLg), build and run the image/document editor with:

```sh
cargo run --release -p jiffshot
```

The installed `jiffshot` command accepts a single image, video or `.jsd` path.
Use `jiffshot --capture`, `--capture-interactive`, `--grab-text`, `--scrolling`,
`--scrolling-horizontal`, `--panoramic`, `--interval`, or `--record` to start a capture action
directly; scrolling and panning are manual, `--interval` uses saved interval preferences,
and `--record` starts without audio. The installed desktop
entry also offers capture, screen text, scrolling, panoramic, interval and recording actions where the desktop
supports application actions. Use `--help` for the complete Linux command-line
usage. The separate `jiffshot-cli` automation client is currently Windows-only.

File and folder choosers use the XDG FileChooser portal when available and
fall back to the native GTK picker when it is unavailable. Other portal errors
are reported; canceling a chooser does not change the document.

The Linux editor exposes the document engine's annotation and selection tools, undo/redo, zoom, pixel grid, tool-variant cycling, and horizontal, vertical, or grid image combining. Enable **Templates** to create an editable layout with a theme, optional captions, numbers and footer. Select a zone and choose **Paste into zone** to add an image from the clipboard; use the zone controls to fit, crop, clear or swap images (Shift-click selects two zones). Drag-and-drop into template zones is not currently supported on Linux. Enable **Editor properties and effects** to edit the selected text, callout, or step label and adjust annotation colors, stroke width, font, opacity, and emphasis. Style changes also set the current tool's defaults when nothing is selected. This panel also offers **Auto trim**, **Copy recognized text**, **Smart redact**, and **Simplify UI**. Simplify flattens annotations and replaces recognized text with editable bars, and can be undone. Smart redact covers whole recognized lines containing sensitive patterns, but OCR can miss text: review the result before sharing. Redactions in `.jsd` remain removable and do not erase the underlying image; export a flattened image when the underlying content must not be included. Whole-image effects are undoable. For effects with a numeric setting, enter a value in **Effect parameter** using the indicated units and range; leave it blank for the default. Changing the selected effect clears the value. Border and color replacement use the current stroke and fill colors. **Watermark** selects an image and places it in the bottom-right corner at 35% opacity by default; **Capture info** adds available sidecar capture time, window and process metadata, or the current local time when none is available. **Spotlight & magnify** uses the selected annotation's area, or the image center when nothing is selected. It replaces the selected annotation with the effect; Undo restores it. Linux OCR and Text Replace use the system Tesseract engine; install its development libraries and English language data to enable OCR:

```sh
sudo apt-get install clang libclang-dev libleptonica-dev libtesseract-dev tesseract-ocr-eng
```

The **Editor properties and effects** panel also provides controls to select all, clear the selection, duplicate, delete, group, ungroup, reorder, or flatten annotations. Keyboard shortcuts are `Ctrl+A` (select all), `Ctrl+D` (duplicate), `Delete`, `Ctrl+T` (flatten all), and `Ctrl+Shift+I/G/U/F/B/T` (invert selection, group, ungroup, bring forward, send backward, or flatten selected). Text fields keep their normal editing shortcuts.

**Export…** saves the flattened document as PNG, JPEG, BMP, GIF, WebP or a single-page PDF, selected by the filename extension. Saving a `.jsd` separately keeps annotations editable.

The **Capture screen or window…** action uses the XDG ScreenCast portal and PipeWire to capture one selected monitor or window after desktop permission is granted. It opens the image in the editor and saves a PNG copy under `Pictures/Jiffshot` by default. **Choose captures folder…** selects and persists a different library directory in `$XDG_CONFIG_HOME/Jiffshot/settings.json` (or `~/.config/Jiffshot/settings.json`); **Open captures folder** opens the active folder in the desktop file manager. **Record screen… (start/stop)** records the portal-selected source to H.264 MP4 in the same folder. Use **Pause/resume recording** to omit a section from the saved video, and choose a recording action again to stop, even while paused. **Restart recording** discards the current video and starts again with the same source and devices; **Cancel recording** discards it. Stop or cancel during the portal chooser closes the request without saving a video. The capture session and selected audio and webcam devices stay active while paused. Separate recording actions can add the default microphone, the default output's monitor (system sound), or both mixed together as AAC. Audio availability and access follow the active PulseAudio-compatible sound server. Select **Webcam overlay** before recording to add a camera overlay. The webcam preferences near the bottom of the scrollable controls select a V4L2 device path, a rectangular or circular overlay in any corner, and a size from 10% to 50%; choose **Save webcam preferences** before starting. Rectangles use that percentage of the recording dimensions; circles use the recording height and are capped by its width. The default is `/dev/video0` as a 25% rectangle in the bottom-right corner. The camera must expose raw video and be accessible to the user. Changing the camera during recording is not yet supported. Building these backends also requires:

```sh
sudo apt-get install pkg-config libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev gstreamer1.0-pipewire gstreamer1.0-pulseaudio gstreamer1.0-plugins-bad gstreamer1.0-plugins-good gstreamer1.0-plugins-ugly gstreamer1.0-libav
```

Screen capture and recording require a working desktop portal and PipeWire session; WSLg setups that do not expose those services can run the editor but cannot exercise these capture flows.

Set **Recording frame rate** to **Source** (the default), 15, 24, 30, or 60 fps and choose **Save recording frame rate** to apply it to new recordings, including command-line and global-shortcut starts. Restarting an active recording retains the rate chosen when it started. Fixed rates may drop or duplicate frames when the source runs at a different rate; they do not increase the source's actual temporal detail.

**Desktop screenshot chooser…** (also `--capture-interactive`) uses the desktop's XDG Screenshot portal. Depending on the desktop, the chooser may offer a rectangular region, window, or full-screen selection; its available modes and permission prompts are controlled by the desktop. Cancelling saves nothing, and a missing portal is reported. Capture results are still saved and copied when enabled if the editor changed while the chooser was open, but they do not replace newer editor changes.

**Grab text…** (also `--grab-text`) uses that chooser to select a screenshot, recognizes text with English Tesseract data, and copies the result to the text clipboard without changing the editor or saving a Jiffshot capture. If nothing is recognized, the chooser is cancelled, or an error occurs, the clipboard is left unchanged. Recognition can be imperfect, so review the copied text.

For a timed screen or window capture, set **Screen capture delay (seconds)** to 1–60 and choose **Save capture delay**. After selecting a source in the portal, the countdown gives you time to open the content to capture. **Cancel delayed capture** stops the countdown without saving; if the desktop chooser is still open, close it as well. A delay of 0 captures immediately. The setting also applies to `--capture` and the global capture shortcut, but not to the desktop screenshot chooser, scrolling capture, interval capture, daily capture, or recording.

Enable **Screen presets** to save named screen/window capture setups with their own delay and cursor choices. Enter a name, choose the options, and select **Save screen preset**; saving an existing name replaces it. Names are case-sensitive and limited to 128 UTF-8 bytes. Use its **Capture** button to run it, or enter its name and choose **Delete screen preset**. Running a preset still asks the desktop for a source and does not change the normal capture preferences. Presets for other capture modes are not yet available.

Use **Include cursor in captures** and **Include cursor in recordings**, then **Save cursor preferences**, to request the pointer independently for screen, interval and daily captures and recordings. Both default to included; actual behavior depends on the desktop portal and compositor. Scrolling and panoramic capture request a hidden cursor, and the desktop screenshot chooser controls its own cursor behavior.

For manual scrolling capture, select **Horizontal scrolling** before starting if you want to scroll right; leave it unchecked to scroll down. Choose **Scrolling capture… (start/finish)** and select a monitor or window in the portal, then scroll slowly while keeping successive views overlapping. Choose the same control again to finish or **Cancel scrolling** to discard the capture. Cursor hiding is requested from the desktop; lost overlap is reported and the last complete image is saved if the capture reaches its memory or dimension limit. Automatic scrolling is not available on Linux.

For manual two-axis capture, choose **Panoramic capture… (start/finish)** or `--panoramic` and select a monitor or window. Pan slowly in any direction while keeping successive views overlapping, then choose the same control to finish or **Cancel scrolling** to discard it. It works best with stationary toolbars and animations excluded from the selected content. Unseen parts of the output remain transparent; lost overlap and size limits are reported. Panoramic capture, scrolling capture, interval capture, and recording cannot run at the same time.

Enable **Interval options** to set an interval of at least one whole second and optionally **Skip identical frames**. Choose **Interval capture… (start/stop)** and select a screen or window once; the first image is saved immediately and subsequent images follow at the configured interval. Choose the same control to stop; if the desktop chooser is still open, close it as well. The interval and skip preference are saved on start and are also used by `--interval`. Interval captures are saved in the captures folder without opening each image in the editor or copying it to the clipboard. Stop interval capture before starting a recording or scrolling capture, and vice versa.

Enable **Daily schedule options**, enter a local time in `HH:MM`, select **Enable daily capture**, and choose **Save daily schedule**. Jiffshot must remain open at the scheduled time; it requests a screen or window from the desktop portal, which may prompt for selection and authorization each time. The image is saved to the captures folder without replacing the editor or clipboard. The schedule makes at most one attempt per local date, including across restarts and time changes; a declined or failed request, or an active capture or recording, is not retried that day. A scheduled minute skipped by a clock change or daylight-saving transition is missed. Disabling the schedule prevents future attempts but does not close an already open desktop chooser; close it to cancel that request.

Completed screenshots, scrolling captures, and panoramic captures are also copied to the image clipboard by default. To change this, toggle **Copy captures to clipboard** and choose **Save capture preferences**; a clipboard error is reported without discarding the capture. **Copy image** and **Paste image** also use the system image clipboard. Wayland desktops need a supported data-control protocol; when that is unavailable the clipboard backend can fall back to X11/XWayland.

The **Capture library** initially lists the eight newest screenshots and recordings; select one to open its image or import a video's first frame. **Search all**, **Images**, **Videos**, **Pinned**, and **Tagged** search the whole capture folder, including Windows-style `YYYY-MM` subfolders. Search matches filenames and any existing Jiffshot metadata (window title, app, URL, timestamp, and tags). Use `app:browser`, `site:example.com`, or `tag:planning` to match only those metadata fields, and `age:day` or `age:week` to filter by the last 24 hours or seven days of file modification time. Terms can be combined and are case-insensitive; field values match substrings, and unknown prefixes are treated as ordinary text. Enable **Organize captures**, select a capture, and use **Save tags** or **Pin / unpin** to update its sidecar. Tags are comma- or semicolon-separated; existing metadata is preserved, malformed or symbolic-link sidecars are rejected, and changes detected during an edit are reported. New sidecars include image dimensions and Windows-compatible defaults; video dimensions and capture time are left unspecified when unavailable. **Recent** returns to the recent-capture strip. Scroll the controls above the canvas to reach additional options when the window is small. The editor supports Ctrl+O to open, Ctrl+S to save a `.jsd`, and Ctrl+Z/Ctrl+Y for undo/redo. **Theme: System / Light / Dark** switches the app theme; the choice persists alongside the capture-folder setting in the Linux settings file. **Combine horizontal…**, **Combine vertical…**, and **Combine as grid…** accept multiple image files and create an editable document. **Video frame (seconds) / Import frame…** imports a frame at the requested timestamp into the editor. Opening AVI, MKV, MOV, MP4, or WebM also imports its first video frame; **Play video… / Stop** starts or stops GStreamer video and audio playback, and **Pause/resume video** controls playback. Set the **Trim range (seconds)** start and end values, select a video, and choose **Trim video to MP4…** to export that interval or **Cut range out…** to remove it and join the remaining parts. **Combine videos…** joins at least two clips in case-sensitive filename order (full path breaks filename ties), scales them to the first clip's dimensions, and exports at 30 frames per second. These MP4 edits preserve the selected source audio track as AAC; when combining clips with and without audio, silent clips contribute silence. The destination must differ from every source. **Export video as GIF…** creates a looping animated GIF using the source frame timing, rounded to GIF's 10 ms resolution; very short frames may be omitted. The GIF destination must differ from the source.

With **Organize captures** enabled, **Open saved capture with…** and **Email saved capture…** ask the desktop portal to open or attach the selected library file. **Copy saved capture to folder…** copies it to a chosen folder without overwriting existing files. **Print saved image…** sends a one-page PDF of the saved image to the desktop print portal; videos cannot be printed. These actions use the selected saved file, not unsaved editor changes. To share or print edits, export a flattened image into the capture folder and select it in the library. Open-with, email, and printing require compatible desktop portals and applications.

In the same panel, **Auto-copy new captures…** chooses a folder for automatic copies of newly saved screenshots, scrolling and panoramic images, interval and daily captures, and completed recordings. This opt-in persists until **Disable auto-copy** is selected. Existing files are never overwritten, and a copy failure is reported without removing the original. The copy folder must differ from the captures folder. Interval captures copy sequentially, so a slow destination can delay subsequent captures. Auto-copy does not include screen text or unsaved editor changes; email and printing remain manual.

**Export silent MP4…** writes a video-only MP4 without modifying the source. It re-encodes the video and removes all audio tracks.

Opening images, documents, and video frames runs in the background. If the editor changes or another file is opened before loading finishes, the older result is discarded rather than replacing newer work.

Enter a **Preferred capture shortcut** (for example `CTRL+SHIFT+X`) and choose **Enable global capture shortcut…** to request a binding from the desktop's XDG GlobalShortcuts portal. Optionally select **Include global recording controls** before enabling to request pause/resume and stop/save shortcuts, with preferred keys Shift+F9 and Shift+F10. The optional **Recording start shortcut** selects silent, microphone, system, or mixed audio and requests a start/stop binding with preferred key Shift+F8. It defaults to **No start shortcut**, including for existing users. Starting a recording still requests desktop capture permission and uses the current webcam-overlay checkbox. Disable and re-enable the shortcuts to apply changed choices. Jiffshot remembers these preferences and requests the bindings again on launch; the desktop may prompt again or choose different final bindings. A failed request is reported and retried on the next launch unless disabled. Use **Disable global shortcut** to turn off automatic rebinding. Availability and activation depend on the desktop portal.

Choose **Enable tray** to keep Jiffshot running after closing its window. This requires a desktop with a working StatusNotifier system tray; the setting is remembered, but a normal launch still shows the window. Click the tray icon or select **Open Jiffshot** to restore it; its menu also offers capture and quit. Enabled shortcuts and daily schedules continue while the window is hidden, subject to desktop portal permission prompts. Quit is blocked while a capture or recording is active. **Disable tray** restores normal close-to-quit behavior. If the tray host is unavailable or disappears, Jiffshot keeps or restores the window and reports the problem instead of hiding without a way back.

Choose **Enable login startup** to start Jiffshot when the desktop session supports XDG autostart. At login, it starts hidden only if the tray was enabled and a tray host is available; otherwise the editor stays visible. **Disable login startup** turns it off. This writes a user-owned desktop entry and does not change unrelated startup entries. If a portable installation is moved, enable login startup again so the entry points to its new location. `jiffshot --background` uses the same safe tray-or-visible behavior.

While hidden, capture and recording results, scheduled-capture problems, and shortcut failures are sent to the desktop notification service. Interval capture notifies on completion rather than every frame. If notifications are unavailable, the result remains in the editor's status bar and the failure to notify is logged.

#### Remaining Linux parity work

- [x] Build and verify a relocatable Linux x64 tar.gz, Ubuntu DEB, and Fedora RPM; clean-install-test the native packages.
- [x] Exercise capture, recording, and chooser workflows in isolated WSL/Sway portals with virtual audio and webcam sources.
- [x] Provide opt-in tray operation with window restoration if the tray host disappears.
- [x] Support opt-in desktop login startup with safe tray fallback.
- [x] Save named screen/window presets with independent delay and cursor choices.
- [ ] Automatic scrolling, specialized selection modes, unattended/background scheduling, and presets for other capture modes.
- [ ] Recording floating controls, live drawing and cursor effects, and the full Windows video editor.
- [x] Open, email, copy, and print selected saved captures through desktop integrations.
- [x] Automatically copy new captures and recordings to an opt-in folder.
- [x] Adjust numeric parameters for supported image effects.
- [x] Start, stop, and pause recordings with opt-in global shortcuts.
- [ ] Other automatic sharing destinations and additional global shortcuts.
- [ ] Unified preferences, template drag-and-drop, and richer library organization.
- [ ] Verify native desktop consent and global shortcut activation, and test physical audio and camera devices.

For Windows on ARM, cross-build from x64 (requires Visual Studio's ARM64 build tools); the output is in `target/aarch64-pc-windows-msvc/release`:

```powershell
rustup target add aarch64-pc-windows-msvc
cargo build --release -p jiffshot --target aarch64-pc-windows-msvc
```

CI (`.github/workflows/ci.yml`) builds and tests Windows x64 and cross-builds Windows ARM64. Linux uses native `ubuntu-24.04` (x64) and `ubuntu-24.04-arm` (ARM64) hosted runners: each runs workspace tests, app Clippy, package-content tests, and portable smoke tests. Fedora 43 containers on each native runner separately compile and test the Fedora build and RPM. Windows artifacts include portable ZIP, MSI, and MSIX packages for both architectures (`jiffshot-win-x64`, `jiffshot-win-arm64`, and their `-msi` / `-msix` variants). Linux artifacts contain Ubuntu 24.04 tar.gz/DEB packages (`jiffshot-linux-x64`, `jiffshot-linux-arm64`) and separately compiled Fedora 43 RPMs (`jiffshot-linux-x64-rpm`, `jiffshot-linux-arm64-rpm`), with architecture/distro-specific caches. Clean Ubuntu/Fedora containers on both native architectures verify package and installed ELF architecture, checksums, dependency resolution, desktop/MIME registration, isolated graphical startup, and removal without deleting user settings. Release assembly requires all six Linux packages and verifies their builder checksums before collecting them, per-architecture Linux checksums, and `SHA256SUMS.txt`; Windows packages are signed, while Linux packages are currently unsigned. Rustolonia [v12.1.2](https://github.com/Devolutions/rustolonia/releases/tag/v12.1.2) publishes both Linux x64 and ARM64 runtime host assets; this availability alone does not establish a successful native Jiffshot ARM64 build or smoke test. To move to a newer Rustolonia, change the tag on the two lines in `Cargo.toml`'s `[workspace.dependencies]`.

### Building and Installing Linux Packages

On native Ubuntu 24.04 x64 or ARM64, after installing the Linux build dependencies above:

```sh
cargo build --release --locked -p jiffshot
sudo apt-get install binutils dpkg-dev desktop-file-utils
# PowerShell 7.4 or newer (pwsh) is required: https://learn.microsoft.com/powershell/scripting/install/install-ubuntu
pwsh package/linux/build.ps1 -Format tar.gz,deb
```

With a custom Cargo target directory, pass `-BuildDir /path/to/target/release`. Outputs default to `package/out/linux`:

| Format | x64 filename | ARM64 filename | Use |
| --- | --- | --- | --- |
| Portable | `Devolutions.Jiffshot-linux-x64-0.1.0.tar.gz` | `Devolutions.Jiffshot-linux-arm64-0.1.0.tar.gz` | Extract, then run `jiffshot-0.1.0-linux-<arch>/bin/jiffshot` |
| Ubuntu | `jiffshot_0.1.0_amd64.deb` | `jiffshot_0.1.0_arm64.deb` | `sudo apt install ./<matching-architecture>.deb` |
| Fedora | `jiffshot-0.1.0-1.x86_64.rpm` | `jiffshot-0.1.0-1.aarch64.rpm` | `sudo dnf install ./<matching-architecture>.rpm` |

The version is read from the workspace `Cargo.toml`; `-Version` or `JIFFSHOT_VERSION` can assert the expected release version. Architecture is detected from the executable's ELF64 header and must match **every** bundled Rustolonia, SkiaSharp, and HarfBuzzSharp library. Invalid, unsupported, or mixed-architecture inputs are rejected before packaging. Optional `-Arch x64` or `-Arch arm64` asserts this identity; it never relabels or cross-compiles a binary. `SHA256SUMS-linux-x64.txt` or `SHA256SUMS-linux-arm64.txt` covers the formats generated by each invocation; use separate output directories for the Ubuntu and Fedora invocations, which have separate checksum manifests.

**Build the RPM natively on Fedora 43 on each architecture**, with Fedora's development dependencies listed in [the Linux packaging instructions](package/linux/README.txt):

```sh
cargo build --release --locked -p jiffshot
pwsh package/linux/build.ps1 -Format rpm -OutputDir package/out/linux-rpm
```

Do not convert the Ubuntu binary into a Fedora RPM: Ubuntu's `liblept.so.5` and Fedora's `libleptonica.so.6` are not interchangeable. Packages inherit their build distro's ABI requirements; these builds do not claim compatibility with older distributions, Debian 13, or other architectures.

Each format contains a relocatable launcher, Jiffshot and its three private native runtime libraries, the SVG icon, desktop entry, `.jsd` MIME registration, and documentation. The launcher preserves file arguments and the caller's working directory, including paths containing spaces. Native packages install under `/usr` and resolve system libraries through the package manager; the portable archive still needs those libraries installed. No .NET SDK is required. Fedora may need additional RPM Fusion media codecs for recording/export; missing GStreamer elements are reported by the app. Removing a native package leaves user captures and settings untouched.

The repository currently has no declared application license. The RPM records `LicenseRef-Unknown`; this metadata does not grant a new license or replace upstream runtime licensing terms.

### Logo Assets

`assets/jiffshot.svg` is the source for the program logo. Regenerate the Windows icon (all nine sizes) and rebuild the executable to embed it:

```powershell
cargo run --release -p jiffshot -- --write-icon assets\jiffshot.ico
cargo build --release -p jiffshot
```

To export a square PNG from the SVG, choose a size from 1 through 4096 pixels:

```powershell
cargo run --release -p jiffshot -- --write-logo 512 jiffshot-512.png
```

### Building the MSI Installer

`package/` builds a per-machine MSI with [WixSharp](https://github.com/oleg-shilo/wixsharp) on its WiX 4+ stream. It installs `jiffshot.exe` and the host DLLs into `C:\Program Files\Devolutions\Jiffshot`, adds a Start menu shortcut and associates `.jsd` documents; installing a newer MSI upgrades the old one. It needs the .NET SDK (the packager is a small .NET Framework program) and WiX CLI 5.0.2 (`winget install --id WiXToolset.WiXCLI --version 5.0.2.0 --exact`) with `wix.exe` on `PATH`:

```powershell
cargo build --release -p jiffshot
dotnet build package -c Release
package/bin/Release/net472/Jiffshot.Package.exe -bin target/release -platform x64
```

The MSI lands in `package/out` as `Devolutions.Jiffshot-x64-0.1.0.msi` (with the selected architecture and version; `-out` picks another folder). Programs and Features lists it as **Devolutions Jiffshot** under publisher **Devolutions Inc.**. For ARM64, pass the ARM64 build's folder and `-platform arm64`. The version comes from `Cargo.toml` unless `-version` gives one. CI builds both MSIs from its builds (`jiffshot-win-x64-msi`, `jiffshot-win-arm64-msi`), and installs and uninstalls the x64 one.

### Releases & Code Signing

`ci.yml` builds and tests unsigned portable packages, MSIs, and MSIX packages for pushes to master, pull requests, and manual runs. `release.yml` reuses those jobs with signing enabled: it signs the portable executable and Rustolonia host DLL before building the installers, then signs the MSI and MSIX. The x64 MSI is installed and uninstalled before the release can proceed; signed MSIX release builds are also installed and uninstalled.

Run **Release** from GitHub Actions with a version matching `Cargo.toml` (for example, `0.1.0`, without `v`). Update and commit the workspace version before releasing a new version. Dry runs are enabled by default and upload signed packages without creating a tag or release. The `github_env` input selects the signing environment independently of `dry_run`: on master, `auto` (the default) and `prod` use `publish-prod`, while `test` uses `publish-test`. Other branches always use `publish-test` and only permit dry runs, even if `prod` is requested. For test signing, the workflow retrieves the `publish-test` profile root with psign and trusts it only in each ephemeral runner's current user certificate store, so Authenticode validation remains enabled. A non-dry-run dispatch with `github_env=test` publishes test-signed artifacts that are not publicly trusted. The workflow rejects non-dry-run releases from other branches, version mismatches, and existing tags.

Both modes retain the portable, MSI, MSIX, and Linux Actions artifacts plus a `release-assets` artifact containing x64/ARM64 MSIs, MSIX packages, ZIP archives, all six Linux tar.gz/DEB/RPM packages, `SHA256SUMS-linux-x64.txt`, `SHA256SUMS-linux-arm64.txt`, and the overall `SHA256SUMS.txt`. Missing Linux formats, architectures, builder checksum manifests, or mismatched hashes block assembly. After all builds, signing, and validation succeed, a non-dry-run creates `v<version>` at the dispatched commit and publishes a GitHub Release with those same files attached. Do not create the tag manually.

```powershell
# Validate with test signing without publishing (defaults to dry_run=true).
gh workflow run release.yml --ref master -f version=0.1.0 -f github_env=test
# Create the tag and release with production signing after validation.
gh workflow run release.yml --ref master -f version=0.1.0 -f dry_run=false
```

Signing uses psign and Azure Trusted Signing with the same secrets as `Devolutions/windbg-tool`: `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_SUBSCRIPTION_ID`, `TRUSTED_SIGNING_ENDPOINT`, `TRUSTED_SIGNING_ACCOUNT_NAME`, and `TRUSTED_SIGNING_PROFILE_NAME`. The Azure identity must allow GitHub Actions OIDC authentication for this repository's selected environment and have signing access to the profile. `TRUSTED_SIGNING_TIMESTAMP_SERVER` is an optional repository variable, defaulting to `http://timestamp.acs.microsoft.com/`. Missing signing configuration or failed signature/timestamp verification fails the build rather than uploading unsigned release artifacts.

### Testing

From the repository root:

```powershell
cargo test --release --workspace                          # capture side, documents, media, library, editor
./target/release/jiffshot.exe --selftest DIR                  # overlay logic, offscreen
./target/release/jiffshot.exe --record-selftest DIR 4         # records its own test window
```

The editor drives its own UI when `JIFFSHOT_UITEST=DIR` is set (with `JIFFSHOT_UITEST_IMAGE`, `JIFFSHOT_UITEST_VIDEO`, an isolated `JIFFSHOT_LIBRARY` and, to leave your settings alone, scratch `APPDATA` and `LOCALAPPDATA`), writing screenshots of every window and `ui-log.txt` to `DIR`. `cargo run -p jiffshot-doc --example synthetic -- OUT.png` writes the test image its coordinates assume.
The capture panels and outlines are left out of screen captures; set `JIFFSHOT_HUD_CAPTURABLE=1` to see them in screenshots while checking them. `JIFFSHOT_CAPTURE_DRY_RUN=1` runs the selection overlay for real but only logs the selection (nothing is saved or copied), for checking it live; `JIFFSHOT_RECORD_DRAW=1` and `JIFFSHOT_RECORD_SWITCH=1` make `--record-selftest` check Screen Draw marks and switching sound and the pointer part-way.
