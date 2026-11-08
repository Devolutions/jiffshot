#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Exercise the desktop screenshot chooser and stale-result protection.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.Desktop.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.Editor.psm1') -Force

function Get-CaptureFiles {
    param([Parameter(Mandatory)][string]$Directory)
    , [string[]]@(Get-ChildItem -LiteralPath $Directory -Force | ForEach-Object FullName)
}

function Get-AddedFiles {
    param([string[]]$After, [string[]]$Before)
    , [string[]]@($After | Where-Object { $Before -cnotcontains $_ })
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$portal = New-MockPortal
$watcher = New-E2EMockWatcher
$app = $server = $driver = $reader = $null
$work = New-E2EWorkDirectory 'jiffshot-screenshot-ui-'
try {
    New-E2EPrivateDirectory (Join-Path $work 'runtime') | Out-Null
    $config = New-Item -ItemType Directory -Force -Path (Join-Path $work 'config/Jiffshot')
    $settingsPath = Join-Path $config 'settings.json'
    $captures = New-Item -ItemType Directory -Path (Join-Path $work 'captures')
    $source = Join-Path $work 'desktop screenshot.png'
    [System.IO.File]::WriteAllBytes($source, (Get-E2ESolidPng 255 0 0))
    $sourceUri = ConvertTo-E2EFileUri $source
    $portal.ScreenshotUri = $sourceUri
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $displayName = $server.Display
        $environment = New-E2EEnvironment $displayName $work $runtime
        $environment['JIFFSHOT_CLASSIC_UI'] = '1'
        $environment['JIFFSHOT_LIBRARY'] = $captures.FullName
        $environment['JIFFSHOT_CONFIG_DIR'] = $config.FullName
        $environment['JIFFSHOT_LOCAL_DIR'] = Join-Path $work 'local'
        $reader = New-E2ETextReader
        foreach ($mode in @('success', 'hold', 'cancel', 'remote')) {
            Write-E2EJson $settingsPath ([ordered]@{
                    theme              = 'Light'
                    capture_directory  = $captures.FullName
                    linux_tray_enabled = ($mode -eq 'hold')
                })
            $portal.ScreenshotMode = switch ($mode) { 'hold' { 'hold' } 'cancel' { 'cancel' } default { 'success' } }
            $portal.ScreenshotUri = ($mode -eq 'remote') ? 'https://example.invalid/screenshot.png' : $sourceUri
            $expectedRequests = $portal.ScreenshotRequests.Count + 1
            $beforeFiles = Get-CaptureFiles $captures.FullName
            $previousItems = $watcher.Items.Count
            $previousQueries = $watcher.HostQueries
            $app = Start-E2EProcess @($binaryPath, '--capture-interactive') $environment $work $appLog
            Wait-E2EUntil { $portal.ScreenshotRequests.Count -eq $expectedRequests } "the screenshot request for $mode"
            $requestPath, $options = $portal.ScreenshotRequests[$portal.ScreenshotRequests.Count - 1]
            Assert-E2E ($options['interactive'].Value -eq $true) "$($options | Out-String)"
            Assert-E2E ($options['modal'].Value -eq $true) "$($options | Out-String)"
            Assert-E2E ($portal.CaptureRequests -eq 0) 'interactive screenshots must use the Screenshot portal'
            if ($mode -eq 'hold') {
                Assert-E2E ($null -eq (Get-E2EWindowInfo $environment)) 'the editor must be hidden while the chooser is open'
                Wait-E2EUntil {
                    $watcher.Items.Count -gt $previousItems -and $watcher.HostQueries -gt $previousQueries
                } 'the screenshot test tray'
                $trayItem = $watcher.Items[-1]
                Wait-E2EUntil {
                    if ($null -eq (Get-E2EWindowInfo $environment)) {
                        Invoke-E2ETrayActivate $watcher $trayItem
                    }
                    $null -ne (Get-E2EWindowInfo $environment)
                } 'the editor reopened from the tray while the chooser is open'
                $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
                Wait-E2EUntil {
                    Invoke-E2EHotkey $driver '2'
                    (Get-E2EStatusText $reader $driver).ToLowerInvariant().Contains('arrow tool selected')
                } 'the reopened editor to accept the Arrow tool shortcut'
                Wait-E2EUntil { $driver.Capture(275, 315, 220, 100).CountColor(255, 255, 255) -gt 10000 } 'the reopened editor canvas to redraw'
                $driver.Move(300, 330)
                $driver.Button(1, $true)
                $driver.Move(500, 420)
                $driver.Button(1, $false)
                $driver.Sync()
                Wait-E2EUntil { $driver.RedCanvasPixels() -gt 40 } 'an annotation while the screenshot chooser is open'
                $beforePixels = $driver.RedCanvasPixels()
                Send-PortalResponse $portal $requestPath 0 @{ uri = (New-DBusVariant 's' $sourceUri) }
            }
            else {
                Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } "the editor after $mode" 12
                $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
            }
            if ($mode -in @('success', 'hold')) {
                Wait-E2EUntil {
                    (Get-AddedFiles (Get-CaptureFiles $captures.FullName) $beforeFiles).Count -eq 1
                } "the saved screenshot for $mode"
                if ($mode -eq 'success') {
                    Wait-E2EUntil { $driver.RedCanvasPixels() -gt 5000 } 'the chosen screenshot in the editor'
                }
                else {
                    Wait-E2EStatus $reader $driver 'Desktop screenshot captured' 20
                    $afterPixels = $driver.RedCanvasPixels()
                    Assert-E2E ($beforePixels * 0.8 -le $afterPixels -and $afterPixels -le $beforePixels * 1.5 + 50) "a late screenshot replaced newer edits: $beforePixels, $afterPixels"
                    Invoke-E2EHotkey $driver 'z'
                    Wait-E2EUntil { $driver.RedCanvasPixels() -lt $beforePixels * 0.2 } 'Undo after a late screenshot'
                    Invoke-E2EHotkey $driver 'y'
                    Wait-E2EUntil { $driver.RedCanvasPixels() -ge $beforePixels * 0.8 } 'Redo after a late screenshot'
                }
            }
            else {
                $expectedStatus = if ($mode -eq 'cancel') { 'Desktop screenshot cancelled' } else { 'non-local screenshot URI' }
                Wait-E2EStatus $reader $driver $expectedStatus 20
                $afterFiles = Get-CaptureFiles $captures.FullName
                Assert-E2E ($afterFiles.Count -eq $beforeFiles.Count -and (Get-AddedFiles $afterFiles $beforeFiles).Count -eq 0) "$mode must not create a capture"
                Assert-E2E (-not $app.HasExited) "app exited after $mode"
            }
            $case = if ($mode -eq 'hold') { 'late_result_preserves_newer_edits' } else { $mode }
            Write-E2EPass "classic_screenshot_$case"
            Close-E2EX11Driver $driver
            $driver = $null
            Stop-E2EProcess $app
            $app = $null
        }
        Write-E2ELine 'Interactive screenshots saved and loaded, preserved edits, and rejected invalid results'
    }
    catch {
        if ($null -ne $driver -and $null -ne (Get-E2EWindowInfo $environment)) {
            Write-E2ETrace $driver 'failure' 'screenshot'
        }
        Write-E2ELogTail @($xvfbLog, $appLog)
        throw
    }
    finally {
        if ($null -ne $reader) { $reader.Dispose() }
        Close-E2EX11Driver $driver
        Stop-E2EProcess $app
        if ($null -ne $server) { Stop-E2EProcess $server.Process }
    }
}
finally {
    Unregister-E2EService $watcher
    Unregister-E2EService $portal
    Remove-E2EWorkDirectory $work
}
