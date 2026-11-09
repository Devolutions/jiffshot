#!/usr/bin/env pwsh
#Requires -Version 7.4

param([switch]$DebugBuild)

$env:XDG_SESSION_TYPE = 'x11'
Remove-Item Env:\WAYLAND_DISPLAY -ErrorAction SilentlyContinue
$profile = if ($DebugBuild) { @() } else { @('--release') }
& xvfb-run -a -s '-screen 0 1280x900x24 -noreset -nolisten tcp' cargo test @profile --locked -p jiffshot native_x11_activity -- --ignored --test-threads=1
if ($LASTEXITCODE -ne 0) { throw 'X11 activity tracking test failed' }
