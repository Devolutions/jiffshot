#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify screen text capture through a mock Screenshot portal and real OCR.
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
    if ($result.ExitCode -ne 0) {
        return ''
    }
    [System.Text.Encoding]::UTF8.GetString($result.Output)
}

function Set-ClipboardText {
    param([Parameter(Mandatory)][System.Collections.IDictionary]$Environment, [Parameter(Mandatory)][string]$Text)
    $null = Invoke-E2ECommand @('xclip', '-selection', 'clipboard') -Environment $Environment -InputBytes ([System.Text.Encoding]::UTF8.GetBytes($Text)) -Check
}

function Invoke-DrawArrow {
    param([JiffshotE2E.X11Driver]$Driver)
    $Driver.Move(250, 350)
    $Driver.Button(1, $true)
    $Driver.Move(520, 430)
    $Driver.Button(1, $false)
    $Driver.Sync()
}

function Test-GrabTextNeverCreatesAutomaticCopy {
    param([Parameter(Mandatory)][string]$Destination)
    Assert-E2E (@(Get-ChildItem -LiteralPath $Destination -Force).Count -eq 0) 'screen text recognition must not create an automatic capture copy'
}

function Assert-TrueVariant {
    param([Parameter(Mandatory)]$Options, [Parameter(Mandatory)][string]$Name)
    $value = $Options[$Name].Value
    Assert-E2E ($value -is [bool] -and $value) "$Name must be True"
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
if (-not $OcrFixture) {
    $OcrFixture = Join-Path $PSScriptRoot '../../docs/images/editor.png'
}
$fixture = Resolve-E2EPath $OcrFixture
$app = $server = $driver = $text = $portal = $null
$work = New-E2EWorkDirectory 'jiffshot-grab-text-ui-'
try {
    $portal = New-MockPortal
    $portal.ScreenshotMode = 'hold'
    New-E2EPrivateDirectory (Join-Path $work 'runtime') | Out-Null
    $config = Join-Path $work 'config/Jiffshot'
    New-Item -ItemType Directory -Path $config | Out-Null
    $captures = Join-Path $work 'captures'
    New-Item -ItemType Directory -Path $captures | Out-Null
    $automaticCopies = Join-Path $work 'automatic-copies'
    New-Item -ItemType Directory -Path $automaticCopies | Out-Null
    Write-E2EJson (Join-Path $config 'settings.json') ([ordered]@{
            theme                     = 'Light'
            capture_directory         = $captures
            linux_auto_copy_directory = $automaticCopies
        })
    $emptyImage = Join-Path $work 'empty.png'
    [System.IO.File]::WriteAllBytes($emptyImage, (Get-E2ESolidPng 255 255 255))
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $displayName = $server.Display
        $environment = New-E2EEnvironment $displayName $work $runtime
        foreach ($mode in @('text', 'empty', 'cancel', 'invalid')) {
            $sentinel = "clipboard-$mode-unchanged"
            Set-ClipboardText $environment $sentinel
            $expectedRequests = $portal.ScreenshotRequests.Count + 1
            $app = Start-E2EProcess @($binaryPath, '--grab-text') $environment $work $appLog
            Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } "the editor for $mode" 12
            $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
            Wait-E2EUntil { $portal.ScreenshotRequests.Count -eq $expectedRequests } "the screenshot request for $mode"
            # A script-scope $request would overwrite the module's exported $PortalRequestInterface interface name.
            $requestPath, $options = $portal.ScreenshotRequests[$portal.ScreenshotRequests.Count - 1]
            Assert-TrueVariant $options 'interactive'
            Assert-TrueVariant $options 'modal'
            Wait-E2ESeconds 0.5
            if ($mode -eq 'text') {
                Invoke-DrawArrow $driver
                Wait-E2EUntil { $driver.RedCanvasPixels() -gt 40 } 'an editor annotation'
                $beforePixels = $driver.RedCanvasPixels()
            }
            $beforeStatus = Get-E2EStatusFrame $driver
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
            if ($mode -eq 'text') {
                Wait-E2EUntil {
                    (Get-ClipboardText $environment).ToLowerInvariant().Contains('capture')
                } 'recognized screenshot text in the clipboard' 25
                $afterPixels = $driver.RedCanvasPixels()
                Assert-E2E ($beforePixels * 0.8 -le $afterPixels -and $afterPixels -le $beforePixels * 1.5 + 50) "screen OCR must not replace the edited document ($beforePixels, $afterPixels)"
            }
            else {
                Wait-E2EUntil { -not (Get-E2EStatusFrame $driver).SameAs($beforeStatus) } "the $mode result"
                Assert-E2E ((Get-ClipboardText $environment) -ceq $sentinel) "$mode must leave the clipboard unchanged"
            }
            Assert-E2E (@(Get-ChildItem -LiteralPath $captures -Force).Count -eq 0) 'grabbing text must not save a capture'
            Test-GrabTextNeverCreatesAutomaticCopy $automaticCopies
            Assert-E2E ($portal.CaptureRequests -eq 0) 'grabbing text must use Screenshot, not ScreenCast'
            Close-E2EX11Driver $driver
            $driver = $null
            Stop-E2EProcess $app
            $app = $null
        }
        $app = Start-E2EProcess @($binaryPath) $environment $work $appLog
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'the editor for the Grab text control' 12
        $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
        Wait-E2ESeconds 0.5
        $text = New-E2ETextReader
        $position = Find-E2EScrollingLabel $text $driver 'Grab text'
        Write-E2ETrace $driver 'grab-text-control'
        Set-ClipboardText $environment 'from-gui-before'
        $expectedRequests = $portal.ScreenshotRequests.Count + 1
        $driver.Click($position[0], $position[1])
        Wait-E2EUntil { $portal.ScreenshotRequests.Count -eq $expectedRequests } "the Grab text control's screenshot chooser"
        $requestPath, $options = $portal.ScreenshotRequests[$portal.ScreenshotRequests.Count - 1]
        Assert-TrueVariant $options 'interactive'
        Send-PortalResponse $portal $requestPath 0 @{ uri = (New-DBusVariant 's' (ConvertTo-E2EFileUri $fixture)) }
        Wait-E2EUntil {
            (Get-ClipboardText $environment).ToLowerInvariant().Contains('capture')
        } 'recognized text from the Grab text control' 25
        Assert-E2E (@(Get-ChildItem -LiteralPath $captures -Force).Count -eq 0) 'grabbing text must not save a capture'
        Test-GrabTextNeverCreatesAutomaticCopy $automaticCopies
        Write-E2EPass 'test_grab_text_never_creates_automatic_copy'
        Write-E2ELine 'Screen text OCR copied selected text from CLI and GUI, preserved the editor, and left the clipboard unchanged for empty, cancelled and invalid selections'
    }
    catch {
        if ($null -ne $driver) {
            Write-E2ETrace $driver 'grab-text-failure'
        }
        Write-E2ELogTail @($appLog, $xvfbLog)
        throw
    }
    finally {
        Close-E2EX11Driver $driver
        if ($null -ne $text) { $text.Dispose() }
        Stop-E2EProcess $app
        if ($null -ne $server) { Stop-E2EProcess $server.Process }
    }
}
finally {
    Unregister-E2EService $portal
    Remove-E2EWorkDirectory $work
}
