# Jiffshot

A screen capture tool and image editor for Windows, written entirely in Rust
with its UI on Rustolonia's code-first API (no AXAML, no C#). It runs as one
program, `jiffshot.exe`: the tray icon and capture side on their own thread,
the editor and every other window on Avalonia.

## Features

**Capture**
- Region, window (the mouse wheel walks up and down child elements), screen
  and all screens, from a frozen overlay with a magnifier, size labels and
  aspect locks (Shift square, Ctrl 16:9, Ctrl+Shift 4:3).
- Scrolling (vertical, horizontal or both), panoramic, freehand, multiple
  areas, fixed size, open menus, Grab Text (Windows OCR) and Step Capture (a
  guide of numbered screenshots, one per click).
- Time delay, time-lapse and a daily schedule; presets with their own
  hotkeys; the Capture window (All-in-One, Image and Video tabs) and a small
  Capture Widget at the top of the screen.
- After a capture: capture effects, the clipboard, the editor, and share
  destinations (file, email, Office apps, any program, FTP, print).

**Recording**
- An area, a window or a screen, with system sound, the microphone or both,
  the webcam in a corner, pointer highlight and click rings, and Screen Draw.
- A toolbar under the area: stop, pause, restart, minimize, cancel, and the
  pointer, webcam and sound switches, which change the recording as it runs.
- In the editor: trim, cut, frame stepping, GIF export and videos made from
  images.

**Editor**
- Arrow, line, shape, text, callout, step, pen, highlight, blur, magnify,
  spotlight, stamps, fill, crop, cut out, selection, magic wand, eraser,
  eyedropper, Smart Move and Text Replace, with quick styles.
- Effects (borders, shadows, edges, perspective, page curl and more),
  templates with drop zones, and combined images.
- A library of captures with search, filters, tags and pins; export to PNG,
  JPEG, PDF, BMP, GIF and WebP; editable `.jsd` documents.

One Preferences dialog (Capture, Editor, Library, Share, Hotkeys) and one
color theme cover every window; settings are `%APPDATA%\Jiffshot\jiffshot.json`
and captures go to `Pictures\Jiffshot` unless changed.
[docs/architecture.md](docs/architecture.md) describes how it fits together.

## Layout

| Path | Contents |
|---|---|
| `crates/jiffshot-capture/` | Monitors and DPI, screen grabs, window targets, clipboard, OCR, media (recording and playback), the library, settings |
| `crates/jiffshot-doc/` | Documents: annotation objects, rendering, editing and undo, effects, smart tools, `.jsd` and image files |
| `crates/jiffshot-workflow/` | The capture side: tray, hotkeys, selection overlay, recording, capture panels, post-capture workflow, command-line modes |
| `app/` | `jiffshot.exe`: the editor's model (`src/app.rs`, view models in `src/vm.rs`), every window (`src/ui/`) and the capture side, in one process |
| `package/` | The MSI installer, built with WixSharp |

## Building

Jiffshot needs Rust 1.88+ and nothing else: it depends on Rustolonia's
`v12.1.2` release, and the first build downloads that release's prebuilt
host (checked against the checksums pinned at the tag). No .NET SDK or
Rustolonia checkout is involved.

```powershell
cargo run --release -p jiffshot
```

The build copies the host next to `jiffshot.exe`, so `target/release` runs as
it is; zip `jiffshot.exe` and the host DLLs beside it, with
`app/README-portable.txt`, for the portable app.

For Windows on ARM, cross-build from x64 (it needs Visual Studio's ARM64
build tools); the output is in `target/aarch64-pc-windows-msvc/release`:

```powershell
rustup target add aarch64-pc-windows-msvc
cargo build --release -p jiffshot --target aarch64-pc-windows-msvc
```

CI (`.github/workflows/ci.yml`) builds both and uploads them as portable
zips (`jiffshot-win-x64`, `jiffshot-win-arm64`). To move to a newer
Rustolonia, change the tag on the two lines in `Cargo.toml`'s
`[workspace.dependencies]`.

With no arguments Jiffshot starts in the tray. `jiffshot.exe FILE` opens a
file in the editor, `--capture`, `--record`, `--window` and the other capture
flags start a capture, and `--quit` ends it; a second launch hands its request
to the one already running. Closing the editor keeps Jiffshot in the tray.

### Logo assets

`assets/jiffshot.svg` is the source for the program logo. Regenerate the
Windows icon (all nine sizes) and rebuild the executable to embed it:

```powershell
cargo run --release -p jiffshot -- --write-icon assets\jiffshot.ico
cargo build --release -p jiffshot
```

To export a square PNG from the SVG, choose a size from 1 through 4096 pixels:

```powershell
cargo run --release -p jiffshot -- --write-logo 512 jiffshot-512.png
```

## Installer

`package/` builds a per-machine MSI with [WixSharp](https://github.com/oleg-shilo/wixsharp)
on WiX 3.14. It installs `jiffshot.exe` and the host DLLs into
`C:\Program Files\Devolutions\jiffshot`, adds a Start menu shortcut and
associates `.jsd` documents; installing a newer MSI upgrades the old one. It
needs the .NET SDK (the packager is a small .NET Framework program) and WiX
3.14 (`choco install wixtoolset --version 3.14.0`, or set `WIXSHARP_WIXDIR`
to its `bin` folder):

```powershell
cargo build --release -p jiffshot
dotnet build package -c Release
package/bin/Release/net472/Jiffshot.Package.exe -bin target/release -platform x64
```

The MSI lands in `package/out` as `Devolutions.Jiffshot-x64-0.1.0.msi`
(with the selected architecture and version; `-out` picks another folder).
Programs and Features lists it as **Devolutions Jiffshot**. For ARM64, pass
the ARM64 build's folder and `-platform arm64`. The version comes from
`Cargo.toml` unless `-version` gives one. CI builds both MSIs from its builds
(`jiffshot-win-x64-msi`, `jiffshot-win-arm64-msi`), and installs and
uninstalls the x64 one.

## Releases

`ci.yml` builds and tests unsigned portable packages and MSIs for pushes to
master, pull requests, and manual runs. `release.yml` reuses those jobs with
signing enabled: it signs the portable executable and Rustolonia host DLL before
building the MSI, then signs the MSI. The x64 MSI is installed and uninstalled
before the release can proceed.

Run **Release** from GitHub Actions with a version matching `Cargo.toml` (for
example, `0.1.0`, without `v`). Update and commit the workspace version before
releasing a new version. Dry runs are enabled by default and can run on any
branch using `publish-test`; they upload signed packages without creating a tag
or release. To publish, select master and disable `dry_run`, which uses
`publish-prod`. The workflow rejects production releases from other branches,
version mismatches, and existing tags.

Both modes retain the portable and MSI Actions artifacts plus a `release-assets`
artifact containing x64/ARM64 MSIs, portable ZIPs, and `SHA256SUMS.txt`. After all
builds, signing, and validation succeed, a production run creates `v<version>`
at the dispatched commit and publishes a GitHub Release with those same files
attached. Do not create the tag manually.

```powershell
# Validate signed packages without publishing (defaults to dry_run=true).
gh workflow run release.yml --ref master -f version=0.1.0
# Create the tag and release after validation.
gh workflow run release.yml --ref master -f version=0.1.0 -f dry_run=false
```

Signing uses psign and Azure Trusted Signing with the same secrets as
`Devolutions/windbg-tool`: `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`,
`AZURE_SUBSCRIPTION_ID`, `TRUSTED_SIGNING_ENDPOINT`,
`TRUSTED_SIGNING_ACCOUNT_NAME`, and `TRUSTED_SIGNING_PROFILE_NAME`.
The Azure identity must allow GitHub Actions OIDC authentication for this
repository's selected environment and have signing access to the profile.
`TRUSTED_SIGNING_TIMESTAMP_SERVER` is an optional repository variable, defaulting
to `http://timestamp.acs.microsoft.com/`. Missing signing configuration or
failed signature/timestamp verification fails the build rather than uploading
unsigned release artifacts.

## Testing

From the repository root:

```powershell
cargo test --release --workspace                          # capture side, documents, media, library, editor
./target/release/jiffshot.exe --selftest DIR                  # overlay logic, offscreen
./target/release/jiffshot.exe --record-selftest DIR 4         # records its own test window
```

The editor drives its own UI when `JIFFSHOT_UITEST=DIR` is set (with
`JIFFSHOT_UITEST_IMAGE`, `JIFFSHOT_UITEST_VIDEO`, an isolated
`JIFFSHOT_LIBRARY` and, to leave your settings alone, scratch `APPDATA` and
`LOCALAPPDATA`), writing screenshots of every window and `ui-log.txt` to
`DIR`. `cargo run -p jiffshot-doc --example synthetic -- OUT.png` writes the
test image its coordinates assume.
The capture panels and outlines are left out of screen captures; set
`JIFFSHOT_HUD_CAPTURABLE=1` to see them in screenshots while checking them.
`JIFFSHOT_CAPTURE_DRY_RUN=1` runs the selection overlay for real but only logs
the selection (nothing is saved or copied), for checking it live;
`JIFFSHOT_RECORD_DRAW=1` and `JIFFSHOT_RECORD_SWITCH=1` make
`--record-selftest` check Screen Draw marks and switching sound and the
pointer part-way.
