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
$app = $server = $driver = $null
$work = New-E2EWorkDirectory 'jiffshot-screenshot-ui-'
try {
    New-E2EPrivateDirectory (Join-Path $work 'runtime') | Out-Null
    $config = New-Item -ItemType Directory -Force -Path (Join-Path $work 'config/Jiffshot')
    $captures = New-Item -ItemType Directory -Path (Join-Path $work 'captures')
    Write-E2EJson (Join-Path $config 'settings.json') ([ordered]@{
            theme             = 'Light'
            capture_directory = $captures.FullName
        })
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
        foreach ($mode in @('success', 'hold', 'cancel', 'remote')) {
            $portal.ScreenshotMode = switch ($mode) { 'hold' { 'hold' } 'cancel' { 'cancel' } default { 'success' } }
            $portal.ScreenshotUri = ($mode -eq 'remote') ? 'https://example.invalid/screenshot.png' : $sourceUri
            $expectedRequests = $portal.ScreenshotRequests.Count + 1
            $beforeFiles = Get-CaptureFiles $captures.FullName
            $app = Start-E2EProcess @($binaryPath, '--capture-interactive') $environment $work $appLog
            Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } "the editor for $mode" 12
            $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
            Wait-E2EUntil { $portal.ScreenshotRequests.Count -eq $expectedRequests } "the screenshot request for $mode"
            $requestPath, $options = $portal.ScreenshotRequests[$portal.ScreenshotRequests.Count - 1]
            Assert-E2E ($options['interactive'].Value -eq $true) "$($options | Out-String)"
            Assert-E2E ($options['modal'].Value -eq $true) "$($options | Out-String)"
            Assert-E2E ($portal.CaptureRequests -eq 0) 'interactive screenshots must use the Screenshot portal'
            Wait-E2ESeconds 0.5
            if ($mode -eq 'hold') {
                Invoke-E2EDrawArrow $driver
                Wait-E2EUntil { $driver.RedCanvasPixels() -gt 40 } 'an annotation while the screenshot chooser is open'
                $beforePixels = $driver.RedCanvasPixels()
                Send-PortalResponse $portal $requestPath 0 @{ uri = (New-DBusVariant 's' $sourceUri) }
            }
            if ($mode -in @('success', 'hold')) {
                Wait-E2EUntil {
                    (Get-AddedFiles (Get-CaptureFiles $captures.FullName) $beforeFiles).Count -eq 1
                } "the saved screenshot for $mode"
                if ($mode -eq 'success') {
                    Wait-E2EUntil { $driver.RedCanvasPixels() -gt 5000 } 'the chosen screenshot in the editor'
                }
                else {
                    Wait-E2ESeconds 2
                    $afterPixels = $driver.RedCanvasPixels()
                    Assert-E2E ($beforePixels * 0.8 -le $afterPixels -and $afterPixels -le $beforePixels * 1.5 + 50) "a late screenshot replaced newer edits: $beforePixels, $afterPixels"
                }
            }
            else {
                Wait-E2ESeconds 1
                $afterFiles = Get-CaptureFiles $captures.FullName
                Assert-E2E ($afterFiles.Count -eq $beforeFiles.Count -and (Get-AddedFiles $afterFiles $beforeFiles).Count -eq 0) "$mode must not create a capture"
                Assert-E2E (-not $app.HasExited) "app exited after $mode"
            }
            Close-E2EX11Driver $driver
            $driver = $null
            Stop-E2EProcess $app
            $app = $null
        }
        Write-E2ELine 'Interactive screenshots saved and loaded, preserved edits, and rejected invalid results'
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
