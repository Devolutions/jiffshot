<p align="center">
  <img src="assets/jiffshot.svg" width="96" height="96" alt="Jiffshot logo" />
</p>

<h1 align="center">Jiffshot</h1>

<p align="center">
  <strong>Fast, all-in-one screen capture, video recorder, and image &amp; video editor for Windows.</strong>
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

**Jiffshot** is a native screen capture tool, screen recorder, and non-destructive image and video editor for Windows 10 and Windows 11. It runs as a single lightweight executable (`jiffshot.exe`) that stays ready in the system tray for instant screenshots, scrolling captures, OCR text extraction, MP4/GIF screen recordings, and rich visual annotations.

<p align="center">
  <img src="docs/images/editor.png" alt="Jiffshot Editor annotating a screenshot" width="900" />
</p>

## Download & Installation

Prebuilt, Authenticode-signed packages for **Windows 10 and Windows 11** (`x64` and `ARM64`) are available on the [**GitHub Releases**](https://github.com/Devolutions/jiffshot/releases) page:

👉 **[Download Jiffshot from GitHub Releases](https://github.com/Devolutions/jiffshot/releases/latest)**

| Package | Architecture | Description |
|---|---|---|
| **`Devolutions.Jiffshot-x64-<version>.msi`** | Windows x64 (Intel / AMD) | Standard Windows Installer. Installs to `C:\Program Files\Devolutions\jiffshot`, adds a Start Menu shortcut, and associates `.jsd` Jiffshot Document files. |
| **`Devolutions.Jiffshot-arm64-<version>.msi`** | Windows ARM64 | Standard Windows Installer for Windows on ARM devices. |
| **`Devolutions.Jiffshot-x64-<version>-portable.zip`** | Windows x64 (Intel / AMD) | Portable archive—unzip anywhere and run `jiffshot.exe`. Nothing is installed in the system. |
| **`Devolutions.Jiffshot-arm64-<version>-portable.zip`** | Windows ARM64 | Portable archive for Windows on ARM devices. |

> **Note for Windows "N" editions:** Video recording uses Windows Media Foundation, which is omitted from Windows "N" SKUs by default. Install Microsoft's *Media Feature Pack* in Windows Optional Features to enable MP4 recording and playback.

## Quick Start

1. **Launch Jiffshot:** Start **Devolutions Jiffshot** from the Start Menu (or run `jiffshot.exe` from the portable folder). Jiffshot places its icon in the Windows notification area (system tray).
2. **Take a capture:** Press **Print Screen** (`PrtScn`) at any time, click the tray icon, or open the **Capture Window**.
   - **Capture a window or UI element:** Move the pointer over any window and click. Roll the **mouse wheel** up or down while hovering to step between the top-level window and individual child UI controls.
   - **Capture a region:** Click and drag a rectangle. Hold `Shift` for a square, `Ctrl` for `16:9`, or `Ctrl+Shift` for `4:3`.
   - **Capture a full monitor or desktop:** Press `F` for the current screen or `A` for all screens.
3. **Annotate & share:** By default, captures are saved to `Pictures\Jiffshot`, copied to the clipboard, and opened in the **Jiffshot Editor** where you can add arrows, callouts, numbered steps, blur, or effects, then copy (`Ctrl+Shift+C`), export (`Ctrl+E`), or share directly to other apps.
4. **Stay ready in the tray:** Closing the editor window keeps Jiffshot running quietly in the notification area so your hotkeys stay active. To quit completely, right-click the tray icon and choose **Exit**.

---

## Features

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
| `crates/jiffshot-workflow/` | The capture side: tray, hotkeys, selection overlay, recording, capture panels, post-capture workflow, command-line modes |
| `app/` | `jiffshot.exe`: the editor's model (`src/app.rs`, view models in `src/vm.rs`), every window (`src/ui/`) and the capture side, in one process |
| `package/` | The MSI installer, built with WixSharp |

### Compiling

Jiffshot requires **Rust 1.88+** and nothing else: it depends on Rustolonia's `v12.1.2` release, and the first build downloads that release's prebuilt host (verified against the checksums pinned at the tag). No .NET SDK or Rustolonia checkout is required to build the application.

```powershell
cargo run --release -p jiffshot
```

The build copies the host next to `jiffshot.exe`, so `target/release` runs as it is; zip `jiffshot.exe` and the host DLLs beside it, with `app/README-portable.txt`, for the portable app.

For Windows on ARM, cross-build from x64 (requires Visual Studio's ARM64 build tools); the output is in `target/aarch64-pc-windows-msvc/release`:

```powershell
rustup target add aarch64-pc-windows-msvc
cargo build --release -p jiffshot --target aarch64-pc-windows-msvc
```

CI (`.github/workflows/ci.yml`) builds both and uploads them as portable zips (`jiffshot-win-x64`, `jiffshot-win-arm64`). To move to a newer Rustolonia, change the tag on the two lines in `Cargo.toml`'s `[workspace.dependencies]`.

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

`package/` builds a per-machine MSI with [WixSharp](https://github.com/oleg-shilo/wixsharp) on WiX 3.14. It installs `jiffshot.exe` and the host DLLs into `C:\Program Files\Devolutions\jiffshot`, adds a Start menu shortcut and associates `.jsd` documents; installing a newer MSI upgrades the old one. It needs the .NET SDK (the packager is a small .NET Framework program) and WiX 3.14 (`choco install wixtoolset --version 3.14.0`, or set `WIXSHARP_WIXDIR` to its `bin` folder):

```powershell
cargo build --release -p jiffshot
dotnet build package -c Release
package/bin/Release/net472/Jiffshot.Package.exe -bin target/release -platform x64
```

The MSI lands in `package/out` as `Devolutions.Jiffshot-x64-0.1.0.msi` (with the selected architecture and version; `-out` picks another folder). Programs and Features lists it as **Devolutions Jiffshot** under publisher **Devolutions Inc.**. For ARM64, pass the ARM64 build's folder and `-platform arm64`. The version comes from `Cargo.toml` unless `-version` gives one. CI builds both MSIs from its builds (`jiffshot-win-x64-msi`, `jiffshot-win-arm64-msi`), and installs and uninstalls the x64 one.

### Releases & Code Signing

`ci.yml` builds and tests unsigned portable packages and MSIs for pushes to master, pull requests, and manual runs. `release.yml` reuses those jobs with signing enabled: it signs the portable executable and Rustolonia host DLL before building the MSI, then signs the MSI. The x64 MSI is installed and uninstalled before the release can proceed.

Run **Release** from GitHub Actions with a version matching `Cargo.toml` (for example, `0.1.0`, without `v`). Update and commit the workspace version before releasing a new version. Dry runs are enabled by default and upload signed packages without creating a tag or release. The `github_env` input selects the signing environment independently of `dry_run`: on master, `auto` (the default) and `prod` use `publish-prod`, while `test` uses `publish-test`. Other branches always use `publish-test` and only permit dry runs, even if `prod` is requested. For test signing, the workflow retrieves the `publish-test` profile root with psign and trusts it only in each ephemeral runner's current user certificate store, so Authenticode validation remains enabled. A non-dry-run dispatch with `github_env=test` publishes test-signed artifacts that are not publicly trusted. The workflow rejects non-dry-run releases from other branches, version mismatches, and existing tags.

Both modes retain the portable and MSI Actions artifacts plus a `release-assets` artifact containing x64/ARM64 MSIs, portable ZIPs, and `SHA256SUMS.txt`. After all builds, signing, and validation succeed, a non-dry-run creates `v<version>` at the dispatched commit and publishes a GitHub Release with those same files attached. Do not create the tag manually.

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
