#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify XDG login startup and tray fallback in a private desktop session.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.Desktop.psm1') -Force

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

namespace JiffshotE2E.Autostart
{
    /// <summary>Parses and launches XDG desktop entries through GIO, as a desktop session does.</summary>
    public static class Gio
    {
        private const string GioLibrary = "libgio-2.0.so.0";
        private const string GObjectLibrary = "libgobject-2.0.so.0";
        private const string GLibLibrary = "libglib-2.0.so.0";

        [StructLayout(LayoutKind.Sequential)]
        private struct GError
        {
            public uint Domain;
            public int Code;
            public IntPtr Message;
        }

        [DllImport(GioLibrary)] private static extern IntPtr g_desktop_app_info_new_from_filename([MarshalAs(UnmanagedType.LPUTF8Str)] string filename);
        [DllImport(GioLibrary)] private static extern int g_desktop_app_info_get_is_hidden(IntPtr info);
        [DllImport(GioLibrary)] private static extern IntPtr g_app_info_get_executable(IntPtr info);
        [DllImport(GioLibrary)] private static extern int g_app_info_launch(IntPtr info, IntPtr files, IntPtr context, out IntPtr error);
        [DllImport(GioLibrary)] private static extern IntPtr g_app_launch_context_new();
        [DllImport(GioLibrary)] private static extern void g_app_launch_context_setenv(IntPtr context, [MarshalAs(UnmanagedType.LPUTF8Str)] string variable, [MarshalAs(UnmanagedType.LPUTF8Str)] string value);
        [DllImport(GioLibrary)] private static extern void g_app_launch_context_unsetenv(IntPtr context, [MarshalAs(UnmanagedType.LPUTF8Str)] string variable);
        [DllImport(GObjectLibrary)] private static extern void g_object_unref(IntPtr instance);
        [DllImport(GLibLibrary)] private static extern void g_error_free(IntPtr error);

        /// <summary>Returns the parsed entry, or null when GLib rejects it.</summary>
        public static DesktopEntry Load(string filename)
        {
            IntPtr info = g_desktop_app_info_new_from_filename(filename);
            return info == IntPtr.Zero ? null : new DesktopEntry(info);
        }

        public sealed class DesktopEntry : IDisposable
        {
            private IntPtr _info;

            internal DesktopEntry(IntPtr info)
            {
                _info = info;
            }

            public bool IsHidden => g_desktop_app_info_get_is_hidden(_info) != 0;

            public string Executable => Marshal.PtrToStringUTF8(g_app_info_get_executable(_info));

            /// <summary>Launches the entry without files in an environment like the session's.</summary>
            public bool Launch(System.Collections.IDictionary environment, string[] unset)
            {
                IntPtr context = g_app_launch_context_new();
                try
                {
                    foreach (System.Collections.DictionaryEntry entry in environment)
                    {
                        g_app_launch_context_setenv(context, (string)entry.Key, Convert.ToString(entry.Value));
                    }
                    foreach (string name in unset)
                    {
                        g_app_launch_context_unsetenv(context, name);
                    }
                    if (g_app_info_launch(_info, IntPtr.Zero, context, out IntPtr error) != 0)
                    {
                        return true;
                    }
                    if (error != IntPtr.Zero)
                    {
                        string message = Marshal.PtrToStringUTF8(Marshal.PtrToStructure<GError>(error).Message);
                        g_error_free(error);
                        throw new InvalidOperationException("GLib could not launch the desktop entry: " + message);
                    }
                    return false;
                }
                finally
                {
                    g_object_unref(context);
                }
            }

            public void Dispose()
            {
                if (_info != IntPtr.Zero)
                {
                    g_object_unref(_info);
                    _info = IntPtr.Zero;
                }
            }
        }
    }
}
'@

function Test-Running {
    param([Parameter(Mandatory)][int]$ProcessId)
    try {
        $stat = [System.IO.File]::ReadAllText("/proc/$ProcessId/stat")
    }
    catch [System.IO.FileNotFoundException], [System.IO.DirectoryNotFoundException] {
        return $false
    }
    $stat[$stat.LastIndexOf(') ') + 2] -ne 'Z'
}

function ConvertTo-ShellLiteral {
    param([Parameter(Mandatory)][string]$Text)
    "'" + $Text.Replace("'", "'\''") + "'"
}

function Start-App {
    Start-E2EProcess @($binaryPath) $environment $work $appLog
}

function Connect-Editor {
    Connect-E2EWindow $environment 'the editor' 12
}

function Invoke-ClickLabel {
    param([Parameter(Mandatory)][string]$Label)
    $position = Find-E2EScrollingLabel $reader $driver $Label
    $driver.Click($position[0], $position[1])
}

function Test-DesktopEntry {
    & desktop-file-validate $entry
    if ($LASTEXITCODE -ne 0) {
        throw "desktop-file-validate $entry exited with $LASTEXITCODE"
    }
}

function Invoke-LaunchEntry {
    Remove-Item -LiteralPath $marker -Force -ErrorAction SilentlyContinue
    $desktopEntry = [JiffshotE2E.Autostart.Gio]::Load($entry)
    Assert-E2E ($null -ne $desktopEntry) "GLib rejected the desktop entry: $([System.IO.File]::ReadAllText($entry))"
    try {
        Assert-E2E (-not $desktopEntry.IsHidden) 'GLib must not hide the enabled desktop entry'
        Assert-E2E ($desktopEntry.Launch($environment, @('WAYLAND_DISPLAY'))) 'GLib must launch the desktop entry'
    }
    finally {
        $desktopEntry.Dispose()
    }
    Wait-E2EUntil { Test-Path -LiteralPath $marker } 'the autostart executable' 10
    $launched = Read-E2EJson $marker
    $arguments = @($launched[1])
    Assert-E2E (Test-E2EDeepEqual $arguments @('--background')) "unexpected autostart arguments: $($arguments -join ' ')"
    [int]$launched[0]
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$portal = New-MockPortal
$watcher = $app = $server = $driver = $reader = $launchedPid = $null
$work = New-E2EWorkDirectory 'jiffshot-autostart-ui-'
try {
    New-E2EPrivateDirectory (Join-Path $work 'runtime') | Out-Null
    $config = New-Item -ItemType Directory -Force -Path (Join-Path $work 'config/Jiffshot')
    $settingsPath = Join-Path $config 'settings.json'
    Write-E2EJson $settingsPath ([ordered]@{ theme = 'Light'; linux_tray_enabled = $false })
    $entry = Join-Path $work 'config/autostart/com.devolutions.Jiffshot.desktop'
    $marker = Join-Path $work 'launched.json'
    $launcher = [System.IO.Path]::Combine($work, 'start $`"percent%\ space')
    $launcherScript = @(
        '#!/bin/sh'
        "marker=$(ConvertTo-ShellLiteral $marker)"
        'arguments='
        'for argument in "$@"; do'
        '    escaped=$(printf ''%s'' "$argument" | sed -e ''s/\\/\\\\/g'' -e ''s/"/\\"/g'')'
        '    arguments="${arguments:+$arguments, }\"$escaped\""'
        'done'
        'printf ''[%s, [%s]]'' "$$" "$arguments" > "$marker.partial"'
        'mv -f "$marker.partial" "$marker"'
        "exec $(ConvertTo-ShellLiteral $binaryPath) `"`$@`""
    ) -join "`n"
    [System.IO.File]::WriteAllText($launcher, $launcherScript + "`n")
    [JiffshotE2E.Posix]::Chmod($launcher, [Convert]::ToInt32('755', 8))
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $environment = New-E2EEnvironment $server.Display $work $runtime
        $environment['JIFFSHOT_LAUNCHER'] = $launcher
        $reader = New-E2ETextReader

        $app = Start-App
        $driver = Connect-Editor
        Wait-E2ESeconds 0.5
        Invoke-ClickLabel 'Enable login startup'
        Wait-E2EUntil { Test-Path -LiteralPath $entry } 'the enabled startup entry'
        Test-DesktopEntry
        Assert-E2E ([System.IO.File]::ReadAllText($entry).Contains("X-Jiffshot-Autostart=true`n")) 'the enabled startup entry must be marked as managed by Jiffshot'
        $driver.RequestClose()
        Wait-E2EUntil { $app.HasExited } 'the initial editor to close'
        Close-E2EX11Driver $driver
        $driver = $null
        $app = $null

        $launchedPid = Invoke-LaunchEntry
        $driver = Connect-Editor
        Assert-E2E (Test-Running $launchedPid) 'the autostarted app must be running'
        $driver.RequestClose()
        Wait-E2EUntil { -not (Test-Running $launchedPid) } 'close without an enabled tray'
        $launchedPid = $null
        Close-E2EX11Driver $driver
        $driver = $null
        Write-E2ELine 'Desktop startup preserves a quoted launcher and opens without tray opt-in'

        Write-E2EJson $settingsPath ([ordered]@{ theme = 'Light'; linux_tray_enabled = $true })
        $launchedPid = Invoke-LaunchEntry
        $driver = Connect-Editor
        Wait-E2ESeconds 1.5
        Assert-E2E ($null -ne (Get-E2EWindowInfo $environment)) 'a missing watcher must not hide an autostarted editor'
        $driver.RequestClose()
        Wait-E2EUntil { -not (Test-Running $launchedPid) } 'close without a tray watcher'
        $launchedPid = $null
        Close-E2EX11Driver $driver
        $driver = $null
        Write-E2ELine 'Login startup remains visible when the tray is unavailable'

        $watcher = New-E2EMockWatcher
        Write-E2EJson $settingsPath ([ordered]@{
                theme                          = 'Light'
                linux_tray_enabled             = $true
                linux_global_shortcuts_enabled = $true
                global_capture_shortcut        = 'CTRL+ALT+P'
                global_recording_shortcuts     = $true
            })
        $launchedPid = Invoke-LaunchEntry
        Wait-E2EUntil { $watcher.Items.Count -gt 0 -and $watcher.HostQueries -ge 2 } 'the autostarted tray registration'
        Wait-E2EUntil { $portal.Bindings.Count -gt 0 } 'shortcuts restored at login'
        $shortcutSession, $boundShortcuts = $portal.Bindings[$portal.Bindings.Count - 1]
        $boundIds = @(foreach ($binding in $boundShortcuts) { $binding[0] })
        Assert-E2E (Test-E2EDeepEqual $boundIds @('capture', 'pause-recording', 'stop-recording')) "unexpected restored shortcuts: $($boundIds -join ', ')"
        $trigger = $boundShortcuts[0][1]['preferred_trigger'].Value
        Assert-E2E ($trigger -ceq 'CTRL+ALT+P') "unexpected restored capture trigger $trigger"
        Wait-E2EUntil { $null -eq (Get-E2EWindowInfo $environment) } 'login startup to hide in the tray'
        Assert-E2E (Test-Running $launchedPid) 'background startup must keep the app running'
        $previous = $portal.CaptureRequests
        Send-PortalActivated $portal $shortcutSession 'capture'
        Wait-E2EUntil { $portal.CaptureRequests -gt $previous } 'a restored shortcut to activate while hidden'
        Wait-E2ESeconds 0.6
        $trayItem = $watcher.Items[$watcher.Items.Count - 1]
        Invoke-E2ETrayActivate $watcher $trayItem
        $driver = Connect-Editor
        $trayMenu = Get-E2ETrayMenu $watcher $trayItem
        Invoke-E2ETrayMenuItem $watcher $trayItem $trayMenu['Quit Jiffshot']
        Wait-E2EUntil { -not (Test-Running $launchedPid) } 'the autostarted app to quit'
        $launchedPid = $null
        Close-E2EX11Driver $driver
        $driver = $null
        Write-E2ELine 'Login startup restores shortcuts and hides only after tray registration'

        $previousItems = $watcher.Items.Count
        $app = Start-App
        $driver = Connect-Editor
        Wait-E2EUntil { $watcher.Items.Count -gt $previousItems } 'the normal launch tray item'
        Wait-E2ESeconds 0.5
        Invoke-ClickLabel 'Disable login startup'
        Wait-E2EUntil { [System.IO.File]::ReadAllText($entry).Contains("Hidden=true`n") } 'startup to be disabled'
        Test-DesktopEntry
        $desktopEntry = [JiffshotE2E.Autostart.Gio]::Load($entry)
        try {
            Assert-E2E ($null -ne $desktopEntry -and $desktopEntry.IsHidden) 'GLib must parse the disabled desktop entry as hidden'
            Assert-E2E ($desktopEntry.Executable -ceq '/usr/bin/true') 'disabled portable startup must parse without Jiffshot on PATH'
        }
        finally {
            if ($null -ne $desktopEntry) {
                $desktopEntry.Dispose()
            }
        }
        [System.IO.File]::WriteAllText($entry, "unmanaged startup configuration`n")
        Invoke-ClickLabel 'Enable login startup'
        Wait-E2ESeconds 0.5
        Assert-E2E ([System.IO.File]::ReadAllText($entry) -ceq "unmanaged startup configuration`n") 'enabling startup must leave an unmanaged entry untouched'
        $trayItem = $watcher.Items[$watcher.Items.Count - 1]
        $trayMenu = Get-E2ETrayMenu $watcher $trayItem
        Invoke-E2ETrayMenuItem $watcher $trayItem $trayMenu['Quit Jiffshot']
        Wait-E2EUntil { $app.HasExited } 'the normal launch to quit'
        $app = $null
        Close-E2EX11Driver $driver
        $driver = $null
        Write-E2ELine 'Disabling startup suppresses autostart and leaves unmanaged entries untouched'
    }
    catch {
        if ($null -ne $driver -and $null -ne (Get-E2EWindowInfo $environment)) {
            Write-E2ETrace $driver 'autostart-failure'
        }
        Write-E2ELogTail @($xvfbLog, $appLog)
        throw
    }
    finally {
        Close-E2EX11Driver $driver
        if ($null -ne $reader) {
            $reader.Dispose()
        }
        Stop-E2EProcess $app
        if ($null -ne $launchedPid -and (Test-Running $launchedPid)) {
            [JiffshotE2E.Posix]::Kill($launchedPid, 15)
            Wait-E2EUntil { -not (Test-Running $launchedPid) } 'autostart cleanup'
        }
        if ($null -ne $server) {
            Stop-E2EProcess $server.Process
        }
    }
}
finally {
    Unregister-E2EService $watcher
    Unregister-E2EService $portal
    Remove-E2EWorkDirectory $work
}
