Jiffshot 0.4.0 (portable)
=========================

Screen capture and image editor for Windows 10 and 11.

Getting started
- Unzip this folder anywhere and run jiffshot.exe. No installer is needed.
- Jiffshot lives in the notification area (the tray icon). Left-click it to
  open the Capture Window; its menu captures, records video, opens the editor
  and has Preferences.
- Print Screen captures (Ctrl+Alt+Print Screen while another app, such
  as another capture tool or the Snipping Tool, holds Print Screen). Click
  a window, drag an area, or press F1 while selecting for all keys.
- Closing the editor keeps Jiffshot in the tray; the tray's Exit quits.
- On the first launch, Jiffshot enables startup for this Windows user.
  At sign-in it stays hidden in the tray, including the Capture Widget;
  opening Jiffshot restores the widget if it was enabled and shows any
  startup warnings. Turn off "Run when Windows starts" in the tray menu
  or Capture preferences to opt out. Windows Settings > Apps > Startup
  can also disable startup.
- jiffshot-cli.exe drives Jiffshot from scripts and AI agents (screenshots,
  recordings, step guides, clicks and typing). Run "jiffshot-cli help".

Where things go
- Captures: Pictures\Jiffshot (Preferences > Library to change it).
- Settings: %APPDATA%\Jiffshot\jiffshot.json.
- Before moving or removing the portable folder, turn off "Run when
  Windows starts", then quit Jiffshot. Delete the folder (and the capture
  and settings folders, if you like). After moving it, launch the new
  copy and re-enable startup if desired.

Notes
- Windows may warn that the app is from an unknown publisher: the build
  is not code-signed. Choose More info > Run anyway.
- Video recording uses Windows Media Foundation, which Windows "N"
  editions leave out (Media Feature Pack from Microsoft adds it back).
