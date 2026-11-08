#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify screen text capture in the shared editor without replacing its document.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir,
    [string]$OcrFixture
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.Editor.psm1') -Force

function Get-ClipboardText {
    param([Parameter(Mandatory)][System.Collections.IDictionary]$Environment)
    $result = Invoke-E2ECommand @('xclip', '-selection', 'clipboard', '-o') -Environment $Environment -CaptureOutput -Timeout 5
    if ($result.ExitCode -ne 0) { return '' }
    [System.Text.Encoding]::UTF8.GetString($result.Output)
}

function Set-ClipboardText {
    param([Parameter(Mandatory)][System.Collections.IDictionary]$Environment, [Parameter(Mandatory)][string]$Text)
    $null = Invoke-E2ECommand @('xclip', '-selection', 'clipboard') -Environment $Environment -InputBytes ([System.Text.Encoding]::UTF8.GetBytes($Text)) -Check
}

function Wait-ScreenshotChooser {
    param([Parameter(Mandatory)]$Portal, [int]$ScreenshotCount, [int]$ScreenCastCount)
    Wait-E2EUntil {
        $Portal.ScreenshotRequests.Count -gt $ScreenshotCount -or $Portal.CaptureRequests -gt $ScreenCastCount
    } 'the desktop screenshot chooser' 20
    Assert-E2E ($Portal.CaptureRequests -eq $ScreenCastCount) 'interactive capture must not request the ScreenCast portal'
    Assert-E2E ($Portal.ScreenshotRequests.Count -eq $ScreenshotCount + 1) 'interactive capture must use the Screenshot portal'
    $requestPath, $options = $Portal.ScreenshotRequests[-1]
    foreach ($name in @('interactive', 'modal')) {
        Assert-E2E ($options[$name].Value -is [bool] -and $options[$name].Value) "$name must be True"
    }
    $requestPath
}

function Assert-NoCaptureFiles {
    param([Parameter(Mandatory)][string]$Captures, [Parameter(Mandatory)][string]$AutomaticCopies)
    Assert-E2E (@(Get-ChildItem -LiteralPath $Captures -Force).Count -eq 0) 'Grab Text must not save a capture'
    Assert-E2E (@(Get-ChildItem -LiteralPath $AutomaticCopies -Force).Count -eq 0) 'Grab Text must not create an automatic copy'
}

function Get-EditedImageFrame {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver)
    $Driver.Capture(100, 130, 400, 226)
}

function Get-CaptureWindowInfo {
    param([Parameter(Mandatory)][System.Collections.IDictionary]$Environment)
    $tree = & xwininfo -display $Environment['DISPLAY'] -root -tree
    foreach ($line in $tree) {
        if ($line -notmatch '^\s*(0x[0-9a-fA-F]+)\s+\(has no name\):\s+\("jiffshot" "jiffshot"\)\s+(\d+)x(\d+)' -or
            [int]$Matches[2] -lt 600 -or [int]$Matches[3] -lt 350) {
            continue
        }
        $id = $Matches[1]
        $info = (& xwininfo -display $Environment['DISPLAY'] -id $id) -join "`n"
        $x = [regex]::Match($info, 'Absolute upper-left X:\s*(-?\d+)')
        $y = [regex]::Match($info, 'Absolute upper-left Y:\s*(-?\d+)')
        if ($info.Contains('Map State: IsViewable') -and $x.Success -and $y.Success) {
            return [pscustomobject]@{
                Window = [Convert]::ToInt64($id.Substring(2), 16)
                X = [int]$x.Groups[1].Value
                Y = [int]$y.Groups[1].Value
            }
        }
    }
    $null
}

function Wait-SharedStatus {
    param(
        [Parameter(Mandatory)][JiffshotE2E.TextReader]$Reader,
        [Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver,
        [Parameter(Mandatory)][string]$Expected
    )
    $seen = [ref]''
    try {
        Wait-E2EUntil {
            $seen.Value = $Reader.Recognize($Driver.Capture(76, 390, 168, 35), 7, $false).Trim()
            $seen.Value.ToLowerInvariant().Contains($Expected.ToLowerInvariant())
        } "shared editor status containing '$Expected'" 25
    }
    catch [System.TimeoutException] {
        throw [System.TimeoutException]::new("$($_.Exception.Message); last visible text: '$($seen.Value)'")
    }
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
if (-not $OcrFixture) { $OcrFixture = Join-Path $PSScriptRoot '../../docs/images/editor.png' }
$fixture = Resolve-E2EPath $OcrFixture
$app = $server = $driver = $captureDriver = $portal = $reader = $null
$work = New-E2EWorkDirectory 'jiffshot-shared-grab-text-ui-'
try {
    $portal = New-MockPortal
    $portal.ScreenshotMode = 'hold'
    New-E2EPrivateDirectory (Join-Path $work 'runtime') | Out-Null
    $config = (New-Item -ItemType Directory -Force -Path (Join-Path $work 'config/Jiffshot')).FullName
    $captureConfig = (New-Item -ItemType Directory -Force -Path (Join-Path $work 'config/jiffshot')).FullName
    $captures = (New-Item -ItemType Directory -Path (Join-Path $work 'captures')).FullName
    $automaticCopies = (New-Item -ItemType Directory -Path (Join-Path $work 'automatic-copies')).FullName
    Write-E2EJson (Join-Path $config 'settings.json') ([ordered]@{
            theme                         = 'Light'
            capture_directory             = $captures
            linux_auto_copy_directory     = $automaticCopies
            copy_to_clipboard             = $true
            linux_global_shortcuts_enabled = $true
            linux_global_preset_shortcuts = $true
            linux_screen_capture_presets  = @(@{
                    name = 'Read text'; delay_seconds = 0; include_cursor = $false; mode = 'text'
                })
        })
    Write-E2EJson (Join-Path $config 'jiffshot.json') ([ordered]@{ theme = 'Light'; library = $captures })
    Write-E2EJson (Join-Path $captureConfig 'capture-window.json') ([ordered]@{ tab = 1; area = 'text' })
    $emptyImage = Join-Path $work 'empty.png'
    [System.IO.File]::WriteAllBytes($emptyImage, (Get-E2ESolidPng 255 255 255))
    $documentPath = Join-Path $work 'existing.png'
    [System.IO.File]::WriteAllBytes($documentPath, (Get-E2ESolidPng 226 240 255 1280 720))
    $documentHash = (Get-FileHash -LiteralPath $documentPath -Algorithm SHA256).Hash
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $displayName = $server.Display
        $environment = New-E2EEnvironment $displayName $work $runtime
        $environment.Remove('JIFFSHOT_CLASSIC_UI')
        $environment['JIFFSHOT_LIBRARY'] = $captures
        $environment['JIFFSHOT_CONFIG_DIR'] = $config
        $environment['JIFFSHOT_LOCAL_DIR'] = Join-Path $work 'local'

        Set-ClipboardText $environment 'before-command-line-text'
        $screenshotCount = $portal.ScreenshotRequests.Count
        $screenCastCount = $portal.CaptureRequests
        $app = Start-E2EProcess @($binaryPath, '--grab-text') $environment $work $appLog
        $requestPath = Wait-ScreenshotChooser $portal $screenshotCount $screenCastCount
        Send-PortalResponse $portal $requestPath 0 @{ uri = (New-DBusVariant 's' (ConvertTo-E2EFileUri $fixture)) }
        Wait-E2EUntil { (Get-ClipboardText $environment).ToLowerInvariant().Contains('capture') } 'command-line recognized text' 25
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment -Title 'Untitled — Jiffshot Editor') } 'the unchanged command-line editor'
        Assert-NoCaptureFiles $captures $automaticCopies
        Stop-E2EProcess $app
        $app = $null

        $bindingCount = $portal.Bindings.Count
        $app = Start-E2EProcess @($binaryPath, $documentPath) $environment $work $appLog
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment -Title 'existing.png — Jiffshot Editor') } 'the shared editor' 20
        $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment -Title 'existing.png — Jiffshot Editor')
        $reader = New-E2ETextReader
        Wait-E2EUntil { $portal.Bindings.Count -gt $bindingCount } 'the text preset shortcut'
        $shortcutSession, $boundShortcuts = $portal.Bindings[-1]
        Assert-E2E (@($boundShortcuts | Where-Object { $_[0] -ceq 'preset-1' }).Count -eq 1) 'the text preset shortcut was not bound'
        Wait-E2ESeconds 0.5
        $baseline = (Get-EditedImageFrame $driver).CountRed()
        $driver.Move(250, 300)
        $driver.Button(1, $true)
        $driver.Move(520, 400)
        $driver.Button(1, $false)
        $driver.Sync()
        Wait-E2EUntil { (Get-EditedImageFrame $driver).CountRed() -gt $baseline + 40 } 'an unsaved editor annotation'
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment -Title '• existing.png — Jiffshot Editor') } 'the modified document title'
        $beforePixels = (Get-EditedImageFrame $driver).CountRed()
        $beforeFrame = Get-EditedImageFrame $driver
        Write-E2ETrace $driver 'before' 'shared-grab-text'

        foreach ($mode in @('text', 'empty', 'cancel', 'invalid')) {
            $sentinel = "preset-$mode-clipboard"
            Set-ClipboardText $environment $sentinel
            $screenshotCount = $portal.ScreenshotRequests.Count
            $screenCastCount = $portal.CaptureRequests
            Send-PortalActivated $portal $shortcutSession 'preset-1'
            $requestPath = Wait-ScreenshotChooser $portal $screenshotCount $screenCastCount
            if ($mode -eq 'cancel') {
                Send-PortalResponse $portal $requestPath 1 @{}
            }
            else {
                $uri = switch ($mode) {
                    'text' { ConvertTo-E2EFileUri $fixture }
                    'empty' { ConvertTo-E2EFileUri $emptyImage }
                    default { 'https://example.invalid/screenshot.png' }
                }
                Send-PortalResponse $portal $requestPath 0 @{ uri = (New-DBusVariant 's' $uri) }
            }
            Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment -Title '• existing.png — Jiffshot Editor') } "the editor after $mode"
            if ($mode -eq 'text') {
                Wait-E2EUntil { (Get-ClipboardText $environment).ToLowerInvariant().Contains('capture') } 'preset recognized text' 25
                Wait-SharedStatus $reader $driver 'Copied'
            }
            else {
                $expectedStatus = switch ($mode) {
                    'empty' { 'No text recognized' }
                    'cancel' { 'Text capture' }
                    'invalid' { 'Could not grab' }
                }
                Wait-SharedStatus $reader $driver $expectedStatus
                Assert-E2E ((Get-ClipboardText $environment) -ceq $sentinel) "$mode must leave the clipboard unchanged"
            }
            Write-E2ETrace $driver "after-$mode" 'shared-grab-text'
            Assert-E2E ((Get-EditedImageFrame $driver).CountRed() -ge $beforePixels * 0.8) "$mode must preserve the annotation"
            Assert-E2E ((Get-EditedImageFrame $driver).SameAs($beforeFrame)) "$mode must preserve the edited canvas"
            Assert-E2E ((Get-FileHash -LiteralPath $documentPath -Algorithm SHA256).Hash -ceq $documentHash) "$mode must not modify the source image"
            Assert-NoCaptureFiles $captures $automaticCopies
        }

        Send-PortalActivated $portal $shortcutSession 'capture'
        Wait-E2EUntil { $null -ne (Get-CaptureWindowInfo $environment) } 'the Capture window'
        $captureDriver = New-E2EX11Driver $displayName (Get-CaptureWindowInfo $environment)
        Set-ClipboardText $environment 'before-capture-window-text'
        $screenshotCount = $portal.ScreenshotRequests.Count
        $screenCastCount = $portal.CaptureRequests
        $captureDriver.Press('Return')
        $requestPath = Wait-ScreenshotChooser $portal $screenshotCount $screenCastCount
        Send-PortalResponse $portal $requestPath 0 @{ uri = (New-DBusVariant 's' (ConvertTo-E2EFileUri $fixture)) }
        Wait-E2EUntil { (Get-ClipboardText $environment).ToLowerInvariant().Contains('capture') } 'Capture-window recognized text' 25
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment -Title '• existing.png — Jiffshot Editor') } 'the editor after Capture-window text'
        $captureDriver.Press('Escape')
        Wait-E2EUntil { $null -eq (Get-CaptureWindowInfo $environment) } 'the Capture window to close'
        Wait-E2EUntil { (Get-EditedImageFrame $driver).SameAs($beforeFrame) } 'the edited canvas after the Capture window closes'
        Assert-NoCaptureFiles $captures $automaticCopies
        Close-E2EX11Driver $captureDriver
        $captureDriver = $null
        Close-E2EX11Driver $driver
        $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment -Title '• existing.png — Jiffshot Editor')
        Invoke-E2EHotkey $driver 'z'
        Wait-E2EUntil { (Get-EditedImageFrame $driver).CountRed() -lt $beforePixels * 0.2 } 'Undo to remove the annotation after Grab Text'
        Invoke-E2EHotkey $driver 'y'
        Wait-E2EUntil { (Get-EditedImageFrame $driver).CountRed() -ge $beforePixels * 0.8 } 'Redo to restore the annotation after Grab Text'
        Assert-NoCaptureFiles $captures $automaticCopies
        Write-E2EPass 'shared_grab_text_preserves_document_history_and_clipboard'

        Close-E2EX11Driver $driver
        $driver = $null
        Stop-E2EProcess $app
        $app = $null
        $screenshot = Join-Path $work 'ordinary-screenshot.png'
        [System.IO.File]::WriteAllBytes($screenshot, (Get-E2ESolidPng 255 0 0))
        foreach ($mode in @('success', 'cancel')) {
            $beforeCaptures = @(Get-ChildItem -LiteralPath $captures -File).Count
            $beforeCopies = @(Get-ChildItem -LiteralPath $automaticCopies -File).Count
            $screenshotCount = $portal.ScreenshotRequests.Count
            $screenCastCount = $portal.CaptureRequests
            $app = Start-E2EProcess @($binaryPath, '--capture-interactive') $environment $work $appLog
            $requestPath = Wait-ScreenshotChooser $portal $screenshotCount $screenCastCount
            if ($mode -eq 'success') {
                Send-PortalResponse $portal $requestPath 0 @{ uri = (New-DBusVariant 's' (ConvertTo-E2EFileUri $screenshot)) }
                $pngEnd = [byte[]](0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82)
                Wait-E2EUntil {
                    $saved = @(Get-ChildItem -LiteralPath $captures -File)
                    $saved.Count -eq $beforeCaptures + 1 -and (Test-E2EFileEnd $saved[-1].FullName $pngEnd)
                } 'the saved ordinary screenshot'
                $saved = @(Get-ChildItem -LiteralPath $captures -File)[-1].FullName
                Assert-E2E (((Read-E2EPng $saved).Pixel(120, 70) -join ',') -ceq '255,0,0,255') 'the saved screenshot must contain the selected pixels'
                Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment -Title "$([System.IO.Path]::GetFileName($saved)) — Jiffshot Editor") } 'the saved screenshot in the editor'
                $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment -Title "$([System.IO.Path]::GetFileName($saved)) — Jiffshot Editor")
                Wait-E2EUntil { (Get-EditedImageFrame $driver).CountRed() -gt 5000 } 'the ordinary screenshot pixels in the editor'
            }
            else {
                Send-PortalResponse $portal $requestPath 1 @{}
                Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment -Title 'Untitled — Jiffshot Editor') } 'the editor after a cancelled screenshot'
                $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment -Title 'Untitled — Jiffshot Editor')
                Wait-SharedStatus $reader $driver 'Capture cancelled'
                Assert-E2E (@(Get-ChildItem -LiteralPath $captures -File).Count -eq $beforeCaptures) 'cancelling a screenshot must not save a capture'
                Assert-E2E (@(Get-ChildItem -LiteralPath $automaticCopies -File).Count -eq $beforeCopies) 'cancelling a screenshot must not create an automatic copy'
            }
            Close-E2EX11Driver $driver
            $driver = $null
            Stop-E2EProcess $app
            $app = $null
        }
        Write-E2EPass 'shared_screenshot_success_and_cancellation'
    }
    catch {
        if ($null -ne $driver) { Write-E2ETrace $driver 'shared-grab-text-failure' }
        if ($env:JIFFSHOT_GUI_TRACE -and $null -ne $server) {
            & xwininfo -display $server.Display -root -tree
        }
        Write-E2ELogTail @($appLog, $xvfbLog)
        throw
    }
    finally {
        if ($null -ne $reader) { $reader.Dispose() }
        Close-E2EX11Driver $captureDriver
        Close-E2EX11Driver $driver
        Stop-E2EProcess $app
        if ($null -ne $server) { Stop-E2EProcess $server.Process }
    }
}
finally {
    Unregister-E2EService $portal
    Remove-E2EWorkDirectory $work
}
