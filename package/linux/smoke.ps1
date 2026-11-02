#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Launch a packaged app in a private Xvfb display without user settings.

.PARAMETER Launcher
Packaged bin/jiffshot launcher to start (symbolic links are resolved).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Launcher
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

Import-Module (Join-Path $PSScriptRoot 'JiffshotLinuxSmoke.psm1') -Force

# A 2x2 RGBA PNG; the path contains spaces to exercise launcher argument quoting.
$png = 'iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAYAAABytg0kAAAAEUlEQVR4nGP4z8DwH4QZYAwAR8oH+WdZbrcAAAAASUVORK5CYII='
$libraries = @('librustolonia_host.so', 'libSkiaSharp.so', 'libHarfBuzzSharp.so')

function Exit-Usage([string]$Message) {
    [Console]::Error.WriteLine("smoke.ps1: error: $Message")
    exit 2
}

function Start-ShellProcess([string]$Script, [string[]]$Arguments, [string]$WorkingDirectory, [hashtable]$Environment) {
    $info = [System.Diagnostics.ProcessStartInfo]::new('/bin/sh')
    foreach ($argument in @('-c', $Script, 'sh') + $Arguments) {
        $info.ArgumentList.Add($argument)
    }
    $info.UseShellExecute = $false
    $info.WorkingDirectory = $WorkingDirectory
    if ($Environment) {
        foreach ($key in $Environment.Keys) {
            if ($null -eq $Environment[$key]) {
                [void]$info.Environment.Remove($key)
            }
            else {
                $info.Environment[$key] = $Environment[$key]
            }
        }
    }
    [System.Diagnostics.Process]::Start($info)
}

function Stop-SmokeProcess([System.Diagnostics.Process]$Process) {
    if ($null -eq $Process) { return }
    try {
        if (-not $Process.HasExited) {
            & /bin/sh -c 'kill -TERM "$1" 2>/dev/null || true' sh "$($Process.Id)"
            if (-not $Process.WaitForExit(5000)) {
                $Process.Kill()
                [void]$Process.WaitForExit(5000)
            }
        }
    }
    finally {
        $Process.Dispose()
    }
}

function Read-LogTail([string]$Path) {
    if (-not [System.IO.File]::Exists($Path)) { return '' }
    $text = [System.Text.Encoding]::UTF8.GetString([System.IO.File]::ReadAllBytes($Path))
    if ($text.Length -gt 16384) { $text.Substring($text.Length - 16384) } else { $text }
}

function Get-XWindowInfo([string]$Display, [string[]]$Arguments) {
    $info = [System.Diagnostics.ProcessStartInfo]::new('xwininfo')
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.Environment['DISPLAY'] = $Display
    $query = [System.Diagnostics.Process]::Start($info)
    try {
        $output = $query.StandardOutput.ReadToEnd()
        $query.WaitForExit()
        if ($query.ExitCode -ne 0) {
            throw "xwininfo $($Arguments -join ' ') returned non-zero exit status $($query.ExitCode)."
        }
        $output
    }
    finally {
        $query.Dispose()
    }
}

foreach ($tool in 'Xvfb', 'xwininfo') {
    if (-not (Get-Command -Name $tool -CommandType Application -ErrorAction Ignore)) {
        Exit-Usage "missing smoke-test tool: $tool"
    }
}
$sockets = '/tmp/.X11-unix'
if ([System.IO.Directory]::Exists($sockets)) {
    # access(W_OK) also reports read-only mounts.
    & { $PSNativeCommandUseErrorActionPreference = $false; /bin/sh -c 'test -w "$1"' sh $sockets }
    if ($LASTEXITCODE -ne 0) {
        Exit-Usage 'X11 sockets are read-only; run this smoke test in an isolated container/rootfs'
    }
}
$Launcher = [string](& realpath -e -- $Launcher)

$work = [System.IO.Directory]::CreateTempSubdirectory('jiffshot-smoke-').FullName
$server = $app = $null
try {
    $image = Join-Path $work 'input image with spaces.png'
    [System.IO.File]::WriteAllBytes($image, [System.Convert]::FromBase64String($png))
    $runtime = Join-Path $work 'runtime'
    [void][System.IO.Directory]::CreateDirectory($runtime, [System.IO.UnixFileMode]'UserRead, UserWrite, UserExecute')
    $displayFile = Join-Path $work 'display'
    $serverLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    [System.IO.File]::WriteAllText($displayFile, '')
    try {
        $server = Start-ShellProcess 'exec Xvfb -displayfd 3 -screen 0 1280x900x24 -nolisten tcp -noreset 3>"$1" >"$2" 2>&1' @($displayFile, $serverLog) $work
        $deadline = [DateTime]::UtcNow.AddSeconds(10)
        $display = ''
        while ([DateTime]::UtcNow -lt $deadline) {
            $display = [System.IO.File]::ReadAllText($displayFile)
            if ($display.EndsWith("`n") -or $server.HasExited) { break }
            Start-Sleep -Milliseconds 50
        }
        if (-not $display.EndsWith("`n")) {
            throw 'private Xvfb did not start'
        }
        $display = $display.Trim()
        if ($display -cnotmatch '\A[0-9]+\z') {
            throw "Xvfb returned an invalid display: '$display'"
        }
        $environment = @{
            DISPLAY                  = ":$display"
            HOME                     = $work
            XDG_CONFIG_HOME          = Join-Path $work 'config'
            XDG_DATA_HOME            = Join-Path $work 'data'
            XDG_CACHE_HOME           = Join-Path $work 'cache'
            XDG_RUNTIME_DIR          = $runtime
            WAYLAND_DISPLAY          = $null
            DBUS_SESSION_BUS_ADDRESS = $null
        }
        # exec keeps the shell PID, so /proc/<pid>/maps describes the launched app.
        $app = Start-ShellProcess 'log=$1; shift; exec "$@" </dev/null >"$log" 2>&1' @($appLog, $Launcher, $image) $work $environment
        $deadline = [DateTime]::UtcNow.AddSeconds(30)
        $ready = $false
        $windows = $windowInfo = ''
        $missingLibraries = @($libraries)
        $imageName = [System.IO.Path]::GetFileName($image)
        while ([DateTime]::UtcNow -lt $deadline) {
            if ($app.HasExited) {
                throw "packaged app exited during startup ($($app.ExitCode))"
            }
            $windows = Get-XWindowInfo ":$display" @('-root', '-tree')
            $windowId = Find-JiffshotSmokeWindowId -WindowTree $windows -ImageName $imageName
            $windowInfo = if ($windowId) { Get-XWindowInfo ":$display" @('-id', $windowId) } else { '' }
            $maps = [System.IO.File]::ReadAllText("/proc/$($app.Id)/maps")
            $missingLibraries = @($libraries | Where-Object { -not $maps.Contains($_) })
            if ($windowId -and $windowInfo -match '(?m)^\s*Map State:\s+IsViewable\s*$' -and $missingLibraries.Count -eq 0) {
                "Packaged app opened the image in a visible editor and loaded all private runtimes: $Launcher"
                $ready = $true
                break
            }
            Start-Sleep -Milliseconds 200
        }
        if (-not $ready) {
            $missing = if ($missingLibraries.Count) { $missingLibraries -join ', ' } else { 'none' }
            [Console]::Error.WriteLine("Missing private runtimes: $missing")
            [Console]::Error.WriteLine("Last window tree:`n$windows")
            if ($windowInfo) {
                [Console]::Error.WriteLine("Last editor window info:`n$windowInfo")
            }
            throw "packaged app did not show '$imageName' in a viewable editor and load all private runtimes"
        }
    }
    catch {
        Read-LogTail $serverLog
        Read-LogTail $appLog
        throw
    }
    finally {
        Stop-SmokeProcess $app
        Stop-SmokeProcess $server
    }
}
finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction Ignore
}
