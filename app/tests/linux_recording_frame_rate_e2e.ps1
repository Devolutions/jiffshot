#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify Linux recording-rate controls and entry points with a mock portal.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir,
    # Run one exact GUI test case.
    [string]$Case,
    [switch]$RealPortal,
    [string]$CaptureDir
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force

$FrameRateSettingKey = 'linux_recording_frame_rate'

function Format-E2EJson {
    param([AllowNull()]$Value)
    ConvertTo-Json -InputObject $Value -Depth 16 -Compress
}

#region Fixture

function New-RecordingRateFixture {
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
    param([Parameter(Mandatory)]$Fixture, [AllowNull()]$Preferences = $null, [string[]]$Arguments = @())
    if ($null -ne $Fixture.App) {
        Close-FixtureApp $Fixture
    }
    if ($null -ne $Preferences) {
        Write-E2EJson $Fixture.Settings $Preferences
    }
    $Fixture.App = Start-E2EProcess (@($Fixture.Binary) + $Arguments) $Fixture.Environment $Fixture.Work $Fixture.AppLog
    Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $Fixture.Environment) } 'the editor' 15
    $Fixture.Driver = New-E2EX11Driver $Fixture.Environment['DISPLAY'] (Get-E2EWindowInfo $Fixture.Environment)
    Wait-E2ESeconds 0.4
}

function Find-FixtureLabel {
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)][string]$Label)
    $position = Find-E2EScrollingLabel $Fixture.Text $Fixture.Driver $Label
    $position[0]
    $position[1]
}

function Invoke-FixtureLabelClick {
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)][string]$Label)
    $x, $y = Find-FixtureLabel $Fixture $Label
    $Fixture.Driver.Click($x, $y)
    Wait-E2ESeconds 0.2
}

function Wait-FixturePreferences {
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)]$Expected)
    Wait-E2EUntil { Test-E2EDeepEqual (Get-FixturePreferences $Fixture) $Expected } 'the expected saved preferences'
    $saved = Get-FixturePreferences $Fixture
    Assert-E2E (Test-E2EDeepEqual $saved $Expected) "$(Format-E2EJson $saved) != $(Format-E2EJson $Expected)"
}

function Select-FrameRate {
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)][int]$Index)
    $x, $y = Find-FixtureLabel $Fixture 'Save recording frame rate'
    if ($y -gt 210) {
        Invoke-E2EScroll $Fixture.Driver down 3
        Wait-E2ESeconds 0.25
        $x, $y = Find-FixtureLabel $Fixture 'Save recording frame rate'
    }
    Write-E2ETrace $Fixture.Driver 'frame-rate-before-selection'
    # Anchor on the save button: OCR can confuse the label with button text.
    $Fixture.Driver.Click($x - 185, $y)
    $Fixture.Driver.Press('Home')
    for ($step = 0; $step -lt $Index; $step++) {
        $Fixture.Driver.Press('Down')
    }
    $Fixture.Driver.Press('Return')
    Wait-E2ESeconds 0.25
    if ($null -eq (Find-E2ELabel $Fixture.Text $Fixture.Driver 'Save recording frame rate')) {
        Write-E2ETrace $Fixture.Driver 'frame-rate-selection-missing'
        $observed = $Fixture.Text.Recognize($Fixture.Driver.Capture(0, 0, 800, 270), 6, $false)
        throw [System.Exception]::new("AssertionError: frame-rate controls disappeared after selection: '$observed'")
    }
}

function Save-FrameRate {
    param([Parameter(Mandatory)]$Fixture, [Parameter(Mandatory)]$Expected)
    Invoke-FixtureLabelClick $Fixture 'Save recording frame rate'
    Wait-FixturePreferences $Fixture $Expected
    Wait-E2EStatus $Fixture.Text $Fixture.Driver 'Recording frame rate saved'
}

function Invoke-GuiCases {
    param([Parameter(Mandatory)][AllowEmptyCollection()][string[]]$Cases, [Parameter(Mandatory)]$Fixture)
    if ($Cases.Count -eq 0) {
        throw [System.Exception]::new('AssertionError: no screen preset GUI tests were selected')
    }
    $unique = @($Cases | Select-Object -Unique)
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

#region Cases

function Stop-PendingGuiRecording {
    param([Parameter(Mandatory)]$Fixture)
    $portal = $Fixture.Portal
    $session, $request = $portal.ScreenStarts[$portal.ScreenStarts.Count - 1]
    Invoke-FixtureLabelClick $Fixture 'Record screen'
    Wait-E2EUntil { $portal.ClosedRequests.Contains($request) } 'the pending request to close'
    Wait-E2EUntil { $portal.Closes.Contains($session) } 'the recording session to close'
}

function test_frame_rate_gui_saves_every_choice_and_restores_after_restart {
    param($Fixture)
    $initial = Copy-E2EMap $Fixture.Original @{ record_fps = 48; other = [ordered]@{ keep = @(1, $null, 'two') } }
    Start-FixtureApp $Fixture $initial
    $values = @('source', '15', '24', '30', '60')
    for ($index = 0; $index -lt $values.Count; $index++) {
        Select-FrameRate $Fixture $index
        Save-FrameRate $Fixture (Copy-E2EMap $initial @{ $FrameRateSettingKey = $values[$index] })
    }
    Write-E2ETrace $Fixture.Driver 'frame-rate-saved'

    Close-FixtureApp $Fixture
    Start-FixtureApp $Fixture
    $saved = Get-FixturePreferences $Fixture
    Assert-E2E (Test-E2EDeepEqual $saved (Copy-E2EMap $initial @{ $FrameRateSettingKey = '60' })) (Format-E2EJson $saved)
    # Change the file after the UI loaded; Save must use the restored selection,
    # not merely leave the existing file value untouched.
    Write-E2EJson $Fixture.Settings (Copy-E2EMap $initial @{ $FrameRateSettingKey = '15' })
    Save-FrameRate $Fixture (Copy-E2EMap $initial @{ $FrameRateSettingKey = '60' })
}

function test_frame_rate_gui_save_respects_the_shared_settings_lock {
    param($Fixture)
    $initial = Copy-E2EMap $Fixture.Original @{ record_fps = 144; $FrameRateSettingKey = '15' }
    Start-FixtureApp $Fixture $initial
    Select-FrameRate $Fixture 2
    $original = [System.IO.File]::ReadAllBytes($Fixture.Settings)
    $lockPath = Join-Path ([System.IO.Path]::GetDirectoryName($Fixture.Settings)) 'settings.json.schedule.lock'
    if (-not [System.IO.File]::Exists($lockPath)) {
        [System.IO.File]::WriteAllBytes($lockPath, [byte[]]@())
    }
    $lock = [JiffshotE2E.Posix]::TryLockExclusive($lockPath)
    Assert-E2E ($lock -ge 0) "could not lock $lockPath"
    try {
        Invoke-FixtureLabelClick $Fixture 'Save recording frame rate'
        Wait-E2EStatus $Fixture.Text $Fixture.Driver 'Could not save recording frame rate'
        Assert-E2E ([System.Linq.Enumerable]::SequenceEqual([System.IO.File]::ReadAllBytes($Fixture.Settings), $original)) `
            'a competing settings lock must prevent every preference write'
    }
    finally {
        [JiffshotE2E.Posix]::Close($lock)
    }
    Save-FrameRate $Fixture (Copy-E2EMap $initial @{ $FrameRateSettingKey = '24' })
}

function test_frame_rate_invalid_preference_blocks_normal_gui_recording {
    param($Fixture)
    $portal = $Fixture.Portal
    $initial = Copy-E2EMap $Fixture.Original @{ record_fps = 72; $FrameRateSettingKey = '30' }
    Start-FixtureApp $Fixture $initial
    foreach ($invalid in @('unknown', 30, $null)) {
        $bad = Copy-E2EMap $initial @{ $FrameRateSettingKey = $invalid }
        Write-E2EJson $Fixture.Settings $bad
        $before = [System.IO.File]::ReadAllBytes($Fixture.Settings)
        $previous = $portal.CaptureRequests
        Invoke-FixtureLabelClick $Fixture 'Record screen'
        Wait-E2EStatus $Fixture.Text $Fixture.Driver 'Could not read the recording frame rate'
        Assert-E2E ($portal.CaptureRequests -eq $previous) "invalid saved rate $(Format-E2EJson $invalid) must prevent a portal request"
        Assert-E2E ([System.Linq.Enumerable]::SequenceEqual([System.IO.File]::ReadAllBytes($Fixture.Settings), $before)) `
            "invalid saved rate $(Format-E2EJson $invalid) must not be rewritten"
    }

    Write-E2EJson $Fixture.Settings $initial
    $previous = $portal.ScreenStarts.Count
    Invoke-FixtureLabelClick $Fixture 'Record screen'
    Wait-E2EUntil { $portal.ScreenStarts.Count -eq $previous + 1 } 'normal recording with a valid saved rate'
    Stop-PendingGuiRecording $Fixture
    $saved = Get-FixturePreferences $Fixture
    Assert-E2E (Test-E2EDeepEqual $saved $initial) (Format-E2EJson $saved)
}

function test_frame_rate_command_line_recording_reads_saved_preference {
    param($Fixture)
    $portal = $Fixture.Portal
    $invalid = Copy-E2EMap $Fixture.Original @{ record_fps = 90; $FrameRateSettingKey = 'unsupported' }
    $previous = $portal.CaptureRequests
    Start-FixtureApp $Fixture $invalid @('--record')
    Wait-E2EStatus $Fixture.Text $Fixture.Driver 'Could not read the recording frame rate'
    Assert-E2E ($portal.CaptureRequests -eq $previous) 'an invalid saved rate must prevent a command-line portal request'
    $saved = Get-FixturePreferences $Fixture
    Assert-E2E (Test-E2EDeepEqual $saved $invalid) (Format-E2EJson $saved)

    Close-FixtureApp $Fixture
    $valid = Copy-E2EMap $invalid @{ $FrameRateSettingKey = '24' }
    $previous = $portal.ScreenStarts.Count
    Start-FixtureApp $Fixture $valid @('--record')
    Wait-E2EUntil { $portal.ScreenStarts.Count -eq $previous + 1 } 'command-line recording with a valid saved rate'
    Stop-PendingGuiRecording $Fixture
    $saved = Get-FixturePreferences $Fixture
    Assert-E2E (Test-E2EDeepEqual $saved $valid) (Format-E2EJson $saved)
}

function test_frame_rate_global_shortcut_reads_saved_preference {
    param($Fixture)
    $portal = $Fixture.Portal
    $initial = Copy-E2EMap $Fixture.Original @{
        record_fps                     = 120
        $FrameRateSettingKey                      = 'invalid'
        global_capture_shortcut        = 'CTRL+SHIFT+X'
        global_recording_shortcuts     = $true
        linux_global_recording_start   = 'silent'
        linux_global_shortcuts_enabled = $true
    }
    $previousBindings = $portal.Bindings.Count
    Start-FixtureApp $Fixture $initial
    Wait-E2EUntil { $portal.Bindings.Count -eq $previousBindings + 1 } 'the restored global recording shortcut'
    $session, $shortcuts = $portal.Bindings[$portal.Bindings.Count - 1]
    $shortcutIds = @($shortcuts | ForEach-Object { $_[0] })
    Assert-E2E ($shortcutIds -contains 'toggle-recording') "$($shortcutIds -join ', ')"
    Wait-E2EStatus $Fixture.Text $Fixture.Driver 'Global shortcuts enabled'
    $saved = Get-FixturePreferences $Fixture
    Assert-E2E ($saved[$FrameRateSettingKey] -ceq 'invalid' -and $saved['record_fps'] -eq 120) (Format-E2EJson $saved)

    $previous = $portal.CaptureRequests
    Send-PortalActivated $portal $session 'toggle-recording'
    Wait-E2EStatus $Fixture.Text $Fixture.Driver 'Could not read the recording frame rate'
    Assert-E2E ($portal.CaptureRequests -eq $previous) 'an invalid saved rate must prevent a shortcut portal request'

    $valid = Copy-E2EMap $saved @{ $FrameRateSettingKey = '60' }
    Write-E2EJson $Fixture.Settings $valid
    $previousStarts = $portal.ScreenStarts.Count
    Send-PortalActivated $portal $session 'toggle-recording'
    Wait-E2EUntil { $portal.ScreenStarts.Count -eq $previousStarts + 1 } 'global-shortcut recording with a valid saved rate'
    $recordingSession, $request = $portal.ScreenStarts[$portal.ScreenStarts.Count - 1]
    Send-PortalActivated $portal $session 'toggle-recording'
    Wait-E2EUntil { $portal.ClosedRequests.Contains($request) } 'shortcut stop to close request'
    Wait-E2EUntil { $portal.Closes.Contains($recordingSession) } 'shortcut stop to close recording session'
    $saved = Get-FixturePreferences $Fixture
    Assert-E2E (Test-E2EDeepEqual $saved $valid) (Format-E2EJson $saved)
}

function Complete-RealRecording {
    param([Parameter(Mandatory)]$Fixture)
    Wait-E2EStatus $Fixture.Text $Fixture.Driver 'Screen recording is active' 25
    Wait-E2ESeconds 1.3
    Invoke-FixtureLabelClick $Fixture 'Record screen'
    Wait-E2EStatus $Fixture.Text $Fixture.Driver 'Screen recording saved' 30
}

function Get-Recordings {
    param([Parameter(Mandatory)]$Fixture)
    , @([System.IO.Directory]::GetFiles($Fixture.Captures, 'Recording-*.mp4'))
}

function test_real_frame_rate_gui_and_command_line_recording_after_save {
    param($Fixture)
    $initial = Copy-E2EMap $Fixture.Original @{ record_fps = 48; $FrameRateSettingKey = '15' }
    $expected = Copy-E2EMap $initial @{ $FrameRateSettingKey = '30' }
    Start-FixtureApp $Fixture $initial
    Select-FrameRate $Fixture 3
    Save-FrameRate $Fixture $expected
    Invoke-FixtureLabelClick $Fixture 'Record screen'
    Complete-RealRecording $Fixture
    Assert-E2E ((Get-Recordings $Fixture).Count -eq 1) "expected one completed real recording, got $((Get-Recordings $Fixture) -join ', ')"
    Close-FixtureApp $Fixture

    Start-FixtureApp $Fixture -Arguments @('--record')
    Complete-RealRecording $Fixture
    $recordings = Get-Recordings $Fixture
    Assert-E2E ($recordings.Count -eq 2) "expected two completed real recordings, got $($recordings -join ', ')"
    Assert-E2E (@($recordings | Where-Object { (Get-Item -LiteralPath $_).Length -le 1000 }).Count -eq 0) `
        'both real recordings must contain media'
    $saved = Get-FixturePreferences $Fixture
    Assert-E2E (Test-E2EDeepEqual $saved $expected) (Format-E2EJson $saved)
}

#endregion

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$captureDirectory = if ($CaptureDir) { Resolve-E2EPath $CaptureDir } else { $null }
$portal = $null
if (-not $RealPortal) {
    # The recording request is held at ScreenCast.Start, so the PipeWire remote is never opened.
    $portal = New-MockPortal
    $portal.ScreenMode = 'fake-remote'
}
$server = $fixture = $reader = $null
$work = New-E2EWorkDirectory 'jiffshot-recording-rate-ui-'
try {
    $runtimeDirectory = New-E2EPrivateDirectory (Join-Path $work 'runtime')
    New-Item -ItemType Directory -Force -Path (Join-Path $work 'config/Jiffshot') | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $work 'captures') | Out-Null
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        if ($RealPortal) {
            $runtimeDirectory = $env:XDG_RUNTIME_DIR
        }
        $environment = New-E2EEnvironment $server.Display $work $runtime -RuntimeDirectory $runtimeDirectory
        $reader = New-E2ETextReader
        $fixture = New-RecordingRateFixture $binaryPath $work $portal $reader $environment $appLog
        if ($RealPortal) {
            if ($null -eq $captureDirectory) {
                throw [System.ArgumentException]::new('-RealPortal requires -CaptureDir')
            }
            $fixture.Captures = $captureDirectory
            $fixture.Original['capture_directory'] = $captureDirectory
            $cases = @('test_real_frame_rate_gui_and_command_line_recording_after_save')
        }
        else {
            $cases = @(
                'test_frame_rate_gui_saves_every_choice_and_restores_after_restart'
                'test_frame_rate_gui_save_respects_the_shared_settings_lock'
                'test_frame_rate_invalid_preference_blocks_normal_gui_recording'
                'test_frame_rate_command_line_recording_reads_saved_preference'
                'test_frame_rate_global_shortcut_reads_saved_preference'
            )
        }
        if ($Case) {
            $cases = @($cases | Where-Object { $_ -ceq $Case })
        }
        Invoke-GuiCases $cases $fixture
    }
    catch {
        if ($null -ne $fixture -and $null -ne $fixture.Driver) {
            Write-E2ETrace $fixture.Driver 'frame-rate-failure'
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
    Unregister-E2EService $portal
    Remove-E2EWorkDirectory $work
}
