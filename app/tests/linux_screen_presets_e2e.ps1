#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Exercise Linux screen capture presets with a private X11 desktop and portal.

.DESCRIPTION
The optional real-portal mode uses the isolated Sway fixture's color-control file.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir,
    [switch]$RealPortal
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.Editor.psm1') -Force

$CapturePresets = 'linux_screen_capture_presets'
$PngEndMarker = [byte[]](0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82)

function Format-E2EJson {
    param([AllowNull()]$Value)
    ConvertTo-Json -InputObject $Value -Depth 16 -Compress
}

#region Saved images

function Get-SavedImages {
    <# Completed Screenshot-*.png files in Directory, sorted by path. #>
    param([Parameter(Mandatory)][string]$Directory)
    $images = [System.Collections.Generic.List[string]]::new()
    foreach ($path in [System.IO.Directory]::GetFiles($Directory, 'Screenshot-*.png')) {
        $bytes = [System.IO.File]::ReadAllBytes($path)
        if ($bytes.Length -ge $PngEndMarker.Length -and
            [System.Linq.Enumerable]::SequenceEqual([byte[]]$bytes[($bytes.Length - $PngEndMarker.Length)..($bytes.Length - 1)], $PngEndMarker)) {
            $images.Add($path)
        }
    }
    $images.Sort([System.StringComparer]::Ordinal)
    , $images.ToArray()
}

function Assert-SavedImage {
    <# Checks a 1280x720 PNG whose center pixel is within 8 of the expected RGB color. #>
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][int[]]$Expected)
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    $signature = [byte[]](0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A)
    Assert-E2E ($bytes.Length -ge 24 -and [System.Linq.Enumerable]::SequenceEqual([byte[]]$bytes[0..7], $signature)) "$Path is not a PNG"
    $width = [System.Buffers.Binary.BinaryPrimitives]::ReadUInt32BigEndian([System.ReadOnlySpan[byte]]::new($bytes, 16, 4))
    $height = [System.Buffers.Binary.BinaryPrimitives]::ReadUInt32BigEndian([System.ReadOnlySpan[byte]]::new($bytes, 20, 4))
    Assert-E2E ($width -eq 1280 -and $height -eq 720) "($width, $height)"
    $image = [JiffshotE2E.Png]::Decode($bytes)
    Assert-E2E ($image.Rgba.Length -eq $width * $height * 4) "$Path decoded to $($image.Rgba.Length) bytes"
    $color = $image.Pixel([int][Math]::Floor($width / 2), [int][Math]::Floor($height / 2))
    $close = $true
    for ($channel = 0; $channel -lt 3; $channel++) {
        if ([Math]::Abs($color[$channel] - $Expected[$channel]) -gt 8) {
            $close = $false
        }
    }
    Assert-E2E $close "($Path, [$($color[0..2] -join ', ')], ($($Expected -join ', ')))"
}

#endregion

#region Fixture

function New-PresetFixture {
    param([string]$Binary, [string]$Work, $Portal, [JiffshotE2E.TextReader]$Reader, [System.Collections.IDictionary]$Environment, [string]$AppLog)
    $captures = Join-Path $Work 'captures'
    [pscustomobject]@{
        Binary      = $Binary
        Work        = $Work
        Portal      = $Portal
        Text        = $Reader
        Environment = $Environment
        AppLog      = $AppLog
        App         = $null
        Driver      = $null
        Settings    = Join-Path $Work 'config/Jiffshot/settings.json'
        Captures    = $captures
        Original    = [ordered]@{
            theme                 = 'Light'
            capture_directory     = $captures
            capture_delay_seconds = 2
            capture_cursor        = $false
            recording_cursor      = $true
            copy_to_clipboard     = $false
            other                 = [ordered]@{ keep = @(1, 'two') }
        }
    }
}

function Get-FixturePreferences {
    param([Parameter(Mandatory)]$Fixture)
    , (Read-E2EJson $Fixture.Settings)
}

function Get-FixtureSettingsBytes {
    param([Parameter(Mandatory)]$Fixture)
    , [System.IO.File]::ReadAllBytes($Fixture.Settings)
}

function Test-FixtureSettingsBytes {
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)][byte[]]$Expected)
    [System.Linq.Enumerable]::SequenceEqual((Get-FixtureSettingsBytes $Fixture), $Expected)
}

function Test-FixtureNoCaptures {
    param([Parameter(Mandatory)]$Fixture)
    @(Get-ChildItem -LiteralPath $Fixture.Captures -Force).Count -eq 0
}

function Close-FixtureApp {
    param([Parameter(Mandatory)]$Fixture)
    if ($null -ne $Fixture.Driver) {
        Close-E2EX11Driver $Fixture.Driver
        $Fixture.Driver = $null
    }
    Stop-E2EProcess $Fixture.App
    $Fixture.App = $null
    Wait-E2EUntil { $null -eq (Get-E2EWindowInfo $Fixture.Environment) } 'the previous editor to close'
}

function Start-FixtureApp {
    param([Parameter(Mandatory)]$Fixture, [AllowNull()]$Preferences = $null)
    if ($null -ne $Fixture.App) {
        Close-FixtureApp $Fixture
    }
    if ($null -ne $Preferences) {
        Write-E2EJson $Fixture.Settings $Preferences
    }
    $Fixture.App = Start-E2EProcess @($Fixture.Binary) $Fixture.Environment $Fixture.Work $Fixture.AppLog
    Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $Fixture.Environment) } 'the editor' 15
    $Fixture.Driver = New-E2EX11Driver $Fixture.Environment['DISPLAY'] (Get-E2EWindowInfo $Fixture.Environment)
    Wait-E2ESeconds 0.4
}

function Find-FixtureLabel {
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)][string]$Label)
    , (Find-E2EScrollingLabel $Fixture.Text $Fixture.Driver $Label)
}

function Invoke-FixtureLabelClick {
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)][string]$Label)
    $position = Find-FixtureLabel $Fixture $Label
    $Fixture.Driver.Click($position[0], $position[1])
    Wait-E2ESeconds 0.2
}

function Show-FixturePresets {
    param([Parameter(Mandatory)]$Fixture)
    $position = Find-FixtureLabel $Fixture 'Screen presets'
    Write-E2ETrace $Fixture.Driver 'screen-presets-before-toggle'
    $Fixture.Driver.Click($position[0], $position[1])
    Wait-E2ESeconds 0.3
    Write-E2ETrace $Fixture.Driver 'screen-presets-after-toggle'
    $null = Find-FixtureLabel $Fixture 'Save screen preset'
    Write-E2ETrace $Fixture.Driver 'screen-presets-controls'
}

function Set-FixtureField {
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)][int]$X, [Parameter(Mandatory)][AllowEmptyString()][string]$Value)
    # TextReader can merge the textbox border into the preceding label. Anchor
    # the row by its save button and use the established 800-pixel X11 layout.
    $y = (Find-FixtureLabel $Fixture 'Save screen preset')[1]
    $Fixture.Driver.Click($X, $y)
    $Fixture.Driver.SelectAll()
    $Fixture.Driver.Press('BackSpace')
    $keys = @{ ' ' = 'space'; '-' = 'minus'; '.' = 'period' }
    foreach ($character in $Value.ToCharArray()) {
        $name = [string]$character
        $key = if ($keys.ContainsKey($name)) { $keys[$name] } else { $name }
        $Fixture.Driver.Press($key)
    }
}

function Set-FixtureName {
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)][AllowEmptyString()][string]$Value)
    Set-FixtureField $Fixture 165 $Value
}

function Set-FixtureDelay {
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)][AllowEmptyString()][string]$Value)
    Set-FixtureField $Fixture 390 $Value
}

function Wait-FixturePreferences {
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)]$Expected)
    Wait-E2EUntil { Test-E2EDeepEqual (Get-FixturePreferences $Fixture) $Expected } 'the expected saved preferences'
    $saved = Get-FixturePreferences $Fixture
    Assert-E2E (Test-E2EDeepEqual $saved $Expected) "$(Format-E2EJson $saved) != $(Format-E2EJson $Expected)"
}

function Wait-FixtureStatus {
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)][string]$Expected, [double]$Timeout = 12)
    Wait-E2EStatus $Fixture.Text $Fixture.Driver $Expected $Timeout
}

function Set-FixtureClipboard {
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)][string]$Text)
    $null = Invoke-E2ECommand @('/bin/sh', '-c', 'exec xclip -selection clipboard >/dev/null 2>&1') -Environment $Fixture.Environment `
        -InputBytes ([System.Text.Encoding]::UTF8.GetBytes($Text)) -Timeout 5 -Check
}

function Get-FixtureClipboard {
    param([Parameter(Mandatory)]$Fixture)
    $result = Invoke-E2ECommand @('xclip', '-selection', 'clipboard', '-o') -Environment $Fixture.Environment -CaptureOutput -Timeout 5 -Check
    [System.Text.Encoding]::UTF8.GetString($result.Output)
}

function Assert-FixtureClipboard {
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)][string]$Expected)
    $actual = Get-FixtureClipboard $Fixture
    Assert-E2E ($actual -ceq $Expected) "clipboard was '$actual', expected '$Expected'"
}

function Assert-FixtureSettingsUnchanged {
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)][byte[]]$Before, [Parameter(Mandatory)][string]$Message)
    Assert-E2E (Test-FixtureSettingsBytes $Fixture $Before) $Message
}

function Start-FixtureChooser {
    <# Clicks a capture label and returns @{ Session; Request; Options } for the held ScreenCast.Start. #>
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)][string]$Label)
    $previous = $Fixture.Portal.ScreenStarts.Count
    Invoke-FixtureLabelClick $Fixture $Label
    Wait-E2EUntil { $Fixture.Portal.ScreenStarts.Count -eq $previous + 1 } "the ScreenCast chooser for $Label"
    $options = $Fixture.Portal.ScreenSelectionOptions[-1]
    Assert-E2E ($options['types'].Value -eq 3) 'the chooser must offer monitors and windows'
    $multiple = $options['multiple'].Value
    Assert-E2E ($multiple -is [bool] -and -not $multiple) 'the chooser must select one source'
    $chooserSession, $chooserRequest = $Fixture.Portal.ScreenStarts[-1]
    [pscustomobject]@{ Session = $chooserSession; Request = $chooserRequest; Options = $options }
}

function Deny-FixtureChooser {
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)]$Chooser)
    Send-PortalResponse $Fixture.Portal $Chooser.Request 1 @{}
    Wait-E2EUntil { $Fixture.Portal.Closes.Contains($Chooser.Session) } 'the cancelled portal session to close'
    Wait-E2ESeconds 0.2
}

function Get-PresetSettings {
    param([Parameter(Mandatory)]$Fixture, [System.Collections.IDictionary]$Override = @{}, [Parameter(Mandatory)][AllowEmptyCollection()][AllowNull()]$PresetList)
    $settings = Copy-E2EMap $Fixture.Original $Override
    $settings[$CapturePresets] = $PresetList
    $settings
}

function New-Preset {
    param([string]$Name, [int]$Delay, [bool]$Cursor)
    [ordered]@{ name = $Name; delay_seconds = $Delay; include_cursor = $Cursor }
}

#endregion

#region Cases

function test_screen_preset_save_replacement_and_independent_preferences {
    param($Fixture)
    Start-FixtureApp $Fixture $Fixture.Original
    Show-FixturePresets $Fixture
    $row = (Find-FixtureLabel $Fixture 'Save screen preset')[1]
    $nameLabel = $Fixture.Text.Recognize($Fixture.Driver.Capture(0, $row - 12, 92, 24), 7, $false)
    $delayLabel = $Fixture.Text.Recognize($Fixture.Driver.Capture(243, $row - 18, 118, 22), 7, $false)
    Assert-E2E ((ConvertTo-E2ELabelKey $nameLabel).Contains('presetname')) $nameLabel
    Assert-E2E ((ConvertTo-E2ELabelKey $delayLabel).Contains('delayseconds')) $delayLabel
    Set-FixtureName $Fixture '  work  '
    Set-FixtureDelay $Fixture '5'
    Invoke-FixtureLabelClick $Fixture 'Preset includes cursor'
    Invoke-FixtureLabelClick $Fixture 'Save screen preset'
    $expected = Get-PresetSettings $Fixture -PresetList @(New-Preset 'work' 5 $true)
    Wait-FixturePreferences $Fixture $expected
    $null = Find-FixtureLabel $Fixture 'Capture: work'
    Wait-FixtureStatus $Fixture 'preset'

    Set-FixtureName $Fixture ' work '
    Set-FixtureDelay $Fixture '60'
    Invoke-FixtureLabelClick $Fixture 'Preset includes cursor'
    Invoke-FixtureLabelClick $Fixture 'Save screen preset'
    $expected[$CapturePresets] = @(New-Preset 'work' 60 $false)
    Wait-FixturePreferences $Fixture $expected
    $Fixture.Portal.HoldStart = $false
    $remoteOpens = $Fixture.Portal.RemoteOpens
    $chooser = Start-FixtureChooser $Fixture 'Capture: work'
    Assert-E2E ($chooser.Options['cursor_mode'].Value -eq 1) 'the replaced preset must use its new cursor'
    Wait-E2EUntil { $Fixture.Portal.RemoteOpens -eq $remoteOpens + 1 } 'the replaced stream'
    Wait-FixtureStatus $Fixture 'Capturing in 60 seconds'
    Invoke-FixtureLabelClick $Fixture 'Cancel delayed capture'
    Wait-E2EUntil { $Fixture.Portal.Closes.Contains($chooser.Session) } 'the replaced session to close'
    Wait-FixtureStatus $Fixture 'Delayed capture cancelled'
    Assert-E2E (Test-FixtureNoCaptures $Fixture) 'cancellation must not save a capture'
    $saved = Get-FixturePreferences $Fixture
    Assert-E2E (Test-E2EDeepEqual $saved $expected) (Format-E2EJson $saved)
}

function test_screen_preset_invalid_input_does_not_overwrite {
    param($Fixture)
    $initial = Get-PresetSettings $Fixture -PresetList @(New-Preset 'keep' 1 $false)
    Start-FixtureApp $Fixture $initial
    Show-FixturePresets $Fixture
    $before = Get-FixtureSettingsBytes $Fixture
    $previousRequests = $Fixture.Portal.CaptureRequests
    foreach ($pair in @(@('', '1'), @('   ', '1'), @('valid', ''), @('valid', '-1'), @('valid', '1.5'), @('valid', '61'))) {
        $name, $delay = $pair
        Set-FixtureName $Fixture 'missing'
        Invoke-FixtureLabelClick $Fixture 'Delete screen preset'
        Wait-FixtureStatus $Fixture 'Could not delete'
        Assert-FixtureSettingsUnchanged $Fixture $before 'missing deletion must not write'
        Set-FixtureName $Fixture $name
        Set-FixtureDelay $Fixture $delay
        Invoke-FixtureLabelClick $Fixture 'Save screen preset'
        Wait-FixtureStatus $Fixture 'Could not save'
        Assert-FixtureSettingsUnchanged $Fixture $before "('$name', '$delay')"
        Assert-E2E ($Fixture.Portal.CaptureRequests -eq $previousRequests) "('$name', '$delay')"
    }
    $saved = Get-FixturePreferences $Fixture
    Assert-E2E (Test-E2EDeepEqual $saved $initial) (Format-E2EJson $saved)
}

function test_screen_preset_survives_restart_and_can_be_deleted {
    param($Fixture)
    Start-FixtureApp $Fixture $Fixture.Original
    Show-FixturePresets $Fixture
    Set-FixtureName $Fixture 'persist'
    Set-FixtureDelay $Fixture '4'
    Invoke-FixtureLabelClick $Fixture 'Save screen preset'
    $saved = Get-PresetSettings $Fixture -PresetList @(New-Preset 'persist' 4 $false)
    Wait-FixturePreferences $Fixture $saved
    Close-FixtureApp $Fixture
    Start-FixtureApp $Fixture
    Show-FixturePresets $Fixture
    $null = Find-FixtureLabel $Fixture 'Capture: persist'
    Assert-E2E (Test-E2EDeepEqual (Get-FixturePreferences $Fixture) $saved) 'restart must retain all preset options'

    $before = Get-FixtureSettingsBytes $Fixture
    Set-FixtureName $Fixture 'missing'
    Invoke-FixtureLabelClick $Fixture 'Delete screen preset'
    Wait-FixtureStatus $Fixture 'Could not delete'
    Assert-FixtureSettingsUnchanged $Fixture $before 'missing deletion must not rewrite settings'

    Set-FixtureName $Fixture ' persist '
    Invoke-FixtureLabelClick $Fixture 'Delete screen preset'
    $deleted = Get-PresetSettings $Fixture -PresetList @()
    Wait-FixturePreferences $Fixture $deleted
    Wait-FixtureStatus $Fixture 'deleted'
    Assert-E2E ($null -eq (Find-E2ELabel $Fixture.Text $Fixture.Driver 'Capture: persist')) 'the deleted preset must not be shown'
    Close-FixtureApp $Fixture
    Start-FixtureApp $Fixture
    Show-FixturePresets $Fixture
    $current = Get-FixturePreferences $Fixture
    Assert-E2E (Test-E2EDeepEqual $current $deleted) (Format-E2EJson $current)
    Assert-E2E ($null -eq (Find-E2ELabel $Fixture.Text $Fixture.Driver 'Capture: persist')) 'the deleted preset must stay hidden after restart'
}

function test_screen_preset_cursor_mode_and_normal_capture_are_independent {
    param($Fixture)
    $Fixture.Portal.HoldStart = $true
    $presetValues = @((New-Preset 'with' 60 $true), (New-Preset 'without' 60 $false))
    foreach ($row in @(@($false, 'Capture: with', 2, 1), @($true, 'Capture: without', 1, 2))) {
        $normalCursor, $presetLabel, $presetMode, $normalMode = $row
        $initial = Get-PresetSettings $Fixture @{ capture_delay_seconds = 30; capture_cursor = $normalCursor } -PresetList $presetValues
        Start-FixtureApp $Fixture $initial
        Show-FixturePresets $Fixture
        $before = Get-FixtureSettingsBytes $Fixture
        $chooser = Start-FixtureChooser $Fixture $presetLabel
        Assert-E2E ($chooser.Options['cursor_mode'].Value -eq $presetMode) (Format-E2EJson $chooser.Options)
        Deny-FixtureChooser $Fixture $chooser
        $chooser = Start-FixtureChooser $Fixture 'Capture screen or window'
        Assert-E2E ($chooser.Options['cursor_mode'].Value -eq $normalMode) (Format-E2EJson $chooser.Options)
        Deny-FixtureChooser $Fixture $chooser
        Assert-FixtureSettingsUnchanged $Fixture $before 'running presets must not change preferences'
        Assert-E2E (Test-FixtureNoCaptures $Fixture) 'cancelled choosers must not save captures'
    }
}

function test_screen_preset_chooser_cancellation_closes_session_without_saving {
    param($Fixture)
    $Fixture.Portal.HoldStart = $true
    $initial = Get-PresetSettings $Fixture @{ copy_to_clipboard = $true } -PresetList @(New-Preset 'cancel' 60 $true)
    Start-FixtureApp $Fixture $initial
    Set-FixtureClipboard $Fixture 'preset chooser sentinel'
    Show-FixturePresets $Fixture
    $before = Get-FixtureSettingsBytes $Fixture
    $remoteOpens = $Fixture.Portal.RemoteOpens
    $chooser = Start-FixtureChooser $Fixture 'Capture: cancel'
    Assert-E2E ($chooser.Options['cursor_mode'].Value -eq 2) 'the preset must include the cursor'
    Deny-FixtureChooser $Fixture $chooser
    Assert-E2E ($Fixture.Portal.RemoteOpens -eq $remoteOpens) 'rejected chooser must not open PipeWire'
    Assert-E2E (Test-FixtureNoCaptures $Fixture) 'chooser cancellation must not save a capture'
    Assert-FixtureClipboard $Fixture 'preset chooser sentinel'
    Assert-FixtureSettingsUnchanged $Fixture $before 'chooser cancellation must not change preferences'

    $chooser = Start-FixtureChooser $Fixture 'Capture: cancel'
    Deny-FixtureChooser $Fixture $chooser
    Assert-E2E (Test-FixtureNoCaptures $Fixture) 'another cancelled request must also save nothing'
}

function test_screen_preset_cancel_during_chooser_is_honored_after_selection {
    param($Fixture)
    $Fixture.Portal.HoldStart = $true
    $initial = Get-PresetSettings $Fixture @{ copy_to_clipboard = $true } -PresetList @(New-Preset 'pending' 60 $false)
    Start-FixtureApp $Fixture $initial
    Set-FixtureClipboard $Fixture 'pending preset sentinel'
    Show-FixturePresets $Fixture
    $before = Get-FixtureSettingsBytes $Fixture
    $remoteOpens = $Fixture.Portal.RemoteOpens
    $chooser = Start-FixtureChooser $Fixture 'Capture: pending'
    Assert-E2E ($chooser.Options['cursor_mode'].Value -eq 1) 'the preset must hide the cursor'
    Invoke-FixtureLabelClick $Fixture 'Cancel delayed capture'
    Wait-FixtureStatus $Fixture 'Cancellation requested'
    Assert-E2E ($Fixture.Portal.RemoteOpens -eq $remoteOpens) 'a pending chooser must not open PipeWire'
    Send-TimedPortalStart $Fixture.Portal $chooser.Request
    Wait-E2EUntil { $Fixture.Portal.RemoteOpens -eq $remoteOpens + 1 } 'the selected source'
    Wait-E2EUntil { $Fixture.Portal.Closes.Contains($chooser.Session) } 'the cancelled pending session to close'
    Wait-FixtureStatus $Fixture 'Delayed capture cancelled'
    Assert-E2E (Test-FixtureNoCaptures $Fixture) 'cancellation must not save a capture'
    Assert-FixtureClipboard $Fixture 'pending preset sentinel'
    Assert-FixtureSettingsUnchanged $Fixture $before 'cancellation must not change preferences'
}

function test_screen_preset_countdown_uses_saved_delay_and_can_be_cancelled {
    param($Fixture)
    $Fixture.Portal.HoldStart = $false
    $initial = Get-PresetSettings $Fixture @{ capture_delay_seconds = 30; copy_to_clipboard = $true } -PresetList @(New-Preset 'slow' 60 $true)
    Start-FixtureApp $Fixture $initial
    Set-FixtureClipboard $Fixture 'preset countdown sentinel'
    Show-FixturePresets $Fixture
    $before = Get-FixtureSettingsBytes $Fixture
    $remoteOpens = $Fixture.Portal.RemoteOpens
    $chooser = Start-FixtureChooser $Fixture 'Capture: slow'
    Assert-E2E ($chooser.Options['cursor_mode'].Value -eq 2) 'the preset must include the cursor'
    Wait-E2EUntil { $Fixture.Portal.RemoteOpens -eq $remoteOpens + 1 } 'the preset stream'
    Wait-FixtureStatus $Fixture 'Capturing in 60 seconds'
    Assert-E2E (-not $Fixture.Portal.Closes.Contains($chooser.Session)) 'the preset must wait before finishing'
    $requests = $Fixture.Portal.CaptureRequests
    Invoke-FixtureLabelClick $Fixture 'Capture: slow'
    Wait-FixtureStatus $Fixture 'already in progress'
    Assert-E2E ($Fixture.Portal.CaptureRequests -eq $requests) 'an active countdown must block another'
    Invoke-FixtureLabelClick $Fixture 'Cancel delayed capture'
    Wait-E2EUntil { $Fixture.Portal.Closes.Contains($chooser.Session) } 'the cancelled preset session to close'
    Wait-FixtureStatus $Fixture 'Delayed capture cancelled'
    Assert-E2E (Test-FixtureNoCaptures $Fixture) 'countdown cancellation must not save a capture'
    Assert-FixtureClipboard $Fixture 'preset countdown sentinel'
    Assert-FixtureSettingsUnchanged $Fixture $before 'countdown cancellation must not change preferences'

    $remoteOpens = $Fixture.Portal.RemoteOpens
    $chooser = Start-FixtureChooser $Fixture 'Capture screen or window'
    Assert-E2E ($chooser.Options['cursor_mode'].Value -eq 1) 'the normal capture must hide the cursor'
    Wait-E2EUntil { $Fixture.Portal.RemoteOpens -eq $remoteOpens + 1 } 'the normal stream'
    Wait-FixtureStatus $Fixture 'Capturing in 30 seconds'
    Invoke-FixtureLabelClick $Fixture 'Cancel delayed capture'
    Wait-E2EUntil { $Fixture.Portal.Closes.Contains($chooser.Session) } 'the cancelled normal session to close'
    Wait-FixtureStatus $Fixture 'Delayed capture cancelled'
    Assert-E2E (Test-FixtureNoCaptures $Fixture) 'countdown cancellation must not save a capture'
    Assert-FixtureClipboard $Fixture 'preset countdown sentinel'
    Assert-FixtureSettingsUnchanged $Fixture $before 'countdown cancellation must not change preferences'
}

function test_screen_preset_invalid_stored_values_remain_unchanged {
    param($Fixture)
    $cases = @(
        [ordered]@{ not = 'an array' },
        @((New-Preset 'dup' 0 $false), (New-Preset 'dup' 2 $true)),
        @(, (New-Preset 'bad' 61 $true))
    )
    foreach ($invalid in $cases) {
        $initial = Get-PresetSettings $Fixture -PresetList $invalid
        Start-FixtureApp $Fixture $initial
        $before = Get-FixtureSettingsBytes $Fixture
        $previousRequests = $Fixture.Portal.CaptureRequests
        Wait-FixtureStatus $Fixture 'Could not load screen capture presets'
        Show-FixturePresets $Fixture
        Set-FixtureName $Fixture 'new'
        Set-FixtureDelay $Fixture '1'
        Invoke-FixtureLabelClick $Fixture 'Save screen preset'
        Wait-FixtureStatus $Fixture 'Could not save'
        Assert-FixtureSettingsUnchanged $Fixture $before (Format-E2EJson $invalid)
        Invoke-FixtureLabelClick $Fixture 'Delete screen preset'
        Wait-FixtureStatus $Fixture 'Could not delete'
        Assert-FixtureSettingsUnchanged $Fixture $before (Format-E2EJson $invalid)
        Assert-E2E ($Fixture.Portal.CaptureRequests -eq $previousRequests) 'invalid presets must not request a capture'
        Assert-E2E (Test-FixtureNoCaptures $Fixture) 'invalid presets must not save a capture'
    }
}

function test_screen_preset_removed_externally_cannot_start_capture {
    param($Fixture)
    $initial = Get-PresetSettings $Fixture -PresetList @(New-Preset 'gone' 0 $true)
    Start-FixtureApp $Fixture $initial
    Show-FixturePresets $Fixture
    $null = Find-FixtureLabel $Fixture 'Capture: gone'
    Write-E2EJson $Fixture.Settings (Get-PresetSettings $Fixture -PresetList @())
    $before = Get-FixtureSettingsBytes $Fixture
    $previousRequests = $Fixture.Portal.CaptureRequests
    Invoke-FixtureLabelClick $Fixture 'Capture: gone'
    Wait-FixtureStatus $Fixture 'Could not run screen capture preset'
    Assert-E2E ($Fixture.Portal.CaptureRequests -eq $previousRequests) 'a removed preset must not request a capture'
    Assert-FixtureSettingsUnchanged $Fixture $before 'a removed preset must not change preferences'
    Assert-E2E (Test-FixtureNoCaptures $Fixture) 'a removed preset must not save a capture'
}

function Get-DelayColorFile {
    $colorFile = $env:JIFFSHOT_DELAY_COLOR_FILE
    if (-not $colorFile) {
        throw [System.InvalidOperationException]::new('real portal mode requires JIFFSHOT_DELAY_COLOR_FILE')
    }
    $colorFile
}

function test_real_saved_preset_delivers_delayed_png_and_clipboard {
    param($Fixture)
    $colorFile = Get-DelayColorFile
    $initial = Copy-E2EMap $Fixture.Original @{ capture_delay_seconds = 30; copy_to_clipboard = $true }
    Start-FixtureApp $Fixture $initial
    Set-FixtureClipboard $Fixture 'real preset sentinel'
    Show-FixturePresets $Fixture
    Set-FixtureName $Fixture 'real'
    Set-FixtureDelay $Fixture '3'
    Invoke-FixtureLabelClick $Fixture 'Save screen preset'
    $saved = Copy-E2EMap $initial @{ $CapturePresets = @(New-Preset 'real' 3 $false) }
    Wait-FixturePreferences $Fixture $saved
    Close-FixtureApp $Fixture
    Start-FixtureApp $Fixture
    Show-FixturePresets $Fixture
    Assert-E2E (Test-E2EDeepEqual (Get-FixturePreferences $Fixture) $saved) 'the real capture must use the persisted preset'

    $started = [System.Diagnostics.Stopwatch]::StartNew()
    Invoke-FixtureLabelClick $Fixture 'Capture: real'
    Wait-FixtureStatus $Fixture 'Capturing in 3 seconds' 25
    Assert-E2E ((Get-SavedImages $Fixture.Captures).Count -eq 0) 'the countdown must precede saving'
    [System.IO.File]::WriteAllText($colorFile, 'e02040')
    Wait-E2EUntil { (Get-SavedImages $Fixture.Captures).Count -eq 1 } 'the real preset PNG' 30
    $elapsed = $started.Elapsed.TotalSeconds
    Assert-E2E ($elapsed -ge 2.0) ('the three-second preset completed too early: {0:F2}s' -f $elapsed)
    $image = (Get-SavedImages $Fixture.Captures)[0]
    Assert-SavedImage $image @(0xE0, 0x20, 0x40)
    Wait-FixtureStatus $Fixture 'Screen captured' 20
    Wait-E2EUntil { $Fixture.Driver.RedCanvasPixels() -gt 500 } 'the captured image to appear in the editor' 15
    $clipboard = Invoke-E2ECommand @('xclip', '-selection', 'clipboard', '-t', 'image/png', '-o') -Environment $Fixture.Environment -CaptureOutput -Timeout 10 -Check
    $clipboardImage = Join-Path $Fixture.Work 'preset-clipboard.png'
    [System.IO.File]::WriteAllBytes($clipboardImage, $clipboard.Output)
    Assert-SavedImage $clipboardImage @(0xE0, 0x20, 0x40)
    Assert-E2E (Test-E2EDeepEqual (Get-FixturePreferences $Fixture) $saved) 'capture must not change normal or preset options'
}

function test_real_saved_preset_respects_disabled_clipboard {
    param($Fixture)
    $colorFile = Get-DelayColorFile
    [System.IO.File]::WriteAllText($colorFile, 'e02040')
    $initial = Copy-E2EMap $Fixture.Original @{ capture_delay_seconds = 30; capture_cursor = $true; copy_to_clipboard = $false }
    Start-FixtureApp $Fixture $initial
    Set-FixtureClipboard $Fixture 'clipboard disabled sentinel'
    Show-FixturePresets $Fixture
    Set-FixtureName $Fixture 'nocopy'
    Set-FixtureDelay $Fixture '0'
    Invoke-FixtureLabelClick $Fixture 'Preset includes cursor'
    Invoke-FixtureLabelClick $Fixture 'Save screen preset'
    $saved = Copy-E2EMap $initial @{ $CapturePresets = @(New-Preset 'nocopy' 0 $false) }
    Wait-FixturePreferences $Fixture $saved
    $previous = [System.Collections.Generic.HashSet[string]]::new([string[]](Get-SavedImages $Fixture.Captures), [System.StringComparer]::Ordinal)
    $newImages = { , @((Get-SavedImages $Fixture.Captures) | Where-Object { -not $previous.Contains($_) }) }
    Invoke-FixtureLabelClick $Fixture 'Capture: nocopy'
    Wait-E2EUntil { (& $newImages).Count -eq 1 } 'the real preset PNG with clipboard disabled' 30
    $image = (& $newImages)[0]
    Assert-SavedImage $image @(0xE0, 0x20, 0x40)
    Wait-FixtureStatus $Fixture 'Screen captured' 20
    Assert-FixtureClipboard $Fixture 'clipboard disabled sentinel'
    $current = Get-FixturePreferences $Fixture
    Assert-E2E (Test-E2EDeepEqual $current $saved) (Format-E2EJson $current)
}

function Invoke-GuiCases {
    param([Parameter(Mandatory)][AllowEmptyCollection()][string[]]$Cases, [Parameter(Mandatory)]$Fixture)
    if ($Cases.Count -eq 0) {
        throw [System.Exception]::new('AssertionError: no screen preset GUI tests were selected')
    }
    $unique = [System.Collections.Generic.HashSet[string]]::new($Cases, [System.StringComparer]::Ordinal)
    $unsafe = @($Cases | Where-Object { $_ -cnotmatch '^test_[A-Za-z0-9_]*$' })
    if ($unique.Count -ne $Cases.Count -or $unsafe.Count -gt 0) {
        throw [System.Exception]::new("AssertionError: unsafe or duplicate GUI case names: $($Cases -join ', ')")
    }
    for ($index = 0; $index -lt $Cases.Count; $index++) {
        $name = $Cases[$index]
        try {
            & $name $Fixture
        }
        catch {
            [Console]::Error.WriteLine("FAIL $name; $($Cases.Count - $index - 1) case(s) not run")
            [Console]::Error.Flush()
            throw
        }
        Write-E2EPass $name
    }
    Write-E2ELine "GUI test result: ok. $($Cases.Count) passed"
}

#endregion

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$portal = $null
if (-not $RealPortal) {
    $portal = New-TimedPortal
}
$server = $fixture = $reader = $null
$work = New-E2EWorkDirectory 'jiffshot-screen-presets-ui-'
try {
    $runtimeDirectory = New-E2EPrivateDirectory (Join-Path $work 'runtime')
    New-Item -ItemType Directory -Force -Path (Join-Path $work 'config/Jiffshot') | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $work 'captures') | Out-Null
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        if ($RealPortal) {
            $runtimeDirectory = $env:XDG_RUNTIME_DIR
        }
        $environment = New-E2EEnvironment $server.Display $work $runtime -RuntimeDirectory $runtimeDirectory
        $reader = New-E2ETextReader
        $fixture = New-PresetFixture $binaryPath $work $portal $reader $environment $appLog
        $cases = if ($RealPortal) {
            @(
                'test_real_saved_preset_delivers_delayed_png_and_clipboard'
                'test_real_saved_preset_respects_disabled_clipboard'
            )
        }
        else {
            @(
                'test_screen_preset_save_replacement_and_independent_preferences'
                'test_screen_preset_invalid_input_does_not_overwrite'
                'test_screen_preset_survives_restart_and_can_be_deleted'
                'test_screen_preset_cursor_mode_and_normal_capture_are_independent'
                'test_screen_preset_chooser_cancellation_closes_session_without_saving'
                'test_screen_preset_cancel_during_chooser_is_honored_after_selection'
                'test_screen_preset_countdown_uses_saved_delay_and_can_be_cancelled'
                'test_screen_preset_invalid_stored_values_remain_unchanged'
                'test_screen_preset_removed_externally_cannot_start_capture'
            )
        }
        Invoke-GuiCases $cases $fixture
    }
    catch {
        if ($null -ne $fixture -and $null -ne $fixture.Driver) {
            Write-E2ETrace $fixture.Driver 'screen-presets-failure'
        }
        Write-E2ELogTail @($appLog, $xvfbLog)
        throw
    }
    finally {
        if ($null -ne $fixture) {
            Close-E2EX11Driver $fixture.Driver
            Stop-E2EProcess $fixture.App
        }
        if ($null -ne $reader) {
            $reader.Dispose()
        }
        if ($null -ne $server) { Stop-E2EProcess $server.Process }
    }
}
finally {
    Close-TimedPortal $portal
    Remove-E2EWorkDirectory $work
}
