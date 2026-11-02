#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Exercise the macOS control socket and CLI with isolated documents.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$Cli,
    [Parameter(Mandatory)][string]$HostLibrary,
    [switch]$DiscoverBundledHost,
    [switch]$ScreenCapture,
    [string]$Output
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacPackage.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacFixture.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacControl.psm1') -Force

function Control([string[]]$Arguments, [int]$Expected = 0) {
    return Invoke-ControlCommand -Cli $script:ControlCli -Environment $script:ControlEnvironment `
        -Arguments $Arguments -Expected $Expected
}

function Wait-ControlFixturePath([string]$Path, $Fixture, [int]$Seconds, [string]$Description) {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    while (-not [IO.File]::Exists($Path)) {
        if ($Fixture.Process.get_HasExited()) {
            $result = Wait-FixtureProcess $Fixture 5
            throw "$Description exited: $($result.Stderr)"
        }
        if ($clock.Elapsed.TotalSeconds -ge $Seconds) { throw "$Description did not appear" }
        Start-Sleep -Milliseconds 100
    }
}

function Assert-FixtureColor($Image, [int]$Channel, [int]$Minimum, [int]$OtherMaximum, [string]$Message) {
    $other = @(0, 1, 2 | Where-Object { $_ -ne $Channel } | ForEach-Object { $Image.Pixel[$_] })
    Assert-FixtureCondition ($Image.Pixel[$Channel] -ge $Minimum -and
        ($other | Measure-Object -Maximum).Maximum -le $OtherMaximum) $Message $Image.Pixel
}

function Invoke-ExternalWindow([string]$Executable, [string]$InfoPath, [string[]]$Options, [scriptblock]$Body) {
    $fixture = Start-FixtureProcess -File $Executable -Arguments (@($InfoPath) + $Options) `
        -Environment $script:ControlEnvironment
    try {
        Wait-ControlFixturePath $InfoPath $fixture 15 'the external window fixture'
        $info = Read-FixtureJson $InfoPath
        Assert-FixtureCondition ($info.pid -eq $fixture.Process.Id) 'the external window fixture returned the wrong PID' $info
        & $Body $info
    }
    finally { Stop-FixtureProcess $fixture }
}

function Invoke-ExternalWindowChecks([string]$Root, $OwnWindow) {
    $executable = Join-Path $Root 'control-window-fixture'
    [void](Invoke-MacCommand -File 'xcrun' -Arguments @(
        'clang', '-fobjc-arc', '-framework', 'Cocoa',
        (Join-Path $PSScriptRoot 'fixtures/control-window/Fixture.m'), '-o', $executable
    ))
    $infoPath = Join-Path $Root 'external-window.json'
    $externalId = $null
    Invoke-ExternalWindow $executable $infoPath @() {
        param($info)
        $script:ExternalWindowId = $info.window
        $matches = (Control @('windows', '--title', 'external control fixture')).windows
        Assert-FixtureCondition ($matches.Count -eq 1 -and $matches[0].hwnd -eq $info.window -and
            $matches[0].pid -eq $info.pid) 'the external window was not found' $matches
        $processMatches = (Control @('windows', '--process', [IO.Path]::GetFileName($executable))).windows
        Assert-FixtureCondition (@($processMatches | Where-Object { $_.hwnd -eq $info.window }).Count -gt 0) `
            'process filtering did not find the external window' $processMatches
        [void](Control @('focus', '--window', [string]$OwnWindow))
        $focused = Control @('focus', '--window', [string]$info.window)
        Assert-FixtureCondition ($focused.foreground -and $focused.window.pid -eq $info.pid) `
            'the external fixture was not focused' $focused
        $imagePath = Join-Path $Root 'external-window.png'
        $capture = Control @('screenshot', '--window', ('0x{0:x}' -f $info.window), '--out', $imagePath)
        Assert-FixtureCondition ($capture.target.window.pid -eq $info.pid) 'the wrong external window was captured' $capture
        Assert-FixtureColor (Get-FixturePngCenter $imagePath) 2 180 80 `
            'external window capture did not show the synthetic window'
    }
    $externalId = $script:ExternalWindowId
    $gone = Control @('wait', 'window', '--window', [string]$externalId, '--gone', '--timeout-ms', '5000')
    Assert-FixtureCondition $gone.gone 'the external window did not disappear' $gone

    Invoke-ExternalWindow $executable (Join-Path $Root 'minimized-window.json') @('--minimized') {
        param($info)
        $script:ExternalWindowId = $info.window
        [void](Control @('focus', '--window', [string]$OwnWindow))
        $listed = Control @('windows', '--window', [string]$info.window)
        $windows = $listed.windows
        Assert-FixtureCondition ($windows.Count -eq 1 -and $windows[0].pid -eq $info.pid -and
            $windows[0].minimized -and -not $windows[0].on_screen -and -not $listed.offscreen_omitted) `
            'the minimized fixture was not identified' $listed
        $byTitle = Control @('windows', '--title', 'external control fixture')
        Assert-FixtureCondition (Test-FixtureEqual @($byTitle.windows | ForEach-Object { $_.hwnd }) @($info.window)) `
            'title filtering did not find the minimized fixture' $byTitle
        $waiting = Control @('wait', 'window', '--window', [string]$info.window, '--timeout-ms', '0')
        Assert-FixtureCondition $waiting.window.minimized 'the minimized fixture was not considered present' $waiting
        $failure = Control @('wait', 'window', '--window', [string]$info.window, '--gone', '--timeout-ms', '0') 4
        Assert-FixtureCondition ($failure.code -ceq 'timeout') 'the minimized fixture was incorrectly considered gone' $failure
        $imagePath = Join-Path $Root 'minimized-no-activate.png'
        $failure = Control @('screenshot', '--window', [string]$info.window, '--no-activate', '--out', $imagePath) 1
        Assert-FixtureCondition ($failure.code -ceq 'conflict' -and -not [IO.File]::Exists($imagePath)) `
            'an off-screen window was captured without activation' $failure
        $stillMinimized = (Control @('windows', '--window', [string]$info.window)).windows
        Assert-FixtureCondition ($stillMinimized.Count -eq 1 -and $stillMinimized[0].minimized) `
            'the no-activate capture changed the minimized fixture' $stillMinimized
        $focused = Control @('focus', '--title', 'external control fixture')
        Assert-FixtureCondition ($focused.foreground -and $focused.window.hwnd -eq $info.window -and
            -not $focused.window.minimized -and $focused.window.on_screen) `
            'the minimized fixture was not restored and focused' $focused
        $imagePath = Join-Path $Root 'restored-window.png'
        $capture = Control @('screenshot', '--window', [string]$info.window, '--no-activate', '--out', $imagePath)
        Assert-FixtureCondition ($capture.target.window.pid -eq $info.pid) 'the restored fixture was not captured' $capture
        Assert-FixtureColor (Get-FixturePngCenter $imagePath) 2 180 80 'the restored fixture was not captured'
    }
    $gone = Control @('wait', 'window', '--window', [string]$script:ExternalWindowId, '--gone', '--timeout-ms', '5000')
    Assert-FixtureCondition $gone.gone 'the minimized fixture did not disappear' $gone

    Invoke-ExternalWindow $executable (Join-Path $Root 'hidden-window.json') @('--hidden') {
        param($info)
        [void](Control @('focus', '--window', [string]$OwnWindow))
        $listed = Control @('windows', '--window', [string]$info.window)
        $windows = $listed.windows
        Assert-FixtureCondition ($windows.Count -eq 1 -and $windows[0].pid -eq $info.pid -and
            -not $windows[0].minimized -and -not $windows[0].on_screen -and -not $listed.offscreen_omitted) `
            'the hidden fixture was not identified' $listed
        $focused = Control @('focus', '--window', [string]$info.window)
        Assert-FixtureCondition ($focused.foreground -and $focused.window.hwnd -eq $info.window -and
            -not $focused.window.minimized -and $focused.window.on_screen) `
            'the hidden fixture was not shown and focused' $focused
        $imagePath = Join-Path $Root 'hidden-restored-window.png'
        $capture = Control @('screenshot', '--window', [string]$info.window, '--no-activate', '--out', $imagePath)
        Assert-FixtureCondition ($capture.target.window.pid -eq $info.pid) 'the restored hidden fixture was not captured' $capture
        Assert-FixtureColor (Get-FixturePngCenter $imagePath) 2 180 80 'the restored hidden fixture was not captured'
    }

    Invoke-ExternalWindow $executable (Join-Path $Root 'ambiguous-window.json') @('--ambiguous') {
        param($info)
        [void](Control @('focus', '--window', [string]$OwnWindow))
        $listed = Control @('windows', '--window', [string]$info.window)
        Assert-FixtureCondition ($listed.windows.Count -eq 0 -and $listed.offscreen_omitted -eq 1 -and
            $listed.offscreen_error.Contains('unique')) 'the ambiguous off-screen window was not reported' $listed
        foreach ($arguments in @(
            @('focus', '--window', [string]$info.window),
            @('wait', 'window', '--window', [string]$info.window, '--gone', '--timeout-ms', '0')
        )) {
            $failure = Control $arguments 1
            Assert-FixtureCondition ($failure.code -ceq 'conflict' -and $failure.message.Contains('unique')) `
                'the ambiguous window was selected or treated as gone' $failure
        }
        $duplicate = (Control @('windows', '--window', [string]$info.duplicate)).windows
        Assert-FixtureCondition ($duplicate.Count -eq 1 -and $duplicate[0].on_screen) `
            'the visible duplicate was unexpectedly changed' $duplicate
    }
}

function Invoke-CaptureChecks([string]$Root, $Process) {
    $areaFile = Join-Path $Root 'control-capture.json'
    Wait-ControlFixturePath $areaFile $Process 15 'the synthetic control capture window'
    $fixture = Read-FixtureJson $areaFile
    $area = $fixture.rect
    $windowId = $fixture.window
    $focusWindowId = $fixture.focus_window
    $region = '{0},{1},{2},{3}' -f $area.x, $area.y, $area.width, $area.height
    Start-Sleep -Milliseconds 300
    $accessibility = Join-Path $Root 'control-element.json'
    Wait-ControlFixturePath $accessibility $Process 10 'the in-process Accessibility hit test'
    $element = Read-FixtureJson $accessibility
    Assert-FixtureCondition (-not $element.Contains('error') -and ($element.name -or $element.kind)) `
        'the in-process fixture was not described' $element
    $imagePath = Join-Path $Root 'screenshot.png'
    $captured = Control @('screenshot', '--region', $region, '--out', $imagePath, '--delay-ms', '100')
    Assert-FixtureCondition ((Test-FixtureEqual $captured.target @{ rect = $area; kind = 'region'; window = $null }) -and
        $captured.path -ceq $imagePath) 'the screenshot target or path is wrong' $captured
    $image = Get-FixturePngCenter $imagePath
    Assert-FixtureCondition ($image.Width -eq $captured.width -and $image.Height -eq $captured.height) `
        'the screenshot dimensions are wrong' $captured
    Assert-FixtureColor $image 0 180 80 'the screenshot did not capture the synthetic red window'

    $saved = Control @('screenshot', '--region', $region)
    Assert-FixtureCondition ((Test-FixtureChildPath $saved.path (Join-Path $Root 'library')) -and
        [IO.File]::Exists($saved.path)) 'the screenshot was not saved to the isolated library' $saved
    $metadata = Read-FixtureJson ([IO.Path]::ChangeExtension($saved.path, '.json'))
    Assert-FixtureCondition ($metadata.kind -ceq 'region' -and (Test-FixtureEqual $metadata.rect $area)) `
        'the library screenshot metadata is wrong' $metadata
    Assert-FixtureColor (Get-FixturePngCenter $saved.path) 0 180 80 'the library screenshot did not capture the fixture'
    $recognized = Control @('ocr', '--region', $region)
    $lines = $recognized.lines
    Assert-FixtureCondition ((Test-FixtureEqual $recognized.target $captured.target) -and
        @($lines | Where-Object { $_.text.ToLowerInvariant().Contains('jiffshot fixture') }).Count -gt 0) `
        'screen OCR did not recognize the synthetic text' $recognized
    $found = Control @('ocr', '--region', $region, '--find', 'FIXTURE')
    Assert-FixtureCondition ($found.matches.Count -eq 1 -and $found.matches[0].text.ToLowerInvariant().Contains('fixture')) `
        'screen OCR did not find the requested word' $found
    foreach ($line in @($lines) + @($found.matches)) {
        $box = $line.rect
        $center = @(($box.x + [Math]::Floor($box.width / 2)), ($box.y + [Math]::Floor($box.height / 2)))
        Assert-FixtureCondition ($box.width -gt 0 -and $box.height -gt 0 -and $box.x -ge $area.x -and
            $box.y -ge $area.y -and ($box.x + $box.width) -le ($area.x + $area.width) -and
            ($box.y + $box.height) -le ($area.y + $area.height) -and (Test-FixtureEqual $line.center $center)) `
            'screen OCR returned invalid desktop coordinates' $line
    }
    Assert-FixtureCondition ((Control @('ocr', '--region', $region, '--find', 'not present')).matches.Count -eq 0) `
        'screen OCR found text that is not present'
    $present = Control @('wait', 'text', 'Fixture', '--region', $region, '--timeout-ms', '5000')
    Assert-FixtureCondition (-not $present.gone -and $present.matches.Count -eq 1 -and
        (Test-FixtureEqual $present.target $captured.target)) 'waiting for visible text returned the wrong result' $present
    $absent = Control @('wait', 'text', 'not present', '--region', $region, '--gone', '--timeout-ms', '5000')
    Assert-FixtureCondition ($absent.gone -and $absent.matches.Count -eq 0) 'waiting for absent text returned the wrong result' $absent
    $failure = Control @('wait', 'text', 'not present', '--region', $region, '--timeout-ms', '0') 4
    Assert-FixtureCondition ($failure.code -ceq 'timeout') 'waiting for missing text did not time out' $failure

    $windows = (Control @('windows', '--window', ('0x{0:x}' -f $windowId))).windows
    Assert-FixtureCondition ($windows.Count -eq 1 -and $windows[0].hwnd -eq $windowId -and
        $windows[0].pid -eq $Process.Process.Id) 'window enumeration did not find the fixture' $windows
    Assert-FixtureCondition ($windows[0].title -ceq 'Jiffshot Control Fixture' -and $windows[0].process) `
        'window enumeration returned incomplete metadata' $windows[0]
    $ownElements = Control @('elements', '--window', [string]$windowId, '--no-activate')
    Assert-FixtureCondition ($ownElements.window.pid -eq $Process.Process.Id -and -not $ownElements.truncated -and
        $ownElements.elements.Count -ge 64) 'in-process Accessibility enumeration failed' $ownElements
    Assert-FixtureCondition (Test-FixtureEqual (Control @('windows', '--all', '--title', 'control fixture')).windows $windows) `
        'case-insensitive window title filtering failed'
    $waiting = Control @('wait', 'window', '--window', [string]$windowId, '--timeout-ms', '5000')
    Assert-FixtureCondition ($waiting.window.hwnd -eq $windowId) 'waiting for the synthetic window failed' $waiting
    Assert-FixtureCondition ((Control @('wait', 'window', '--window', '4294967295', '--gone', '--timeout-ms', '0')).gone) `
        'waiting for an absent window failed'
    Assert-FixtureCondition ((Control @('wait', 'window', '--window', '4294967295', '--timeout-ms', '0') 4).code -ceq 'timeout') `
        'waiting for a missing window did not time out'
    $guide = Control @('steps', 'start', '--title', 'Synthetic capture')
    $point = @{ x = $area.x + [Math]::Floor($area.width / 2); y = $area.y + [Math]::Floor($area.height / 2) }
    $added = Control @('steps', 'add', '--at', "$($point.x),$($point.y)", '--description', 'Synthetic fixture')
    $pointerStep = Control @('steps', 'add', '--description', 'Pointer fixture')
    $status = Control @('steps', 'status')
    $expectedSteps = @(
        @{ index = 1; description = 'Synthetic fixture'; at = $point }
        @{ index = 2; description = 'Pointer fixture'; at = $pointerStep.at }
    )
    Assert-FixtureCondition ($added.index -eq 1 -and (Test-FixtureEqual $added.at $point) -and
        $pointerStep.index -eq 2 -and $pointerStep.at.x -is [long] -and $pointerStep.at.y -is [long] -and
        $status.id -ceq $guide.id -and (Test-FixtureEqual $status.steps $expectedSteps) -and
        (Test-FixtureEqual (Control @('status')).steps $status)) 'the live guide steps were not retained' @($added, $pointerStep, $status)
    $cancelled = Control @('steps', 'cancel')
    Assert-FixtureCondition (Test-FixtureEqual $cancelled @{ id = $guide.id; cancelled = $true; steps = 2 }) `
        'the live guide steps were not discarded' $cancelled
    $imagePath = Join-Path $Root 'window.png'
    $capturedWindow = Control @('screenshot', '--window', [string]$windowId, '--no-activate', '--out', $imagePath)
    Assert-FixtureCondition ($capturedWindow.target.kind -ceq 'window' -and
        $capturedWindow.target.window.hwnd -eq $windowId) 'the window capture targeted the wrong window' $capturedWindow
    Assert-FixtureColor (Get-FixturePngCenter $imagePath) 0 180 80 'window capture did not capture the synthetic window'
    $windowOcr = Control @('ocr', '--window', [string]$windowId, '--no-activate', '--find', 'fixture')
    Assert-FixtureCondition ($windowOcr.matches.Count -gt 0 -and $windowOcr.target.window.hwnd -eq $windowId) `
        'window OCR did not recognize the synthetic text' $windowOcr
    Invoke-RecordingChecks $Root $region $image.Width $image.Height
    $focused = Control @('focus', '--window', [string]$focusWindowId)
    Assert-FixtureCondition ($focused.foreground -and $focused.window.hwnd -eq $focusWindowId) `
        'the synthetic window was not focused' $focused
    $imagePath = Join-Path $Root 'focused-window.png'
    $focusedCapture = Control @('screenshot', '--window', [string]$focusWindowId, '--out', $imagePath)
    Assert-FixtureCondition ($focusedCapture.target.window.hwnd -eq $focusWindowId) `
        'the focused capture targeted the wrong window' $focusedCapture
    Assert-FixtureColor (Get-FixturePngCenter $imagePath) 2 180 80 'focused capture did not capture the synthetic blue window'
    Invoke-ExternalWindowChecks $Root $focusWindowId
    return $region
}

function Invoke-RecordingChecks([string]$Root, [string]$Region, [int]$Width, [int]$Height) {
    $status = Control @('record', 'status')
    Assert-FixtureCondition (-not $status.active -and -not $status.finalizing -and -not $status.tray_recording) `
        'an unexpected recording was active' $status
    Assert-FixtureCondition (Test-FixtureEqual (Control @('status')).recording $status) `
        'top-level status did not include the idle recording state'
    Assert-FixtureCondition ((Control @('record', 'start', '--region', $Region, '--fps', '3') 2).code -ceq 'invalid_request') `
        'an invalid recording frame rate was accepted'
    $existing = Join-Path $Root 'existing-recording.mp4'
    [IO.File]::WriteAllText($existing, 'existing output')
    Assert-FixtureCondition ((Control @('record', 'start', '--region', $Region, '--out', $existing) 1).code -ceq 'conflict') `
        'an existing recording output was accepted'
    Assert-FixtureCondition ([IO.File]::ReadAllText($existing) -ceq 'existing output') `
        'recording startup changed an existing output'

    $path = Join-Path $Root 'control-recording.mp4'
    $started = Control @(
        'record', 'start', '--region', $Region, '--out', $path, '--fps', '15',
        '--audio', 'none', '--no-cursor', '--key', 'control-recording'
    )
    Assert-FixtureCondition ($started.active -and $started.frames -ge 1 -and $started.path -ceq $path -and
        -not [IO.File]::Exists($path)) 'the recording did not start in private staging' $started
    $recordingId = $started.id
    $aggregate = (Control @('status')).recording
    Assert-FixtureCondition ($aggregate.active -and $aggregate.id -ceq $recordingId -and $aggregate.path -ceq $path) `
        'top-level status did not include the active recording' $aggregate
    $retry = Control @('record', 'start', '--key', 'control-recording')
    Assert-FixtureCondition ($retry.id -ceq $recordingId -and $retry.already_started) `
        'the recording start retry was not idempotent' $retry
    Assert-FixtureCondition ((Control @('record', 'start', '--region', $Region) 1).code -ceq 'conflict') `
        'a second recording was accepted'
    Assert-FixtureCondition ((Control @('record', 'pause', '--id', 'missing') 1).code -ceq 'not_found') `
        'an incorrect recording id was accepted'
    Start-Sleep -Milliseconds 350
    $paused = Control @('record', 'pause', '--id', $recordingId)
    Assert-FixtureCondition $paused.paused 'the recording did not pause' $paused
    $aggregate = (Control @('status')).recording
    Assert-FixtureCondition ($aggregate.id -ceq $recordingId -and $aggregate.paused) `
        'top-level status did not include the paused recording' $aggregate
    Start-Sleep -Milliseconds 300
    $settled = Control @('record', 'status')
    Start-Sleep -Milliseconds 300
    $stillPaused = Control @('record', 'status')
    Assert-FixtureCondition ($stillPaused.frames -eq $settled.frames -and $stillPaused.duration_ms -eq $settled.duration_ms) `
        'frames or duration advanced while paused' @($settled, $stillPaused)
    $invalid = Control @('record', 'options', '--id', $recordingId, '--args', '{"clicks":true,"webcam_corner":"invalid"}') 2
    Assert-FixtureCondition ($invalid.code -ceq 'invalid_request' -and -not (Control @('record', 'status')).options.clicks) `
        'invalid recording options changed a live switch'
    $updated = Control @('record', 'options', '--id', $recordingId, '--args', '{"webcam_circle":true}')
    Assert-FixtureCondition $updated.options.webcam_circle 'the live recording option did not change' $updated
    Assert-FixtureCondition (-not (Control @('record', 'resume', '--id', $recordingId)).paused) 'the recording did not resume'

    $kind = ConvertTo-Json -InputObject @{
        type = 'Shape'
        rect = @{ x0 = 0; y0 = 0; x1 = $Width; y1 = $Height }
        shape = 'Rectangle'
    } -Compress
    $markAt = (Control @('record', 'status')).duration_ms
    $mark = Control @('record', 'annotate', '--id', $recordingId, '--kind', $kind, '--style', '{"fill":"#00FF00","width":0}')
    Assert-FixtureCondition ($mark.annotation_id -eq 1 -and $mark.coordinates -ceq 'recording pixels') `
        'the live annotation was not created' $mark
    Start-Sleep -Milliseconds 700
    $clearAt = (Control @('record', 'status')).duration_ms
    Assert-FixtureCondition (($clearAt - $markAt) -ge 300) 'the annotated recording did not advance'
    Assert-FixtureCondition ((Control @('record', 'clear', '--id', $recordingId)).cleared) 'the live annotation was not cleared'
    Start-Sleep -Milliseconds 500
    $result = Control @('record', 'stop', '--id', $recordingId)
    Assert-FixtureCondition ($result.path -ceq $path -and $result.frames -ge 3 -and
        $result.size_bytes -eq ([IO.FileInfo]$path).Length) 'the recording was not finalized' $result
    $info = Control @('video', 'info', '--path', $path)
    Assert-FixtureCondition ($info.width -eq $result.width -and $info.height -eq $result.height -and $info.duration -ge 1.0) `
        'the recorded MP4 has the wrong metadata' $info
    foreach ($frame in @(
        @{ at = 0.1; color = 'red'; channel = 0 }
        @{ at = ($markAt + $clearAt) / 2000; color = 'green'; channel = 1 }
        @{ at = $info.duration - 0.15; color = 'red'; channel = 0 }
    )) {
        $stamp = ([double]$frame.at).ToString('F2', [Globalization.CultureInfo]::InvariantCulture)
        $at = ([double]$frame.at).ToString('R', [Globalization.CultureInfo]::InvariantCulture)
        $imagePath = Join-Path $Root "recording-$($frame.color)-$stamp.png"
        [void](Control @('video', 'frame', '--path', $path, '--out', $imagePath, '--at', $at))
        Assert-FixtureColor (Get-FixturePngCenter $imagePath) $frame.channel 150 85 `
            "the $($frame.color) recording frame is wrong at $at"
    }
    $status = Control @('record', 'status')
    Assert-FixtureCondition (-not $status.active -and $status.last.id -ceq $recordingId) `
        'the finalized recording is not reflected in status' $status
    Assert-FixtureCondition (Test-FixtureEqual (Control @('status')).recording $status) `
        'top-level status did not include the finalized recording'

    $cancelledPath = Join-Path $Root 'cancelled-recording.mp4'
    $started = Control @('record', 'start', '--region', $Region, '--out', $cancelledPath, '--no-cursor')
    $cancelled = Control @('record', 'cancel', '--id', $started.id)
    Assert-FixtureCondition ($cancelled.cancelled -and -not $cancelled.Contains('recording_error') -and
        -not [IO.File]::Exists($cancelledPath)) 'the recording was not discarded' $cancelled
    Assert-FixtureCondition (@(Get-ChildItem -LiteralPath $Root -Filter '.jiffshot-recording-*' -Force).Count -eq 0) `
        'cancelling the recording left a staging directory'

    $limitedPath = Join-Path $Root 'limited-recording.mp4'
    $started = Control @(
        'record', 'start', '--region', $Region, '--out', $limitedPath, '--no-cursor', '--max-seconds', '0.25'
    )
    $clock = [Diagnostics.Stopwatch]::StartNew()
    do {
        $status = Control @('record', 'status')
        if ($status.ended) { break }
        if ($clock.Elapsed.TotalSeconds -ge 10) { throw "the limited recording did not stop: $($status | ConvertTo-Json -Compress)" }
        Start-Sleep -Milliseconds 100
    } while ($true)
    Assert-FixtureCondition ((Control @('record', 'pause') 1).code -ceq 'conflict') 'an ended recording accepted pause'
    $limited = Control @('record', 'stop', '--id', $started.id)
    Assert-FixtureCondition ($limited.duration_ms -ge 100 -and $limited.duration_ms -le 2000) `
        'the recording duration limit was not respected' $limited
    $started = Control @('record', 'start', '--region', $Region, '--no-cursor')
    Start-Sleep -Milliseconds 200
    $saved = Control @('record', 'stop', '--id', $started.id)
    Assert-FixtureCondition ((Test-FixtureChildPath $saved.path (Join-Path $Root 'library')) -and
        [IO.File]::Exists($saved.path)) 'the recording was not saved to the isolated library' $saved

    $collision = Join-Path $Root 'late-collision.mp4'
    $started = Control @('record', 'start', '--region', $Region, '--out', $collision, '--no-cursor')
    [IO.File]::WriteAllText($collision, 'late existing output')
    $failure = Control @('record', 'stop', '--id', $started.id) 1
    $staged = @(Get-ChildItem -LiteralPath $Root -Filter '.jiffshot-recording-*' -Directory -Force |
        ForEach-Object { Join-Path $_.FullName 'recording.mp4' } | Where-Object { [IO.File]::Exists($_) })
    Assert-FixtureCondition ($failure.code -ceq 'conflict' -and [IO.File]::ReadAllText($collision) -ceq 'late existing output' -and
        $staged.Count -eq 1 -and $failure.message.Contains($staged[0])) 'the collision did not preserve both files' $failure
    Assert-FixtureCondition ((Control @('video', 'info', '--path', $staged[0])).duration -gt 0) `
        'the staged recording from a collision was not recoverable'
}

function Invoke-VideoChecks([string]$Root, $Process) {
    Wait-ControlFixturePath (Join-Path $Root 'control-video-ready') $Process 15 'the synthetic control video'
    $source = Join-Path $Root 'control-video.mp4'
    $info = Control @('video', 'info', '--path', $source)
    Assert-FixtureCondition ($info.width -eq 64 -and $info.height -eq 48 -and
        (Test-FixtureEqual $info.audio @{ sample_rate = 48000; channels = 2 })) `
        'the synthetic video metadata is wrong' $info
    Assert-FixtureCondition ($info.duration -ge 0.95 -and $info.duration -le 1.05) `
        'the synthetic video duration is wrong' $info

    function Assert-VideoFrame([string]$Path, [string]$At, [string]$Name, [int]$Channel) {
        $imagePath = Join-Path $Root $Name
        $result = Control @('video', 'frame', '--path', $Path, '--out', $imagePath, '--at', $At)
        $image = Get-FixturePngCenter $imagePath
        Assert-FixtureCondition ($image.Width -eq $result.width -and $image.Height -eq $result.height) `
            "$Name has the wrong dimensions" $result
        Assert-FixtureColor $image $Channel 200 29 "$Name has the wrong video frame"
    }
    Assert-VideoFrame $source '0.1' 'control-video-red.png' 0
    Assert-VideoFrame $source '0.7' 'control-video-green.png' 1
    $trim = Join-Path $Root 'control-video-trim.mp4'
    $trimmed = Control @('video', 'export', '--path', $source, '--out', $trim, '--from', '0.25', '--to', '0.75')
    Assert-FixtureCondition ($trimmed.duration -ge 0.45 -and $trimmed.duration -le 0.55 -and
        (Test-FixtureEqual $trimmed.audio $info.audio)) 'the trimmed video lost duration or audio' $trimmed
    Assert-VideoFrame $trim '0.1' 'control-video-trim-red.png' 0
    Assert-VideoFrame $trim '0.4' 'control-video-trim-green.png' 1
    $gif = Join-Path $Root 'control-video.gif'
    $animation = Control @('video', 'export', '--path', $source, '--out', $gif, '--fps', '10')
    Assert-FixtureCondition ($animation.format -ceq 'gif' -and $animation.frames -eq 10 -and
        $animation.audio_note -ceq 'GIF cannot retain source audio') 'the GIF export metadata is wrong' $animation
    Assert-VideoFrame $gif '0.7' 'control-video-gif-green.png' 1
    $joined = Join-Path $Root 'control-video-joined.mp4'
    $segments = ConvertTo-Json -InputObject @(
        @{ path = $source; from = 0.5; to = 1.0 }
        @{ path = $source; from = 0.0; to = 0.5 }
    ) -Compress
    $output = Control @('video', 'join', '--out', $joined, '--segments', $segments)
    Assert-FixtureCondition ($output.duration -ge 0.95 -and $output.duration -le 1.05 -and
        (Test-FixtureEqual $output.audio $info.audio)) 'the joined video lost duration or audio' $output
    Assert-VideoFrame $joined '0.1' 'control-video-joined-green.png' 1
    Assert-VideoFrame $joined '0.6' 'control-video-joined-red.png' 0
}

function Invoke-ShutdownChecks([string]$Root, $Process, [string]$Socket, [string]$RecordingPath) {
    $videoOutput = Join-Path $Root 'cancelled-video.mp4'
    [IO.File]::WriteAllText($videoOutput, 'previous output')
    $segments = ConvertTo-Json -InputObject @(
        for ($index = 0; $index -lt 256; $index++) { @{ path = Join-Path $Root 'control-video.mp4' } }
    ) -Compress
    $children = [Collections.Generic.List[object]]::new()
    try {
        $exporting = Start-FixtureProcess -File $script:ControlCli -Arguments @(
            '--no-launch', 'video', 'join', '--out', $videoOutput, '--segments', $segments, '--fps', '120', '--mute'
        ) -Environment $script:ControlEnvironment
        $children.Add($exporting)
        $clock = [Diagnostics.Stopwatch]::StartNew()
        while (@(Get-ChildItem -LiteralPath $Root -Filter '.jiffshot-export-*' -Force).Count -eq 0) {
            if ($exporting.Process.get_HasExited()) {
                $result = Wait-FixtureProcess $exporting 5
                throw "the long video export exited before starting: $($result.Stdout); $($result.Stderr)"
            }
            if ($clock.Elapsed.TotalSeconds -ge 15) { throw 'the long video export did not start within 15 seconds' }
            Start-Sleep -Milliseconds 50
        }
        $delayed = Start-FixtureProcess -File $script:ControlCli -Arguments @(
            '--no-launch', 'screenshot', '--region', '2147483647,0,1,1', '--delay-ms', '3600000'
        ) -Environment $script:ControlEnvironment
        $children.Add($delayed)
        Start-Sleep -Milliseconds 300
        if ($delayed.Process.get_HasExited()) {
            $result = Wait-FixtureProcess $delayed 5
            throw "the delayed capture exited prematurely: $($result.Stdout); $($result.Stderr)"
        }
        Assert-FixtureCondition (Test-FixtureEqual (Control @('quit')) @{ quitting = $true }) `
            'the CLI did not acknowledge the graceful exit request'
        $result = Wait-FixtureProcess $delayed 10
        $response = ConvertFrom-Json -InputObject $result.Stdout -AsHashtable -Depth 100
        Assert-FixtureCondition ($result.ExitCode -eq 1 -and $response.error.code -ceq 'conflict') `
            'shutdown did not cancel the delayed capture' @($result.Stdout, $result.Stderr)
        $result = Wait-FixtureProcess $exporting 10
        $response = ConvertFrom-Json -InputObject $result.Stdout -AsHashtable -Depth 100
        Assert-FixtureCondition ($result.ExitCode -eq 1 -and $response.error.code -ceq 'conflict') `
            'shutdown did not cancel the video export' @($result.Stdout, $result.Stderr)
        Assert-FixtureCondition ([IO.File]::ReadAllText($videoOutput) -ceq 'previous output' -and
            @(Get-ChildItem -LiteralPath $Root -Filter '.jiffshot-export-*' -Force).Count -eq 0) `
            'the cancelled video export changed the previous output or left staging files'
        $result = Wait-FixtureProcess $Process 45
        Assert-FixtureCondition ($result.ExitCode -eq 0) 'Jiffshot exited unsuccessfully' $result.ExitCode
        Assert-FixtureCondition (-not [IO.File]::Exists($Socket)) 'the control socket survived graceful shutdown'
        if ($RecordingPath) {
            Assert-FixtureCondition ([IO.File]::Exists($RecordingPath) -and ([IO.FileInfo]$RecordingPath).Length -gt 0) `
                'shutdown did not publish the active CLI recording'
            $reopened = Start-FixtureProcess -File $Process.File -Environment $script:ControlEnvironment `
                -LogPath (Join-Path $Root 'control-reopen.log')
            try {
                [void](Wait-ControlServer $script:ControlCli $script:ControlEnvironment $reopened)
                $info = Control @('video', 'info', '--path', $RecordingPath)
                Assert-FixtureCondition ($info.duration -gt 0 -and $info.width -gt 0 -and $info.height -gt 0) `
                    'the shutdown recording could not be decoded' $info
                $imagePath = Join-Path $Root 'shutdown-recording-frame.png'
                [void](Control @('video', 'frame', '--path', $RecordingPath, '--out', $imagePath, '--at', '0'))
                Assert-FixtureColor (Get-FixturePngCenter $imagePath) 0 150 85 `
                    'the shutdown recording did not capture the synthetic window'
                [void](Control @('quit'))
                $result = Wait-FixtureProcess $reopened 45
                Assert-FixtureCondition ($result.ExitCode -eq 0) 'the verification app exited unsuccessfully' $result.ExitCode
            }
            finally { Stop-FixtureProcess $reopened }
        }
    }
    finally {
        foreach ($child in $children) { Stop-FixtureProcess $child }
    }
}

function Invoke-StepsChecks {
    $idle = @{ active = $false }
    Assert-FixtureCondition (Test-FixtureEqual (Control @('steps', 'status')) $idle) 'an unexpected step guide was active'
    Assert-FixtureCondition (Test-FixtureEqual (Control @('status')).steps $idle) `
        'top-level status did not include the idle guide state'
    $started = Control @('steps', 'start', '--title', 'Control guide')
    Assert-FixtureCondition ($started.active -and $started.title -ceq 'Control guide' -and $started.steps.Count -eq 0) `
        'the step guide did not start' $started
    Assert-FixtureCondition (Test-FixtureEqual (Control @('status')).steps $started) `
        'top-level status did not include the active guide'
    Assert-FixtureCondition ((Control @('steps', 'start') 1).code -ceq 'conflict') `
        'a second step guide did not report a conflict'
    Assert-FixtureCondition ((Control @('steps', 'finish') 1).code -ceq 'conflict') `
        'an empty step guide did not report a conflict'
    $status = Control @('steps', 'status')
    Assert-FixtureCondition ($status.id -ceq $started.id) 'the empty guide was not retained' $status
    $cancelled = Control @('steps', 'cancel')
    Assert-FixtureCondition (Test-FixtureEqual $cancelled @{ id = $started.id; cancelled = $true; steps = 0 }) `
        'the step guide did not cancel' $cancelled
    Assert-FixtureCondition (Test-FixtureEqual (Control @('status')).steps $idle) `
        'top-level status did not include the cancelled guide'
}

function Invoke-ControlChecks([string]$Root, $Process, [bool]$Capture) {
    $status = Wait-ControlServer $script:ControlCli $script:ControlEnvironment $Process
    $required = @(
        'doc.draw', 'quit', 'windows', 'elements', 'focus', 'wait.window', 'screenshot', 'capture.scroll',
        'cursor', 'ocr', 'wait.text', 'click', 'move', 'drag', 'scroll', 'type', 'key', 'video.info',
        'video.frame', 'video.export', 'video.join', 'steps.start', 'steps.add', 'steps.finish',
        'steps.cancel', 'record.start', 'record.stop', 'record.annotate'
    )
    $missing = @($required | Where-Object { -not $status.commands.Contains($_) })
    Assert-FixtureCondition ($status.pid -eq $Process.Process.Id -and $missing.Count -eq 0) `
        'unexpected control status or missing commands' $status
    Assert-FixtureCondition ((Test-FixtureEqual $status.steps @{ active = $false }) -and
        -not $status.recording.active -and -not $status.recording.finalizing -and -not $status.recording.tray_recording) `
        'unexpected initial recording or guide status' $status
    $socket = $status.pipe
    $displays = Control @('monitors')
    Assert-FixtureCondition ($displays.monitors.Count -gt 0) 'no displays were reported'
    $pointer = Control @('cursor')
    Assert-FixtureCondition ($pointer.x -is [long] -and $pointer.y -is [long]) 'the pointer position is invalid' $pointer
    Assert-FixtureCondition ((Control @('screenshot', '--region', '2147483647,0,1,1') 2).code -ceq 'invalid_request') `
        'an overflowing region was not rejected'
    Assert-FixtureCondition ((Control @('screenshot', '--screen', '99999999') 1).code -ceq 'not_found') `
        'an unknown display was not reported'
    Assert-FixtureCondition ((Control @('screenshot', '--window', 'not-a-window') 2).code -ceq 'invalid_request') `
        'an invalid window number was not rejected'
    $region = if ($Capture) { Invoke-CaptureChecks $Root $Process } else { $null }
    Invoke-VideoChecks $Root $Process
    Invoke-StepsChecks
    $document = Control @('doc', 'new', '--width', '120', '--height', '90')
    $documentId = $document.id
    $drawn = Control @(
        'doc', 'draw', '--id', $documentId, '--tool', 'Arrow',
        '--from', '10,10', '--to', '90,60', '--style', '{"stroke":"#0000FF"}'
    )
    Assert-FixtureCondition ($drawn.objects.Count -eq 1) 'the annotation was not created' $drawn
    $undone = Control @('doc', 'action', 'undo', '--id', $documentId)
    $redone = Control @('doc', 'action', 'redo', '--id', $documentId)
    Assert-FixtureCondition ($undone.objects.Count -eq 0 -and $redone.objects.Count -eq 1) `
        'document undo/redo did not preserve the annotation'
    $editable = Join-Path $Root 'editable.jsd'
    $imagePath = Join-Path $Root 'export.png'
    [void](Control @('doc', 'save', '--id', $documentId, '--out', $editable))
    [void](Control @('doc', 'export', '--id', $documentId, '--out', $imagePath))
    $manifest = Read-FixtureZipJson $editable 'document.json'
    Assert-FixtureCondition ($manifest.width -eq 120 -and $manifest.height -eq 90 -and $manifest.objects.Count -eq 1) `
        'the saved document did not preserve its dimensions and annotation'
    $exportedImage = Get-FixturePngCenter $imagePath
    Assert-FixtureCondition ($exportedImage.Width -eq 120 -and $exportedImage.Height -eq 90) `
        'the exported PNG has the wrong dimensions'
    $reopened = Control @('doc', 'open', $editable)
    Assert-FixtureCondition ($reopened.objects.Count -eq 1) 'reopening the editable document lost its annotation'
    [void](Control @('doc', 'close', '--id', $reopened.id))
    [void](Control @('doc', 'close', '--id', $documentId))
    Assert-FixtureCondition ((Control @('doc', 'list')).documents.Count -eq 0) 'closed document sessions are still listed'

    $scriptPath = Join-Path $Root 'script.json'
    $scenario = @{
        steps = @(
            @{ command = 'doc.new'; args = @{ width = 20; height = 20 }; save_as = 'document' }
            @{ command = 'doc.draw'; args = @{
                id = @{ '$ref' = 'document.id' }; tool = 'Shape'; from = '1,1'; to = '12,12'
            } }
        )
        finally = @(@{ command = 'doc.close'; args = @{ id = @{ '$ref' = 'document.id' } } })
    }
    [IO.File]::WriteAllText($scriptPath, (ConvertTo-Json -InputObject $scenario -Depth 30 -Compress))
    $report = Control @('run', $scriptPath)
    Assert-FixtureCondition ($report.steps.Count -eq 2 -and
        @(@($report.steps) + @($report.finally) | Where-Object { -not $_.ok }).Count -eq 0) `
        'the scripted document sequence failed' $report
    Assert-FixtureCondition ((Control @('video', 'info', '--path', (Join-Path $Root 'missing.mp4')) 1).code -ceq 'not_found') `
        'an unavailable video did not report not_found'
    Assert-FixtureCondition ((Control @('call', 'unavailable.command') 3).code -ceq 'unsupported') `
        'an unsupported macOS command did not report its limitation'
    Assert-FixtureCondition ((Control @('doc', 'status', '--id', 'missing') 1).code -ceq 'not_found') `
        'an unknown document did not report not_found'
    Assert-FixtureCondition ((Control @('open', (Join-Path $Root 'missing.png')) 1).code -ceq 'not_found') `
        'an unavailable file did not report not_found'
    Assert-FixtureCondition (Test-FixtureEqual (Control @('open', $imagePath)) @{ opened = $imagePath }) `
        'the CLI did not queue the exported image'
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $logPath = Join-Path $Root 'process.log'
    while (-not [IO.File]::ReadAllText($logPath).Contains('control fixture: forwarded file opened after an editor-busy retry')) {
        if ($Process.Process.get_HasExited()) { throw 'Jiffshot exited before opening the forwarded image' }
        if ($clock.Elapsed.TotalSeconds -ge 15) { throw 'the forwarded image was not opened after the editor-busy retry' }
        Start-Sleep -Milliseconds 100
    }
    $recordingPath = $null
    if ($region) {
        $recordingPath = Join-Path $Root 'shutdown-recording.mp4'
        [void](Control @('record', 'start', '--region', $region, '--out', $recordingPath, '--no-cursor'))
    }
    Invoke-ShutdownChecks $Root $Process $socket $recordingPath
}

$binaryPath = Resolve-FixtureFile $Binary
$script:ControlCli = Resolve-FixtureFile $Cli
$hostPath = Resolve-FixtureFile $HostLibrary
if ($DiscoverBundledHost) {
    $bundleContents = [IO.Path]::GetDirectoryName([IO.Path]::GetDirectoryName($binaryPath))
    $expectedHost = Join-Path $bundleContents 'Frameworks/librustolonia_host.dylib'
    if ($hostPath -cne $expectedHost) { throw '-DiscoverBundledHost requires the host alongside the selected app bundle' }
}
$root = New-FixtureRoot $Output 'jiffshot-control-'
try {
    Initialize-FixtureDirectories $root
    $script:ControlEnvironment = New-FixtureEnvironment $root $hostPath 'JIFFSHOT_UITEST_CONTROL'
    $script:ControlEnvironment['JIFFSHOT_UITEST_KEEP'] = '1'
    $script:ControlEnvironment['JIFFSHOT_UITEST_CONTROL_REENTRANT'] = '1'
    $script:ControlEnvironment['JIFFSHOT_UITEST_SHUTDOWN_POST_FAILURES'] = '1'
    $script:ControlEnvironment['JIFFSHOT_UITEST_SHUTDOWN_POST_TARGET'] = 'an exit request'
    if ($ScreenCapture) { $script:ControlEnvironment['JIFFSHOT_UITEST_CONTROL_CAPTURE'] = '1' }
    if ($DiscoverBundledHost) {
        foreach ($key in @($script:ControlEnvironment.Keys)) {
            if ($key.StartsWith('RUSTOLONIA_', [StringComparison]::Ordinal) -or
                $key.StartsWith('DYLD_', [StringComparison]::Ordinal)) {
                $script:ControlEnvironment.Remove($key)
            }
        }
    }
    $appEnvironment = $script:ControlEnvironment.Clone()
    if ($DiscoverBundledHost) { $appEnvironment['DYLD_PRINT_LIBRARIES'] = '1' }
    $process = Start-FixtureProcess -File $binaryPath -Environment $appEnvironment -LogPath (Join-Path $root 'process.log')
    try { Invoke-ControlChecks $root $process ([bool]$ScreenCapture) }
    finally { Stop-FixtureProcess $process }
    $uiLog = [IO.File]::ReadAllText((Join-Path $root 'ui-log.txt'))
    Assert-FixtureCondition (-not $uiLog.Contains('FAILED:')) 'the UI fixture reported a failure'
    $processLog = [IO.File]::ReadAllText((Join-Path $root 'process.log'))
    Assert-FixtureCondition ($processLog.Contains('queued an exit request after retrying the UI dispatcher')) `
        'the exit-request dispatcher retry was not exercised'
    if ($DiscoverBundledHost) {
        Assert-FixtureCondition ($processLog.Contains($hostPath)) 'the application did not load the staged bundle host'
    }
}
catch {
    [Console]::Error.WriteLine("$($_.Exception.Message)`nFixture retained at $root")
    throw
}
if ($Output) { Write-Host "Control fixture passed: $root" }
else { Remove-FixtureRoot $root; Write-Host 'Control fixture passed' }
