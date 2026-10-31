Jiffshot for Linux x64
=====================

The packages contain Jiffshot and its private Rustolonia, SkiaSharp, and
HarfBuzzSharp native runtime. They do not bundle system libraries or codecs.
No .NET SDK is required to run the application.

The release tar.gz and Debian package are built on Ubuntu 24.04 and target
Ubuntu 24.04. The RPM is built separately on Fedora 43 and targets Fedora 43.
Packages built elsewhere inherit that build distro's ELF ABI requirements.
In particular, do not repackage an Ubuntu binary as a Fedora RPM: their
Leptonica libraries have different SONAMEs. Build natively for each target.
Other architectures and older distributions are not currently supported.
The GUI requires an X11 display or XWayland. The current Rustolonia host
does not support a Wayland-only session without XWayland.

Portable tar.gz:
  Extract the archive and run ./jiffshot-VERSION-linux-x64/bin/jiffshot.
  The directory can be relocated, including to paths containing spaces.
  Add its bin directory to PATH to use "jiffshot" from a terminal.
  Desktop and MIME files are included but are not installed automatically.

Debian / Ubuntu:
  sudo apt install ./jiffshot_VERSION_amd64.deb
  Start Jiffshot from the application menu or run "jiffshot [image-video-or-jsd]".
  sudo apt remove jiffshot

Fedora:
  sudo dnf install ./jiffshot-VERSION-1.x86_64.rpm
  Start Jiffshot from the application menu or run "jiffshot [image-video-or-jsd]".
  sudo dnf remove jiffshot
  H.264 recording and some import formats need extra codec plugins.
  Enable the appropriate RPM Fusion repositories and install
  gstreamer1-plugins-ugly and gstreamer1-libav where available.
  The required GStreamer elements are x264enc, h264parse, mp4mux, voaacenc,
  aacparse, audiomixer, pipewiresrc, and pulsesrc. Missing elements are reported
  in the UI.

Run "jiffshot --capture", "--capture-interactive", "--grab-text", "--scrolling",
"--scrolling-horizontal", "--panoramic", "--interval", or "--record" to start an action
directly. Scrolling and panning are manual, --interval uses saved preferences, and --record
starts without audio. Capture, screen text, scrolling, panoramic, interval capture and recording
are also offered as application actions by supporting desktops. Use
"jiffshot --help" for the Linux CLI.

Portable runtime dependencies on Ubuntu 24.04:
  sudo apt install libgstreamer1.0-0 libgstreamer-plugins-base1.0-0 \
    libtesseract5 liblept5 libfontconfig1 libice6 libsm6 libx11-6 \
    libxcursor1 libxi6 libxrandr2 libgtk-3-0t64 libicu74 libssl3t64 fonts-dejavu-core \
    tesseract-ocr-eng gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly gstreamer1.0-libav gstreamer1.0-pipewire \
    gstreamer1.0-pulseaudio
  (The native .deb resolves its exact ELF dependencies automatically.)

File and folder choosers use the XDG FileChooser portal when available.
If that portal is unavailable, the app falls back to the native GTK picker;
other portal errors are reported rather than silently treated as cancellation.

Screen capture and recording require a working XDG ScreenCast portal,
PipeWire, and a graphical desktop. System-sound and microphone recording need
a PulseAudio-compatible sound server; system sound uses the default output's
monitor. An optional rectangular or circular webcam overlay uses a raw-video
V4L2 camera accessible to the user. The webcam preferences in the scrollable
controls select the device path, corner, shape and size (10-50 percent); choose
"Save webcam preferences" before recording. Circle size is a percentage of the
recording height, capped by its width. The default is a rectangle from
/dev/video0 at 25 percent in the bottom-right corner. WSLg without these
portals can run the editor, but cannot validate real screen capture/recording.
Use "Pause/resume recording" to omit a section from a recording, then use a
recording action again to stop and save, including while paused. The capture
session and selected audio and webcam devices stay active while paused.
"Restart recording" discards the current video and starts again with the same
source and devices; "Cancel recording" discards it. Stop or cancel during the
portal chooser closes the request without saving a video.

Set "Recording frame rate" to Source (the default), 15, 24, 30, or 60 fps
and choose "Save recording frame rate" to apply it to new recordings, including
command-line and global-shortcut starts. Restart retains the rate chosen when
the recording began. Fixed rates may drop or duplicate frames and cannot
increase the source's actual temporal detail.

The "Desktop screenshot chooser" uses the desktop's XDG Screenshot portal.
Its available selection modes may include a rectangular area, window or
screen, depending on the desktop. Cancelling saves nothing. If the editor
changes while a capture is pending, the captured image is saved and copied
when enabled but does not replace newer editor changes.

"Grab text..." or --grab-text uses the desktop Screenshot chooser and English
Tesseract data to recognize the selected image and copy text to the clipboard.
It does not replace the editor or save a Jiffshot capture. Cancellation,
unrecognized text or errors leave the clipboard unchanged; review OCR results.

For timed screen or window capture, set "Screen capture delay (seconds)" to
1-60 and choose "Save capture delay". The countdown starts after source
selection, allowing time to open the content to capture. "Cancel delayed
capture" stops it without saving; close the desktop chooser too if it is still
open. Zero captures immediately. This setting also applies to --capture and
the global capture shortcut, but not to the desktop screenshot chooser,
scrolling capture, interval capture, daily capture or recording.

Enable "Screen presets" to save named screen/window captures with their own
delay and cursor choices. Enter a name and options, then choose "Save screen
preset"; saving the same name replaces it. Choose the preset's "Capture" button
to run it, or enter its name and choose "Delete screen preset". A preset still
requests a desktop source and does not change the normal capture preferences.
Names are case-sensitive and limited to 128 UTF-8 bytes. Presets for other
capture modes are not currently available.

"Include cursor in captures" and "Include cursor in recordings" control the
pointer independently for screen, interval and daily captures and recordings. Choose
"Save cursor preferences" to apply them; both default to included, subject to
desktop portal and compositor support. Scrolling and panoramic capture request a hidden cursor,
while the desktop screenshot chooser controls its own cursor behavior.

Manual scrolling capture uses the portal-selected source. Check "Horizontal
scrolling" before starting to scroll right; leave it unchecked to scroll down.
Scroll slowly while retaining overlap between views, and use the same control
to finish or "Cancel scrolling" to discard it. It reports lost overlap and
finishes the last complete image when its size limit is reached. Automatic
scrolling is not implemented.

"Panoramic capture... (start/finish)" or --panoramic selects a screen or window
and lets you pan slowly in any direction while keeping the views overlapping.
Choose the same control to finish, or "Cancel scrolling" to discard it.
Stationary toolbars and animations can interfere with matching. Unseen output
areas remain transparent; lost overlap and size limits are reported.
Panoramic and scrolling capture cannot run with interval capture or recording.

Enable "Interval options" to set an interval of at least one whole second and
optionally skip identical frames. "Interval capture... (start/stop)" selects a
screen or window once, saves an image immediately, and repeats until stopped.
Close the desktop chooser too if stopping while it is open. The choices are
saved on start and apply to --interval. Images go to the capture folder without
opening each in the editor or copying it to the clipboard. Stop interval capture
before starting a recording or scrolling capture, and vice versa.

Enable "Daily schedule options", enter a local time in HH:MM, select "Enable
daily capture" and choose "Save daily schedule". Jiffshot must remain open and
requests a screen or window from the desktop portal, which may prompt for
selection and authorization each time. The image is saved without changing
the editor or clipboard. There is at most one
attempt per local date, including across restarts and time changes; a cancelled
or failed request, or an active capture or recording, is not retried that day.
A scheduled minute skipped by a clock or daylight-saving change is missed.
Disabling the schedule prevents future attempts but does not close an existing
desktop chooser; close it to cancel a pending request.

Screenshots, scrolling captures and panoramic captures are copied to the image clipboard by default.
Toggle "Copy captures to clipboard" and choose "Save capture preferences" to
change this. A clipboard failure is reported without discarding the capture.

The capture library can search filenames and existing Jiffshot metadata in the
capture folder and YYYY-MM subfolders, with image, video, pinned, and tagged
filters. Enable "Organize captures" and select an item to edit its tags or pin.
Other sidecar fields are preserved; malformed, concurrently changed, and
symbolic-link metadata files are rejected when detected.
Use app:browser, site:example.com, and tag:planning for case-insensitive
substring matches in those metadata fields. Use age:day or age:week for the
last 24 hours or seven days of file modification time. Combine qualifiers with
ordinary search terms; unknown prefixes are searched literally.

The same panel can open the selected saved capture with another application,
attach it to an email, or copy it to a chosen folder without overwriting an
existing file. It can also send a one-page PDF of the selected saved image to
the print portal; videos cannot be printed. These actions use the saved file,
not unsaved editor changes. To share or print edits, export a flattened image
into the capture folder and select it in the library. Opening, email, and
printing require compatible desktop portals and applications.

In the same panel, "Auto-copy new captures..." selects a folder for automatic
copies of newly saved screenshots, scrolling and panoramic images, interval and
daily captures, and completed recordings. This opt-in persists until "Disable
auto-copy" is selected. Existing files are never overwritten, and failures are
reported without removing the original. The copy folder must differ from the
captures folder. Interval captures copy sequentially, so a slow destination
can delay later captures. Screen text and unsaved editor edits are not copied;
email and printing remain manual.

Enable "Editor properties and effects" to edit annotation text, colors, stroke,
font and opacity, and to apply undoable whole-image effects. For effects with a
numeric setting, enter an Effect parameter within the displayed range, or leave
it blank for the default. Changing effects clears the value. Borders and color
replacement use the current stroke and fill colors. This panel also offers Auto
trim, Copy recognized text, Smart
redact and Simplify UI. Simplify flattens annotations and replaces recognized
text with editable bars; undo restores the original. Smart redact covers entire
recognized lines with sensitive patterns. OCR can miss text, so review before
sharing. Redactions in .jsd are removable and retain the original pixels; export
a flattened image to omit those pixels. Watermark selects an image and places it
at 35% opacity by default in the bottom-right corner. Capture info adds available sidecar
time, window and process metadata, or the current local time when none is
available. Spotlight & magnify uses the selected annotation's area, or the image
center when nothing is selected. It replaces the selected annotation; Undo
restores it. Scroll the controls above the canvas to reach more options in a
small window.

"Export..." saves the flattened image as PNG, JPEG, BMP, GIF, WebP or a
single-page PDF according to its filename extension. Saving as .jsd separately
retains editable annotations.

The "Editor properties and effects" panel also provides controls to select all,
clear the selection, duplicate, delete, group, ungroup, reorder, or flatten
annotations. Ctrl+A selects all, Ctrl+D duplicates, Delete removes the selection,
and Ctrl+T flattens all annotations. Ctrl+Shift+I/G/U/F/B/T inverts the selection,
groups, ungroups, brings forward, sends backward, or flattens selected annotations.
Text fields keep their normal editing shortcuts.

Enable "Templates" to create an editable layout with a theme, optional captions,
numbers and footer. Select an image zone and use "Paste into zone" to fill it
from the clipboard. The zone controls fit, crop, clear and swap images;
Shift-click selects two zones for swapping. Drag-and-drop is not supported.

Enter a preferred global capture shortcut such as CTRL+SHIFT+X and enable it
through the XDG GlobalShortcuts portal. Optionally select "Include global
recording controls" first to request pause/resume and stop/save shortcuts,
preferring Shift+F9 and Shift+F10. The optional "Recording start shortcut" can
start/stop a silent, microphone, system-sound, or mixed-audio recording,
preferring Shift+F8. It defaults to "No start shortcut"; starting still asks the
desktop for capture permission and uses the current webcam-overlay checkbox.
Preferences are saved when shortcuts are enabled. Disable and re-enable them
to apply changed choices. The app requests bindings again at launch; the
desktop may prompt again or choose different bindings. Failed requests are
reported and retried on the next launch unless disabled. Disable shortcuts to
stop automatic rebinding. Portal support varies by desktop.

Choose "Enable tray" to keep Jiffshot running after closing its window when a
StatusNotifier system tray host is available. The preference persists, but
normal launches show the window. Activate the tray icon or use "Open
Jiffshot" to restore the window; its menu also offers capture and quit.
Enabled shortcuts and daily schedules continue while hidden, but the desktop
may still request capture permission. Finish or cancel an active capture
before quitting. Choose "Disable tray" to close normally. If the tray host is
unavailable or disappears, Jiffshot keeps or restores the window and reports
the problem; the saved preference can be retried or disabled.

Choose "Enable login startup" to start Jiffshot in desktops that support XDG
autostart. Login startup hides the editor only when the tray is enabled and a
tray host is available; otherwise it stays visible. "Disable login startup"
turns it off. The user-owned startup entry leaves unrelated entries unchanged.
After moving a portable installation, enable login startup again so it points
to the new launcher. "jiffshot --background" uses the same tray-or-visible
behavior.

While hidden, capture and recording results, scheduled-capture problems, and
shortcut failures are sent to the desktop notification service. Interval
capture notifies on completion, not every frame. If notifications are
unavailable, the editor status remains available and notification errors are
logged.

Trim, cut, and combined-video exports preserve the selected audio track as AAC.
Combined clips are ordered by case-sensitive filename (path breaks ties),
normalized to the first clip's dimensions at 30 fps, and silent clips add
silence if another clip has audio. "Export silent MP4" re-encodes the video
without audio and leaves the source unchanged. "Export video as GIF" follows
source frame timing, rounded to 10 ms; very short frames may be omitted. Its
destination must differ from the source. Full preferences/hotkey
parity is not implemented. See the included
README.md for current capabilities.
Settings and captures live in the user's XDG config/Pictures directories;
installing or removing a package does not modify or delete them.

Building packages from the repository:
  cargo build --release --locked -p jiffshot
  sudo apt install python3 binutils dpkg-dev desktop-file-utils
  python3 package/linux/build.py --format tar.gz deb

Build the Fedora binary and RPM on Fedora (or in a Fedora container):
  sudo dnf install gcc-c++ clang-devel leptonica-devel tesseract-devel \
    pkgconf-pkg-config gstreamer1-devel gstreamer1-plugins-base-devel \
    python3 binutils rpm-build desktop-file-utils
  cargo build --release --locked -p jiffshot
  python3 package/linux/build.py --format rpm --output-dir package/out/linux-rpm

Override the release directory (including CARGO_TARGET_DIR builds):
  python3 package/linux/build.py --build-dir /tmp/jiffshot-linux-target/release

Outputs default to package/out/linux. The default formats are tar.gz and DEB.
Select --format rpm for the native Fedora build. DEB and RPM cannot be selected
together, and the builder rejects Ubuntu's incompatible OCR ABI for Fedora RPMs.
--version (or JIFFSHOT_VERSION) must match the
workspace's X.Y.Z version. SOURCE_DATE_EPOCH controls archive timestamps
(default: 0). SHA256SUMS-linux-x64.txt covers the generated artifacts.
The script never modifies the build input or installs the application.

Package tests:
  JIFFSHOT_TEST_FORMATS=tar.gz,deb python3 -m unittest discover \
    -s package/linux -p test_build.py -v
  JIFFSHOT_TEST_FORMATS=rpm JIFFSHOT_PACKAGE_DIR=package/out/linux-rpm \
    python3 -m unittest discover -s package/linux -p test_build.py -v
  sudo apt install xvfb x11-utils
  python3 package/linux/smoke.py --launcher /path/to/extracted/bin/jiffshot
The smoke test uses a private Xvfb display, temporary settings, and a generated
image. It requires a visible app window and all three private runtimes loaded,
then stops its own processes. CI also verifies clean installation/removal and
desktop/MIME registration on Ubuntu 24.04 and Fedora 43.
WSLg may expose a read-only /tmp/.X11-unix; in that case run the smoke test
in an isolated container/rootfs instead of changing WSLg's socket directory.

The source CI also tests global shortcuts, responsive file loading, native
file-picker fallback, image
export and effects, saved-capture sharing, webcam preferences, screenshots,
screen text OCR, timed, interval and daily capture, tray and login startup
with a private Xvfb display and mock D-Bus services. It requires
python3-dbus-next, libxtst6, libgtk-3-0t64, dbus, Xvfb, xclip, xwininfo,
gcc, and English Tesseract data:
  dbus-run-session -- python3 app/tests/linux_shortcuts_e2e.py \
    --binary target/release/jiffshot --runtime-dir target/release
  dbus-run-session -- python3 app/tests/linux_load_e2e.py \
    --binary target/release/jiffshot --runtime-dir target/release
  dbus-run-session -- python3 app/tests/linux_export_e2e.py \
    --binary target/release/jiffshot --runtime-dir target/release
  dbus-run-session -- python3 app/tests/linux_file_dialog_fallback_e2e.py \
    --binary target/release/jiffshot --runtime-dir target/release
  dbus-run-session -- python3 app/tests/linux_interactive_effects_e2e.py \
    --binary target/release/jiffshot --runtime-dir target/release
  dbus-run-session -- python3 app/tests/linux_share_e2e.py \
    --binary target/release/jiffshot --runtime-dir target/release
  dbus-run-session -- python3 app/tests/linux_webcam_e2e.py \
    --binary target/release/jiffshot --runtime-dir target/release
  dbus-run-session -- python3 app/tests/linux_screenshot_e2e.py \
    --binary target/release/jiffshot --runtime-dir target/release
  dbus-run-session -- python3 app/tests/linux_grab_text_e2e.py \
    --binary target/release/jiffshot --runtime-dir target/release
  dbus-run-session -- python3 app/tests/linux_timed_capture_e2e.py \
    --binary target/release/jiffshot --runtime-dir target/release
  dbus-run-session -- python3 app/tests/linux_interval_e2e.py \
    --binary target/release/jiffshot --runtime-dir target/release
  dbus-run-session -- python3 app/tests/linux_schedule_e2e.py \
    --binary target/release/jiffshot --runtime-dir target/release
  dbus-run-session -- python3 app/tests/linux_tray_e2e.py \
    --binary target/release/jiffshot --runtime-dir target/release
  dbus-run-session -- python3 app/tests/linux_autostart_e2e.py \
    --binary target/release/jiffshot --runtime-dir target/release
This mock does not verify a real desktop's shortcut activation.

The source repository does not currently specify an application license;
the RPM records LicenseRef-Unknown rather than granting a new license.
Bundled native runtimes retain their upstream licensing terms.
