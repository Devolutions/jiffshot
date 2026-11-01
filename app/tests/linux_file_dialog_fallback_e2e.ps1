#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify that the native file chooser works when the portal is unavailable.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.Editor.psm1') -Force

if (-not ('JiffshotE2E.FileDialogFocus' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace JiffshotE2E
{
    public static class FileDialogFocus
    {
        [DllImport("libX11.so.6")]
        public static extern int XSetInputFocus(IntPtr display, UIntPtr window, int revertTo, UIntPtr time);
    }
}
'@
}

$DialogTitle = 'Open image, video, or document'

function New-NoFileChooserPortal {
    $portal = New-MockPortal
    $portal | Add-Member -NotePropertyMembers ([ordered]@{ MissingQueries = 0; OpenRequests = 0 })
    $portal.Handler = ${function:Invoke-NoFileChooserPortalMessage}
    $portal
}

function Invoke-NoFileChooserPortalMessage {
    param($Portal, [JiffshotE2E.IncomingCall]$Call)
    if ($Call.Interface -eq $PortalFileChooserInterface -and $Call.Member -eq 'OpenFile') {
        $Portal.OpenRequests++
        $Call.Error('org.freedesktop.DBus.Error.UnknownMethod', "Unknown interface $PortalFileChooserInterface")
        return $true
    }
    if ($Call.Interface -eq 'org.freedesktop.DBus.Properties' -and $Call.Member -eq 'Get' -and $Call.Body[0] -eq $PortalFileChooserInterface) {
        $Portal.MissingQueries++
        $Call.Error('org.freedesktop.DBus.Error.InvalidArgs', "Unknown interface $PortalFileChooserInterface")
        return $true
    }
    Invoke-MockPortalMessage $Portal $Call
}

function Get-DialogWindow {
    param([Parameter(Mandatory)][System.Collections.IDictionary]$Environment)
    $display = $Environment['DISPLAY']
    $tree = & xwininfo -display $display -root -tree 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "xwininfo -root -tree failed: $tree"
    }
    foreach ($line in $tree) {
        $line = [string]$line
        if (-not $line.Contains("`"$DialogTitle`"")) {
            continue
        }
        $window = [Convert]::ToInt64(($line.Trim() -split '\s+')[0].Substring(2), 16)
        $info = (& xwininfo -display $display -id ('0x{0:x}' -f $window) 2>&1) -join "`n"
        if ($LASTEXITCODE -eq 0 -and $info.Contains('Map State: IsViewable')) {
            return $window
        }
    }
    $null
}

function Invoke-Press {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [Parameter(Mandatory)][string]$Key)
    $Driver.Key($Key, $true)
    $Driver.Key($Key, $false)
    $Driver.Sync()
}

function Set-TypedPath {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [Parameter(Mandatory)][string]$Path)
    $special = @{ '/' = 'slash'; '.' = 'period'; '-' = 'minus'; '_' = 'minus' }
    foreach ($character in $Path.ToCharArray()) {
        $text = [string]$character
        $shifted = [char]::IsUpper($character) -or $text -eq '_'
        if ($shifted) {
            $Driver.Key('Shift_L', $true)
        }
        $key = if ($special.ContainsKey($text)) { $special[$text] } else { $text.ToLowerInvariant() }
        Invoke-Press $Driver $key
        if ($shifted) {
            $Driver.Key('Shift_L', $false)
        }
    }
    $Driver.Sync()
}

function Set-InputFocus {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [Parameter(Mandatory)][long]$Window)
    $null = [JiffshotE2E.FileDialogFocus]::XSetInputFocus($Driver.Display, [UIntPtr]::new([uint64]$Window), 2, [UIntPtr]::Zero)
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$app = $server = $driver = $portal = $null
$work = New-E2EWorkDirectory 'jiffshot-native-picker-'
try {
    $portal = New-NoFileChooserPortal
    $original = Join-Path $work 'original.png'
    [System.IO.File]::WriteAllBytes($original, (Get-E2ESolidPng 30 120 190))
    $chosen = Join-Path $work 'chosen.png'
    [System.IO.File]::WriteAllBytes($chosen, (Get-E2ESolidPng 255 0 0))
    $config = Join-Path $work 'config/Jiffshot'
    New-Item -ItemType Directory -Path $config | Out-Null
    Write-E2EJson (Join-Path $config 'settings.json') ([ordered]@{ theme = 'Light' })
    New-E2EPrivateDirectory (Join-Path $work 'runtime') | Out-Null
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $displayName = $server.Display
        $environment = New-E2EEnvironment $displayName $work $runtime
        $environment.Remove('GTK_USE_PORTAL')
        $app = Start-E2EProcess @($binaryPath, $original) $environment $work $appLog
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'the editor'
        $windowInfo = Get-E2EWindowInfo $environment
        $window = $windowInfo.Window
        $driver = New-E2EX11Driver $displayName $windowInfo
        Wait-E2ESeconds 0.5

        $openDialog = {
            Set-InputFocus $driver $window
            Invoke-E2EHotkey $driver 'o'
            Wait-E2EUntil { $null -ne (Get-DialogWindow $environment) } 'the native file chooser'
            $dialog = Get-DialogWindow $environment
            Set-InputFocus $driver $dialog
            $driver.Sync()
            Wait-E2ESeconds 0.5
            $dialog
        }

        $chooser = & $openDialog
        Invoke-Press $driver 'Escape'
        Wait-E2EUntil { (Get-DialogWindow $environment) -ne $chooser } 'native chooser cancellation'
        $chooser = & $openDialog
        Invoke-E2EHotkey $driver 'l'
        Wait-E2ESeconds 0.5
        $driver.SelectAll()
        Set-TypedPath $driver $chosen
        Wait-E2ESeconds 0.5
        Invoke-Press $driver 'Return'
        Wait-E2EUntil { (Get-DialogWindow $environment) -ne $chooser } 'native file selection'
        Wait-E2EUntil { $driver.RedCanvasPixels() -gt 5000 } 'the selected image'
        $chooser = & $openDialog
        Invoke-Press $driver 'Escape'
        Wait-E2EUntil { (Get-DialogWindow $environment) -ne $chooser } 'a subsequent cancellation'
        Assert-E2E ($portal.MissingQueries -ge 1) "$($portal.MissingQueries)"
        Assert-E2E ($portal.OpenRequests -ge 3) "$($portal.OpenRequests)"
        $log = [System.IO.File]::ReadAllText($appLog)
        Assert-E2E $log.Contains('File chooser portal unavailable; using the native picker', [StringComparison]::Ordinal) 'the app must report the native picker fallback'
        Write-E2ELine 'GUI native chooser cancellation, file selection, and retry passed'
    }
    catch {
        Write-E2ELogTail @($xvfbLog, $appLog)
        throw
    }
    finally {
        Close-E2EX11Driver $driver
        Stop-E2EProcess $app
        if ($null -ne $server) { Stop-E2EProcess $server.Process }
    }
}
finally {
    Unregister-E2EService $portal
    Remove-E2EWorkDirectory $work
}
