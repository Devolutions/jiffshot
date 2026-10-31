#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Exercise Linux interactive effects in a private Xvfb with a mock portal.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.Editor.psm1') -Force

$PngEndMarker = [byte[]](0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82)

function Format-Pixel {
    param([Parameter(Mandatory)][int[]]$Pixel)
    "($($Pixel -join ', '))"
}

function Test-Pixel {
    <# Returns whether Image.Pixel(X, Y) equals Expected (r, g, b, a). #>
    param([Parameter(Mandatory)]$Image, [int]$X, [int]$Y, [Parameter(Mandatory)][int[]]$Expected)
    ($Image.Pixel($X, $Y) -join ',') -eq ($Expected -join ',')
}

function Test-SamePixel {
    param([Parameter(Mandatory)]$Image, [Parameter(Mandatory)]$Other, [int]$X, [int]$Y)
    Test-Pixel $Image $X $Y $Other.Pixel($X, $Y)
}

function Get-AssertionText {
    <# Returns an AssertionError message without its prefix, or $null for any other error. #>
    param([Parameter(Mandatory)][System.Management.Automation.ErrorRecord]$ErrorRecord)
    $message = $ErrorRecord.Exception.Message
    if ($message.StartsWith('AssertionError: ', [StringComparison]::Ordinal)) {
        return $message.Substring('AssertionError: '.Length)
    }
    $null
}

function Show-Effects {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver)
    for ($index = 0; $index -lt 2; $index++) {
        $Driver.Wheel(750, 150, 5, 3)
        Wait-E2ESeconds 0.15
    }
    $before = $Driver.Capture(0, 115, 800, 35)
    $Driver.Click(178, 96)
    Wait-E2EUntil { -not $Driver.Capture(0, 115, 800, 35).SameAs($before) } 'the editor properties and effects panel'
}

function Select-Effect {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [int]$PreviousFromEnd)
    $Driver.Click(525, 130)
    $Driver.Press('End')
    for ($index = 0; $index -lt $PreviousFromEnd; $index++) {
        $Driver.Press('Up')
    }
    $Driver.Press('Return')
    Wait-E2ESeconds 0.1
}

function Select-FirstEffect {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver)
    $Driver.Click(525, 130)
    $Driver.Press('Home')
    $Driver.Press('Return')
    Wait-E2ESeconds 0.1
}

function Set-EffectParameter {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [AllowEmptyString()][string]$Value)
    $Driver.Click(260, 160)
    $Driver.SelectAll()
    $Driver.Press('BackSpace')
    foreach ($character in $Value.ToCharArray()) {
        $Driver.Press([string]$character)
    }
}

function Export-EffectImage {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [Parameter(Mandatory)]$Portal, [Parameter(Mandatory)][string]$Work)
    $destination = Join-Path $Work "effect-export-$($Portal.SaveRequests.Count).png"
    $Portal.SaveDestinations.Add($destination)
    $previous = $Portal.SaveRequests.Count
    $previousStatus = Get-E2EStatusFrame $Driver
    Invoke-E2EHotkey $Driver 'e'
    Wait-E2EUntil { $Portal.SaveRequests.Count -gt $previous } 'the export dialog'
    Assert-E2E ((ConvertFrom-E2EWindowHandle $Portal.SaveParents[$previous]) -eq $Driver.Window) 'the export dialog must be parented to the editor'
    Wait-E2EUntil { Test-E2EFileEnd $destination $PngEndMarker } 'the completed PNG export'
    Wait-E2EUntil { -not (Get-E2EStatusFrame $Driver).SameAs($previousStatus) } 'the export status'
    Read-E2EPng $destination
}

function Invoke-ApplyEffect {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [int]$PreviousFromEnd, [Parameter(Mandatory)][string]$Description)
    Select-Effect $Driver $PreviousFromEnd
    $before = Get-E2ECanvasFrame $Driver
    $Driver.Click(670, 130)
    Wait-E2EUntil { -not (Get-E2ECanvasFrame $Driver).SameAs($before) } $Description
}

function Undo-Effect {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver)
    $before = Get-E2ECanvasFrame $Driver
    Invoke-E2EHotkey $Driver 'z'
    Wait-E2EUntil { -not (Get-E2ECanvasFrame $Driver).SameAs($before) } 'undoing the editor change'
}

function Assert-ExportMatches {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [Parameter(Mandatory)]$Portal, [Parameter(Mandatory)][string]$Work, [Parameter(Mandatory)]$Expected, [string]$Message = 'the export must match the expected document')
    Assert-E2E (Export-EffectImage $Driver $Portal $Work).SameAs($Expected) $Message
}

function Start-Watermark {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [Parameter(Mandatory)]$Portal, [AllowNull()][string]$Parameter = $null)
    Select-Effect $Driver 1
    if ($PSBoundParameters.ContainsKey('Parameter') -and $null -ne $Parameter) {
        Set-EffectParameter $Driver $Parameter
    }
    $previous = $Portal.OpenRequests.Count
    $beforeStatus = Get-E2EStatusFrame $Driver
    $Driver.Click(670, 130)
    Wait-E2EUntil { $Portal.OpenRequests.Count -gt $previous } 'the watermark picker'
    Wait-E2EUntil { -not (Get-E2EStatusFrame $Driver).SameAs($beforeStatus) } 'the watermark selection prompt'
    $title, $options, $null = $Portal.OpenRequests[$previous]
    Assert-E2E ($title -ceq 'Choose a watermark image') $title
    Assert-E2E ((ConvertFrom-E2EWindowHandle $Portal.OpenParents[$previous]) -eq $Driver.Window) 'the watermark dialog must be parented to the editor'
    Assert-E2E $options.ContainsKey('filters') 'the watermark picker must offer image filters'
    $patterns = @(foreach ($filter in $options['filters'].Value) {
            foreach ($entry in $filter[1]) { $entry[1] }
        })
    foreach ($pattern in @('*.png', '*.jpg', '*.jpeg', '*.bmp', '*.gif', '*.webp')) {
        Assert-E2E ($patterns -ccontains $pattern) "$($patterns -join ', ')"
    }
    $previous
}

function Add-Annotation {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver)
    Invoke-E2EHotkey $Driver '2'
    $Driver.Move(310, 340)
    $Driver.Button(1, $true)
    $Driver.Move(480, 420)
    $Driver.Button(1, $false)
    $Driver.Sync()
    Wait-E2EUntil { $Driver.RedCanvasPixels() -gt 40 } 'a newer red annotation'
}

function Open-FifoWriter {
    <# Opens a FIFO writer once the app's background reader has opened the FIFO. #>
    param([Parameter(Mandatory)][string]$Path)
    $opened = @{ Fd = -1 }
    Wait-E2EUntil {
        $opened.Fd = [JiffshotE2E.Posix]::TryOpenFifoWriter($Path)
        $opened.Fd -ge 0
    } 'the background watermark reader'
    $opened.Fd
}

function Send-FifoPayload {
    param([Parameter(Mandatory)][int]$Fd, [Parameter(Mandatory)][byte[]]$Payload)
    try {
        [JiffshotE2E.Posix]::WriteAll($Fd, $Payload)
    }
    finally {
        [JiffshotE2E.Posix]::Close($Fd)
    }
}

function Set-SelectionToolStatus {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver)
    $previous = Get-E2EStatusFrame $Driver
    Invoke-E2EHotkey $Driver '1'
    Wait-E2EUntil { -not (Get-E2EStatusFrame $Driver).SameAs($previous) } 'the selection tool status'
    Get-E2EStatusFrame $Driver
}

function test_selector_capture_info_adds_band_and_undoes {
    param($Driver, $Portal, $Work, $Baseline)
    Invoke-ApplyEffect $Driver 2 'the Capture info band'
    $result = Export-EffectImage $Driver $Portal $Work
    Assert-E2E ($result.Width -eq 240 -and $result.Height -gt 140) "($($result.Width), $($result.Height))"
    Assert-E2E (Test-Pixel $result 0 0 @(30, 120, 190, 255)) (Format-Pixel $result.Pixel(0, 0))
    Assert-E2E (Test-Pixel $result 0 140 @(34, 38, 46, 255)) (Format-Pixel $result.Pixel(0, 140))
    $rgba = $result.Rgba
    $hasText = $false
    for ($offset = 140 * $result.Width * 4; $offset -lt $rgba.Length; $offset += 4) {
        if ($rgba[$offset] -ne 34 -or $rgba[$offset + 1] -ne 38 -or $rgba[$offset + 2] -ne 46 -or $rgba[$offset + 3] -ne 255) {
            $hasText = $true
            break
        }
    }
    Assert-E2E $hasText 'the capture information band must contain visible text'
    Undo-Effect $Driver
    Assert-ExportMatches $Driver $Portal $Work $Baseline
}

function Test-InvalidCaptureSidecar {
    param($Driver, $Portal, $Work, $Source, $Baseline, [byte[]]$Contents)
    $sidecar = [System.IO.Path]::ChangeExtension($Source, '.json')
    $backup = Join-Path $Work 'capture-sidecar-backup.json'
    [System.IO.File]::Move($sidecar, $backup)
    [JiffshotE2E.Posix]::MakeFifo($sidecar)
    $fd = -1
    try {
        Select-Effect $Driver 2
        $Driver.Click(670, 130)
        $fd = Open-FifoWriter $sidecar
        $beforeStatus = Set-SelectionToolStatus $Driver
        $writer = $fd
        $fd = -1
        Send-FifoPayload $writer $Contents
        Wait-E2EUntil { -not (Get-E2EStatusFrame $Driver).SameAs($beforeStatus) } 'the capture metadata error'
        Assert-ExportMatches $Driver $Portal $Work $Baseline 'invalid capture metadata must not change the document'
    }
    finally {
        if ($fd -ge 0) {
            [JiffshotE2E.Posix]::Close($fd)
        }
        [System.IO.File]::Delete($sidecar)
        [System.IO.File]::Move($backup, $sidecar)
    }
}

function test_selector_capture_info_rejects_malformed_sidecar {
    param($Driver, $Portal, $Work, $Source, $Baseline)
    Test-InvalidCaptureSidecar $Driver $Portal $Work $Source $Baseline ([System.Text.Encoding]::UTF8.GetBytes('{"window_title":"unfinished"'))
}

function test_selector_capture_info_rejects_oversized_sidecar {
    param($Driver, $Portal, $Work, $Source, $Baseline)
    # Matches Python json.dumps({"window_title": "A" * 1015}).
    $contents = [System.Text.Encoding]::UTF8.GetBytes('{"window_title": "' + ('A' * 1015) + '"}')
    Test-InvalidCaptureSidecar $Driver $Portal $Work $Source $Baseline $contents
}

function test_selector_spotlight_magnifies_image_center_and_undoes {
    param($Driver, $Portal, $Work, $Baseline)
    Invoke-ApplyEffect $Driver 0 'the centered Spotlight and magnify effect'
    $result = Export-EffectImage $Driver $Portal $Work
    Assert-E2E ($result.Width -eq 240 -and $result.Height -eq 140) "($($result.Width), $($result.Height))"
    Assert-E2E (-not (Test-SamePixel $result $Baseline 0 0)) 'the outside must be dimmed'
    Assert-E2E (Test-SamePixel $result $Baseline 120 70) 'the center must remain visible'
    Undo-Effect $Driver
    Assert-ExportMatches $Driver $Portal $Work $Baseline
}

function test_custom_border_parameter_is_applied_and_undoable {
    param($Driver, $Portal, $Work, $Baseline)
    Select-FirstEffect $Driver
    Set-EffectParameter $Driver '3'
    $before = Get-E2ECanvasFrame $Driver
    $Driver.Click(670, 130)
    Wait-E2EUntil { -not (Get-E2ECanvasFrame $Driver).SameAs($before) } 'the configured border'
    $result = Export-EffectImage $Driver $Portal $Work
    Assert-E2E ($result.Width -eq 246 -and $result.Height -eq 146) "($($result.Width), $($result.Height))"
    Assert-E2E (Test-Pixel $result 3 3 $Baseline.Pixel(0, 0)) (Format-Pixel $result.Pixel(3, 3))
    Assert-E2E (-not (Test-SamePixel $result $Baseline 0 0)) (Format-Pixel $result.Pixel(0, 0))
    Undo-Effect $Driver
    Assert-ExportMatches $Driver $Portal $Work $Baseline
}

function test_invalid_parameter_preserves_document_and_switching_effect_resets_it {
    param($Driver, $Portal, $Work, $Baseline)
    Select-FirstEffect $Driver
    Set-EffectParameter $Driver '999'
    $beforeStatus = Get-E2EStatusFrame $Driver
    $Driver.Click(670, 130)
    Wait-E2EUntil { -not (Get-E2EStatusFrame $Driver).SameAs($beforeStatus) } 'the invalid parameter error'
    Assert-ExportMatches $Driver $Portal $Work $Baseline 'invalid effect parameters must not alter the document'
    Set-EffectParameter $Driver '20'
    Select-Effect $Driver 2
    Select-FirstEffect $Driver
    $before = Get-E2ECanvasFrame $Driver
    $Driver.Click(670, 130)
    Wait-E2EUntil { -not (Get-E2ECanvasFrame $Driver).SameAs($before) } 'the default border after switching'
    $result = Export-EffectImage $Driver $Portal $Work
    Assert-E2E ($result.Width -eq 256 -and $result.Height -eq 156) "switching effects must clear the old parameter and restore the default ($($result.Width), $($result.Height))"
    Undo-Effect $Driver
    Assert-ExportMatches $Driver $Portal $Work $Baseline
}

function test_custom_spotlight_magnification_is_applied_and_undoable {
    param($Driver, $Portal, $Work, $Baseline)
    Select-Effect $Driver 0
    Set-EffectParameter $Driver '2'
    $before = Get-E2ECanvasFrame $Driver
    $Driver.Click(670, 130)
    Wait-E2EUntil { -not (Get-E2ECanvasFrame $Driver).SameAs($before) } 'the custom spotlight'
    $result = Export-EffectImage $Driver $Portal $Work
    Assert-E2E (Test-SamePixel $result $Baseline 45 70) '2x magnification must leave the wider spotlight area undimmed'
    Assert-E2E (-not (Test-SamePixel $result $Baseline 20 70)) 'the area outside the spotlight must still be dimmed'
    Undo-Effect $Driver
    Assert-ExportMatches $Driver $Portal $Work $Baseline
}

function test_watermark_picker_blends_local_image_and_undoes {
    param($Driver, $Portal, $Work, $Baseline, $Mark)
    $index = Start-Watermark $Driver $Portal
    $before = Get-E2ECanvasFrame $Driver
    Complete-EffectsPortalOpen $Portal $index 0 -Uri (ConvertTo-E2EFileUri $Mark)
    Wait-E2EUntil { -not (Get-E2ECanvasFrame $Driver).SameAs($before) } 'the decoded watermark'
    $result = Export-EffectImage $Driver $Portal $Work
    Assert-E2E ($result.Width -eq 240 -and $result.Height -eq 140) "($($result.Width), $($result.Height))"
    Assert-E2E (Test-Pixel $result 200 115 @(109, 78, 124, 255)) (Format-Pixel $result.Pixel(200, 115))
    Assert-E2E (Test-Pixel $result 20 20 @(30, 120, 190, 255)) (Format-Pixel $result.Pixel(20, 20))
    Undo-Effect $Driver
    Assert-ExportMatches $Driver $Portal $Work $Baseline
}

function test_watermark_parameter_is_snapshotted_before_the_picker_and_undoes {
    param($Driver, $Portal, $Work, $Baseline, $Mark)
    $index = Start-Watermark $Driver $Portal '80'
    Set-EffectParameter $Driver '20'
    $before = Get-E2ECanvasFrame $Driver
    Complete-EffectsPortalOpen $Portal $index 0 -Uri (ConvertTo-E2EFileUri $Mark)
    Wait-E2EUntil { -not (Get-E2ECanvasFrame $Driver).SameAs($before) } 'the custom-opacity watermark'
    $result = Export-EffectImage $Driver $Portal $Work
    Assert-E2E (Test-Pixel $result 200 115 @(210, 24, 38, 255)) (Format-Pixel $result.Pixel(200, 115))
    Assert-E2E (Test-SamePixel $result $Baseline 20 20) (Format-Pixel $result.Pixel(20, 20))
    $Driver.Click(25, 300)
    Undo-Effect $Driver
    Assert-ExportMatches $Driver $Portal $Work $Baseline
}

function test_invalid_watermark_parameter_does_not_open_picker {
    param($Driver, $Portal, $Work, $Baseline)
    Select-Effect $Driver 1
    Set-EffectParameter $Driver '101'
    $previousRequests = $Portal.OpenRequests.Count
    $beforeStatus = Get-E2EStatusFrame $Driver
    $Driver.Click(670, 130)
    Wait-E2EUntil { -not (Get-E2EStatusFrame $Driver).SameAs($beforeStatus) } 'the invalid watermark opacity'
    Wait-E2ESeconds 0.3
    Assert-E2E ($Portal.OpenRequests.Count -eq $previousRequests) 'invalid opacity must be rejected before opening a file chooser'
    Assert-ExportMatches $Driver $Portal $Work $Baseline
    Set-EffectParameter $Driver ''
}

function Test-PickerNonSuccess {
    param($Driver, $Portal, $Work, $Baseline, [uint32]$Code, [string]$Description, [switch]$OmitUris)
    $index = Start-Watermark $Driver $Portal
    $beforeStatus = Get-E2EStatusFrame $Driver
    Complete-EffectsPortalOpen $Portal $index $Code -OmitUris:$OmitUris
    Wait-E2EUntil { -not (Get-E2EStatusFrame $Driver).SameAs($beforeStatus) } "the $Description picker result"
    Write-E2ETrace $Driver "interactive-effects-picker-$Code"
    try {
        $result = Export-EffectImage $Driver $Portal $Work
    }
    catch {
        $text = Get-AssertionText $_
        if ($null -eq $text) { throw }
        throw [System.Exception]::new("AssertionError: export must remain available after the $Description picker: $text", $_.Exception)
    }
    Assert-E2E $result.SameAs($Baseline) "a $Description picker must not apply a watermark"
}

function test_watermark_picker_cancellation_leaves_document_unchanged {
    param($Driver, $Portal, $Work, $Baseline)
    Test-PickerNonSuccess $Driver $Portal $Work $Baseline 1 'cancelled'
}

function test_watermark_picker_error_leaves_document_unchanged {
    param($Driver, $Portal, $Work, $Baseline)
    $Portal.FailNextOpen = $true
    $index = Start-Watermark $Driver $Portal
    $beforeStatus = Get-E2EStatusFrame $Driver
    Send-EffectsPortalOpenError $Portal $index
    Wait-E2EUntil { -not (Get-E2EStatusFrame $Driver).SameAs($beforeStatus) } 'the picker method error'
    Write-E2ETrace $Driver 'interactive-effects-picker-method-error'
    try {
        $result = Export-EffectImage $Driver $Portal $Work
    }
    catch {
        $text = Get-AssertionText $_
        if ($null -eq $text) { throw }
        throw [System.Exception]::new("AssertionError: export must remain available after the picker error: $text", $_.Exception)
    }
    Assert-E2E $result.SameAs($Baseline) 'a picker method error must not apply a watermark'
}

function test_watermark_picker_other_response_leaves_document_unchanged {
    param($Driver, $Portal, $Work, $Baseline)
    Test-PickerNonSuccess $Driver $Portal $Work $Baseline 2 'ended'
}

function test_watermark_cancel_without_uris_keeps_later_dialogs_usable {
    param($Driver, $Portal, $Work, $Baseline)
    Test-PickerNonSuccess $Driver $Portal $Work $Baseline 1 'cancelled without URIs' -OmitUris
}

function test_watermark_error_without_uris_keeps_later_dialogs_usable {
    param($Driver, $Portal, $Work, $Baseline)
    Test-PickerNonSuccess $Driver $Portal $Work $Baseline 2 'failed without URIs' -OmitUris
}

function test_export_cancel_and_failure_keep_later_dialogs_usable {
    param($Driver, $Portal, $Work, $Baseline)
    foreach ($code in @(1, 2)) {
        $Portal.SaveResponses.Add(@([uint32]$code, @{}))
        $previous = $Portal.SaveRequests.Count
        $beforeStatus = Get-E2EStatusFrame $Driver
        Invoke-E2EHotkey $Driver 'e'
        Wait-E2EUntil { $Portal.SaveRequests.Count -gt $previous } 'the export dialog'
        Assert-E2E ((ConvertFrom-E2EWindowHandle $Portal.SaveParents[$previous]) -eq $Driver.Window) 'the export dialog must be parented to the editor'
        if ($code -eq 2) {
            Wait-E2EUntil { -not (Get-E2EStatusFrame $Driver).SameAs($beforeStatus) } 'the export error'
        }
        else {
            Wait-E2ESeconds 0.3
        }
        Assert-ExportMatches $Driver $Portal $Work $Baseline "an export response $code must leave the document and future pickers usable"
    }
}

function test_delayed_watermark_picker_preserves_a_newer_edit {
    param($Driver, $Portal, $Work, $Mark)
    $index = Start-Watermark $Driver $Portal
    Add-Annotation $Driver
    $edited = Export-EffectImage $Driver $Portal $Work
    $redPixels = $edited.CountColor(229, 57, 53, 255)
    Assert-E2E ($redPixels -gt 40) "the red annotation must be exported, got $redPixels pixels"
    $beforeStatus = Set-SelectionToolStatus $Driver
    Complete-EffectsPortalOpen $Portal $index 0 -Uri (ConvertTo-E2EFileUri $Mark)
    Wait-E2EUntil { -not (Get-E2EStatusFrame $Driver).SameAs($beforeStatus) } 'rejecting the stale picker result'
    Assert-ExportMatches $Driver $Portal $Work $edited 'a late picker must preserve the newer annotation and not apply the watermark'
}

function test_blocked_watermark_decode_preserves_a_newer_edit {
    param($Driver, $Portal, $Work, [byte[]]$MarkBytes)
    $fifo = Join-Path $Work 'blocked-watermark.png'
    [JiffshotE2E.Posix]::MakeFifo($fifo)
    $index = Start-Watermark $Driver $Portal
    Complete-EffectsPortalOpen $Portal $index 0 -Uri (ConvertTo-E2EFileUri $fifo)
    $fd = Open-FifoWriter $fifo
    try {
        Add-Annotation $Driver
        $edited = Export-EffectImage $Driver $Portal $Work
        $beforeStatus = Set-SelectionToolStatus $Driver
        $writer = $fd
        $fd = -1
        Send-FifoPayload $writer $MarkBytes
        Wait-E2EUntil { -not (Get-E2EStatusFrame $Driver).SameAs($beforeStatus) } 'rejecting the stale image read'
        Assert-ExportMatches $Driver $Portal $Work $edited 'a late watermark decode must preserve the newer annotation'
    }
    finally {
        if ($fd -ge 0) {
            [JiffshotE2E.Posix]::Close($fd)
        }
    }
}

function test_invalid_watermark_image_does_not_change_document {
    param($Driver, $Portal, $Work, $Baseline)
    $fifo = Join-Path $Work 'invalid-watermark.png'
    [JiffshotE2E.Posix]::MakeFifo($fifo)
    $index = Start-Watermark $Driver $Portal
    Complete-EffectsPortalOpen $Portal $index 0 -Uri (ConvertTo-E2EFileUri $fifo)
    $fd = Open-FifoWriter $fifo
    $beforeStatus = Get-E2EStatusFrame $Driver
    Send-FifoPayload $fd ([System.Text.Encoding]::ASCII.GetBytes('not an image'))
    Wait-E2EUntil { -not (Get-E2EStatusFrame $Driver).SameAs($beforeStatus) } 'the image decode error'
    Assert-ExportMatches $Driver $Portal $Work $Baseline 'a failed watermark decode must not change the document'
}

function test_nonlocal_watermark_uri_does_not_change_document {
    param($Driver, $Portal, $Work, $Baseline)
    $index = Start-Watermark $Driver $Portal
    $beforeStatus = Get-E2EStatusFrame $Driver
    Complete-EffectsPortalOpen $Portal $index 0 -Uri 'https://example.invalid/watermark.png'
    Wait-E2EUntil { -not (Get-E2EStatusFrame $Driver).SameAs($beforeStatus) } 'rejecting a nonlocal image'
    Assert-ExportMatches $Driver $Portal $Work $Baseline 'a nonlocal watermark URI must not change the document'
}

function Set-ReplacementDocument {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [Parameter(Mandatory)]$Environment, [Parameter(Mandatory)][string]$Replacement, [Parameter(Mandatory)][int[]]$Color)
    $null = Invoke-E2ECommand -ArgumentList @('xclip', '-selection', 'clipboard', '-t', 'image/png', '-i', $Replacement) -Environment $Environment -Timeout 5 -Check
    $before = $Driver.Capture(0, 0, 800, 275)
    $Driver.Wheel(750, 150, 4, 6)
    Wait-E2EUntil { -not $Driver.Capture(0, 0, 800, 275).SameAs($before) } 'the image toolbar'
    $Driver.Click(375, 207)
    Wait-E2EUntil { (Measure-E2ECanvasColor $Driver $Color[0] $Color[1] $Color[2]) -gt 5000 } 'the pasted replacement document'
    $before = $Driver.Capture(0, 0, 800, 275)
    $Driver.Wheel(750, 150, 5, 6)
    Wait-E2EUntil { -not $Driver.Capture(0, 0, 800, 275).SameAs($before) } 'the effects toolbar'
}

function test_delayed_watermark_picker_preserves_a_newer_document {
    param($Driver, $Portal, $Work, $Environment, $Replacement, $Mark)
    $index = Start-Watermark $Driver $Portal
    Set-ReplacementDocument $Driver $Environment $Replacement @(10, 200, 40)
    $expected = Export-EffectImage $Driver $Portal $Work
    Assert-E2E (Test-Pixel $expected 200 115 @(10, 200, 40, 255)) (Format-Pixel $expected.Pixel(200, 115))
    $beforeStatus = Set-SelectionToolStatus $Driver
    Complete-EffectsPortalOpen $Portal $index 0 -Uri (ConvertTo-E2EFileUri $Mark)
    Wait-E2EUntil { -not (Get-E2EStatusFrame $Driver).SameAs($beforeStatus) } 'rejecting the picker result for the replaced document'
    Assert-ExportMatches $Driver $Portal $Work $expected 'a delayed picker must not watermark a newer document'
}

function test_blocked_watermark_decode_preserves_a_newer_document {
    param($Driver, $Portal, $Work, $Environment, $Replacement, [byte[]]$MarkBytes)
    $fifo = Join-Path $Work 'blocked-before-replacement.png'
    [JiffshotE2E.Posix]::MakeFifo($fifo)
    $index = Start-Watermark $Driver $Portal
    Complete-EffectsPortalOpen $Portal $index 0 -Uri (ConvertTo-E2EFileUri $fifo)
    $fd = Open-FifoWriter $fifo
    try {
        Set-ReplacementDocument $Driver $Environment $Replacement @(170, 30, 200)
        $expected = Export-EffectImage $Driver $Portal $Work
        Assert-E2E (Test-Pixel $expected 200 115 @(170, 30, 200, 255)) (Format-Pixel $expected.Pixel(200, 115))
        $beforeStatus = Set-SelectionToolStatus $Driver
        $writer = $fd
        $fd = -1
        Send-FifoPayload $writer $MarkBytes
        Wait-E2EUntil { -not (Get-E2EStatusFrame $Driver).SameAs($beforeStatus) } 'rejecting the image read for the replaced document'
        Assert-ExportMatches $Driver $Portal $Work $expected 'a delayed image decode must not watermark a newer document'
    }
    finally {
        if ($fd -ge 0) {
            [JiffshotE2E.Posix]::Close($fd)
        }
    }
}

function Wait-SourceImage {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [Parameter(Mandatory)][string]$Description)
    Wait-E2EUntil { (Measure-E2ECanvasColor $Driver 30 120 190) -gt 5000 } $Description
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$app = $server = $driver = $portal = $null
$work = New-E2EWorkDirectory 'jiffshot-interactive-effects-ui-'
try {
    New-E2EPrivateDirectory (Join-Path $work 'runtime') | Out-Null
    $source = Join-Path $work 'capture.png'
    [System.IO.File]::WriteAllBytes($source, (Get-E2ESolidPng 30 120 190))
    Write-E2EJson ([System.IO.Path]::ChangeExtension($source, '.json')) ([ordered]@{
            captured_at  = '2026-03-01T10:20:30'
            window_title = 'Design review'
            process      = 'Browser'
        })
    $mark = Join-Path $work 'watermark.png'
    $markBytes = Get-E2ESolidPng 255 0 0
    [System.IO.File]::WriteAllBytes($mark, $markBytes)
    $replacement = Join-Path $work 'replacement.png'
    [System.IO.File]::WriteAllBytes($replacement, (Get-E2ESolidPng 10 200 40))
    $secondReplacement = Join-Path $work 'second-replacement.png'
    [System.IO.File]::WriteAllBytes($secondReplacement, (Get-E2ESolidPng 170 30 200))
    $config = Join-Path $work 'config/Jiffshot'
    New-Item -ItemType Directory -Path $config | Out-Null
    Write-E2EJson (Join-Path $config 'settings.json') ([ordered]@{ theme = 'Light' })
    $portal = New-EffectsPortal
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $displayName = $server.Display
        $environment = New-E2EEnvironment $displayName $work $runtime
        $environment['GTK_USE_PORTAL'] = '1'
        $app = Start-E2EProcess @($binaryPath, $source) $environment $work $appLog
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'the editor' 12
        $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
        Wait-SourceImage $driver 'the source image'
        Show-Effects $driver
        Write-E2ETrace $driver 'interactive-effects-controls'
        $baseline = Export-EffectImage $driver $portal $work
        Assert-E2E ($baseline.Width -eq 240 -and $baseline.Height -eq 140) "($($baseline.Width), $($baseline.Height))"
        Assert-E2E (Test-Pixel $baseline 200 115 @(30, 120, 190, 255)) (Format-Pixel $baseline.Pixel(200, 115))

        test_custom_border_parameter_is_applied_and_undoable $driver $portal $work $baseline
        Write-E2EPass 'test_custom_border_parameter_is_applied_and_undoable'
        test_invalid_parameter_preserves_document_and_switching_effect_resets_it $driver $portal $work $baseline
        Write-E2EPass 'test_invalid_parameter_preserves_document_and_switching_effect_resets_it'
        test_custom_spotlight_magnification_is_applied_and_undoable $driver $portal $work $baseline
        Write-E2EPass 'test_custom_spotlight_magnification_is_applied_and_undoable'
        test_selector_capture_info_adds_band_and_undoes $driver $portal $work $baseline
        Write-E2EPass 'test_selector_capture_info_adds_band_and_undoes'
        test_selector_capture_info_rejects_malformed_sidecar $driver $portal $work $source $baseline
        Write-E2EPass 'test_selector_capture_info_rejects_malformed_sidecar'
        test_selector_capture_info_rejects_oversized_sidecar $driver $portal $work $source $baseline
        Write-E2EPass 'test_selector_capture_info_rejects_oversized_sidecar'
        test_selector_spotlight_magnifies_image_center_and_undoes $driver $portal $work $baseline
        Write-E2EPass 'test_selector_spotlight_magnifies_image_center_and_undoes'
        test_watermark_picker_blends_local_image_and_undoes $driver $portal $work $baseline $mark
        Write-E2EPass 'test_watermark_picker_blends_local_image_and_undoes'
        test_watermark_parameter_is_snapshotted_before_the_picker_and_undoes $driver $portal $work $baseline $mark
        Write-E2EPass 'test_watermark_parameter_is_snapshotted_before_the_picker_and_undoes'
        test_invalid_watermark_parameter_does_not_open_picker $driver $portal $work $baseline
        Write-E2EPass 'test_invalid_watermark_parameter_does_not_open_picker'
        test_invalid_watermark_image_does_not_change_document $driver $portal $work $baseline
        Write-E2EPass 'test_invalid_watermark_image_does_not_change_document'
        test_nonlocal_watermark_uri_does_not_change_document $driver $portal $work $baseline
        Write-E2EPass 'test_nonlocal_watermark_uri_does_not_change_document'
        test_delayed_watermark_picker_preserves_a_newer_edit $driver $portal $work $mark
        Write-E2EPass 'test_delayed_watermark_picker_preserves_a_newer_edit'
        Undo-Effect $driver
        Assert-ExportMatches $driver $portal $work $baseline
        test_blocked_watermark_decode_preserves_a_newer_edit $driver $portal $work $markBytes
        Write-E2EPass 'test_blocked_watermark_decode_preserves_a_newer_edit'
        Undo-Effect $driver
        Assert-ExportMatches $driver $portal $work $baseline
        test_delayed_watermark_picker_preserves_a_newer_document $driver $portal $work $environment $replacement $mark
        Write-E2EPass 'test_delayed_watermark_picker_preserves_a_newer_document'
        test_blocked_watermark_decode_preserves_a_newer_document $driver $portal $work $environment $secondReplacement $markBytes
        Write-E2EPass 'test_blocked_watermark_decode_preserves_a_newer_document'
        test_export_cancel_and_failure_keep_later_dialogs_usable $driver $portal $work (Export-EffectImage $driver $portal $work)
        Write-E2EPass 'test_export_cancel_and_failure_keep_later_dialogs_usable'
        $failures = [System.Collections.Generic.List[string]]::new()
        foreach ($test in @(
                'test_watermark_picker_cancellation_leaves_document_unchanged',
                'test_watermark_picker_error_leaves_document_unchanged',
                'test_watermark_picker_other_response_leaves_document_unchanged',
                'test_watermark_cancel_without_uris_keeps_later_dialogs_usable',
                'test_watermark_error_without_uris_keeps_later_dialogs_usable'
            )) {
            Close-E2EX11Driver $driver
            $driver = $null
            Stop-E2EProcess $app
            $app = $null
            Wait-E2EUntil { $null -eq (Get-E2EWindowInfo $environment) } 'the previous editor to close'
            $portal.SaveDestinations.Clear()
            $app = Start-E2EProcess @($binaryPath, $source) $environment $work $appLog
            Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'a fresh editor' 12
            $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
            Wait-SourceImage $driver 'the source image in the fresh editor'
            Show-Effects $driver
            $freshBaseline = Export-EffectImage $driver $portal $work
            $failure = $null
            try {
                & $test $driver $portal $work $freshBaseline
            }
            catch {
                $failure = Get-AssertionText $_
                if ($null -eq $failure) { throw }
            }
            if ($null -ne $failure) {
                Write-E2ETrace $driver "interactive-effects-$test"
                $failures.Add("${test}: $failure")
                [Console]::Error.WriteLine("FAIL $($failures[$failures.Count - 1])")
                [Console]::Error.Flush()
            }
            else {
                Write-E2EPass $test
            }
        }
        if ($failures.Count -gt 0) {
            Assert-E2E $false ($failures -join "`n")
        }
        Write-E2ELine 'GUI interactive effects, portal errors, undo, and stale-result guards passed'
    }
    catch {
        if ($null -ne $driver) {
            Write-E2ETrace $driver 'interactive-effects-failure'
        }
        Write-E2ELogTail @($appLog, $xvfbLog)
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
