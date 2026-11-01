#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Exercise global shortcut controls with a private Xvfb and mock portal.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force

function Test-EmptyDirectory {
    param([Parameter(Mandatory)][string]$Path)
    @(Get-ChildItem -LiteralPath $Path -Force).Count -eq 0
}

function Test-GlobalRecordingStartRoutesSelectedAudioAndStops {
    param($Portal, [string]$Session, [JiffshotE2E.X11Driver]$Driver, [JiffshotE2E.TextReader]$Reader, [string]$Captures)
    $previousRequests = $Portal.CaptureRequests
    Send-PortalActivated $Portal "${Session}_foreign" 'toggle-recording'
    Wait-E2ESeconds 0.4
    Assert-E2E ($Portal.CaptureRequests -eq $previousRequests) 'a foreign shortcut session must not start recording'
    $previousStarts = $Portal.ScreenStarts.Count
    Send-PortalActivated $Portal $Session 'toggle-recording'
    Wait-E2EUntil { $Portal.ScreenStarts.Count -eq $previousStarts + 1 } 'the global recording start to request a source'
    Wait-E2EStatus $Reader $Driver 'Requesting permission for screen and microphone recording'
    $recordingSession, $recordingRequest = $Portal.ScreenStarts[-1]
    Send-PortalActivated $Portal $Session 'toggle-recording'
    Wait-E2EUntil { $Portal.ClosedRequests.Contains($recordingRequest) } 'the recording shortcut to stop a pending recording'
    Wait-E2EUntil { $Portal.Closes.Contains($recordingSession) } 'the stopped recording session to close'
    Wait-E2EStatus $Reader $Driver 'Screen and microphone recording cancelled'
    Assert-E2E (Test-EmptyDirectory $Captures) 'stopping a pending recording must not create a capture'
}

function Get-ShortcutIds {
    param([object[]]$Shortcuts)
    , @(foreach ($shortcut in $Shortcuts) { $shortcut[0] })
}

function Get-PreferredTriggers {
    param([object[]]$Shortcuts)
    , @(foreach ($shortcut in $Shortcuts) { $shortcut[1]['preferred_trigger'].Value })
}

function Test-StrictBool {
    param([AllowNull()]$Value, [bool]$Expected)
    $Value -is [bool] -and $Value -eq $Expected
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$portal = New-MockPortal
$app = $server = $driver = $null
$work = New-E2EWorkDirectory 'jiffshot-shortcut-ui-'
try {
    $captures = (New-Item -ItemType Directory -Path (Join-Path $work 'captures')).FullName
    $config = (New-Item -ItemType Directory -Force -Path (Join-Path $work 'config/Jiffshot')).FullName
    New-E2EPrivateDirectory (Join-Path $work 'runtime') | Out-Null
    $settings = Join-Path $config 'settings.json'
    Write-E2EJson $settings ([ordered]@{ capture_directory = $captures; theme = 'Light' })
    $document = Join-Path $work 'selection.jsd'
    New-E2ESelectionDocument $document
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $displayName = $server.Display
        $environment = New-E2EEnvironment $displayName $work $runtime
        $app = Start-E2EProcess @($binaryPath, $document) $environment $work $appLog
        $window = $null
        for ($attempt = 0; $attempt -lt 150; $attempt++) {
            if ($app.HasExited) {
                throw "application exited during startup: $($app.ExitCode)"
            }
            $window = Get-E2EWindowInfo $environment
            if ($null -ne $window) {
                break
            }
            Wait-E2ESeconds 0.1
        }
        if ($null -eq $window) {
            throw 'application window did not appear'
        }
        $driver = New-E2EX11Driver $displayName $window
        Wait-E2ESeconds 1

        Wait-E2EUntil { $driver.RedCanvasPixels() -gt 100 } 'the annotations to render'
        $baseline = $driver.RedCanvasPixels()
        $driver.Click(20, 400)
        $driver.SelectAll()
        Wait-E2ESeconds 0.3
        $selectedBaseline = $driver.RedCanvasPixels()
        Assert-E2E ($selectedBaseline -gt 100) 'selecting annotations hid the fixture'
        Set-E2EShortcutText $driver 'CTRL+ALT+P'
        Wait-E2ESeconds 0.3
        $afterTextEdit = $driver.RedCanvasPixels()
        Assert-E2E ($afterTextEdit -ge $selectedBaseline * 0.8) "editing shortcut text deleted selected annotations: before=$selectedBaseline, after=$afterTextEdit"
        $driver.Click(20, 400)
        $driver.SelectAll()
        $driver.Key('Delete', $true)
        $driver.Key('Delete', $false)
        $driver.Sync()
        Wait-E2EUntil { $driver.RedCanvasPixels() -lt $baseline * 0.1 } 'Delete to remove the selected annotations'
        $driver.Key('Control_L', $true)
        $driver.Key('z', $true)
        $driver.Key('z', $false)
        $driver.Key('Control_L', $false)
        $driver.Sync()
        Wait-E2EUntil { $driver.RedCanvasPixels() -ge $selectedBaseline * 0.8 } 'Undo to restore the deleted annotations'
        Write-E2ELine 'GUI selection and Delete preserve text editing and are undoable'
        $driver.Click(460, 80)
        Wait-E2EUntil { $portal.Bindings.Count -eq 1 } 'the initial shortcut binding'
        $shortcutSession, $boundShortcuts = $portal.Bindings[0]
        Assert-E2E ($boundShortcuts[0][0] -ceq 'capture') "unexpected shortcuts: $(Get-ShortcutIds $boundShortcuts)"
        Assert-E2E ($boundShortcuts[0][1]['preferred_trigger'].Value -ceq 'CTRL+ALT+P') "unexpected triggers: $(Get-PreferredTriggers $boundShortcuts)"
        $saved = Read-E2EJson $settings
        $savedText = { ConvertTo-Json -InputObject $saved -Depth 16 -Compress }
        Assert-E2E ($saved['global_capture_shortcut'] -ceq 'CTRL+ALT+P') (& $savedText)
        Assert-E2E (Test-StrictBool $saved['global_recording_shortcuts'] $false) (& $savedText)
        Assert-E2E (Test-StrictBool $saved['linux_global_shortcuts_enabled'] $true) (& $savedText)
        Assert-E2E ($saved['capture_directory'] -ceq $captures -and $saved['theme'] -ceq 'Light') (& $savedText)
        Wait-E2ESeconds 0.5
        Send-PortalActivated $portal "${shortcutSession}_foreign" 'capture'
        Send-PortalActivated $portal $shortcutSession 'other'
        Send-PortalActivated $portal $shortcutSession 'toggle-recording'
        Wait-E2ESeconds 0.5
        Assert-E2E ($portal.CaptureRequests -eq 0) 'foreign shortcut events triggered capture'
        Send-PortalActivated $portal $shortcutSession 'capture'
        Wait-E2EUntil { $portal.CaptureRequests -eq 1 } 'capture shortcut activation'

        $portal.ScreenMode = 'hold-start'
        $previous = $portal.ScreenStarts.Count
        $driver.Click(300, 45)
        Wait-E2EUntil { $portal.ScreenStarts.Count -eq $previous + 1 } 'the capture-only recording request'
        $recordingSession, $recordingRequest = $portal.ScreenStarts[-1]
        Send-PortalActivated $portal $shortcutSession 'pause-recording'
        Send-PortalActivated $portal $shortcutSession 'stop-recording'
        Wait-E2ESeconds 0.5
        Assert-E2E (-not $portal.ClosedRequests.Contains($recordingRequest)) 'recording shortcuts were active without opt-in'
        $driver.Click(560, 140)
        Wait-E2EUntil { $portal.ClosedRequests.Contains($recordingRequest) } 'the capture-only recording request to cancel'
        Wait-E2EUntil { $portal.Closes.Contains($recordingSession) } 'the capture-only recording session to close'

        $driver.Click(670, 80)
        Wait-E2EUntil { $portal.Closes.Contains($shortcutSession) } 'the active session to close'
        Assert-E2E (Test-StrictBool (Read-E2EJson $settings)['linux_global_shortcuts_enabled'] $false) 'disabling shortcuts must save linux_global_shortcuts_enabled=false'
        Wait-E2ESeconds 0.4
        Write-E2ELine 'GUI saved the preferred trigger, filtered events, captured and disabled'

        $portal.Mode = 'hold'
        $driver.Click(460, 80)
        Wait-E2EUntil { $portal.Bindings.Count -eq 2 } 'a pending shortcut chooser'
        $pending = $portal.Bindings[1][0]
        $driver.Click(670, 80)
        Wait-E2EUntil { $portal.Closes.Contains($pending) } 'the pending session to close'
        Wait-E2ESeconds 0.4
        Write-E2ELine 'GUI cancelled a pending binding and closed its portal session'

        $portal.Mode = 'reject'
        $driver.Click(460, 80)
        Wait-E2EUntil { $portal.Bindings.Count -eq 3 } 'a rejected shortcut request'
        $rejected = $portal.Bindings[2][0]
        Wait-E2EUntil { $portal.Closes.Contains($rejected) } 'the rejected session to close'
        Wait-E2ESeconds 0.4
        Write-E2ELine 'Rejected shortcut request closed its portal session'

        $portal.Mode = 'success'
        $driver.Click(460, 80)
        Wait-E2EUntil { $portal.Bindings.Count -eq 4 } 'a second accepted shortcut'
        $remote = $portal.Bindings[3][0]
        Wait-E2ESeconds 0.4
        Send-PortalSessionClosed $portal $remote
        Wait-E2ESeconds 0.5
        $driver.Click(460, 80)
        Wait-E2EUntil { $portal.Bindings.Count -eq 5 } 're-enabling after desktop closure'
        Assert-E2E (-not $portal.Closes.Contains($remote)) 'an already-closed session was closed twice'
        $last = $portal.Bindings[4][0]
        Wait-E2ESeconds 0.4
        $driver.Click(670, 80)
        Wait-E2EUntil { $portal.Closes.Contains($last) } 'the final session to close'
        Write-E2ELine 'Desktop closure allowed the shortcut to be enabled again'

        Wait-E2ESeconds 0.4
        $driver.Click(20, 175)
        $driver.Click(460, 80)
        Wait-E2EUntil { $portal.Bindings.Count -eq 6 } 'recording shortcut bindings'
        $recordingShortcutsSession, $boundShortcuts = $portal.Bindings[-1]
        Assert-E2E (Test-E2EDeepEqual (Get-ShortcutIds $boundShortcuts) @('capture', 'pause-recording', 'stop-recording')) "unexpected shortcuts: $(Get-ShortcutIds $boundShortcuts)"
        Assert-E2E (Test-E2EDeepEqual (Get-PreferredTriggers $boundShortcuts) @('CTRL+ALT+P', 'SHIFT+F9', 'SHIFT+F10')) "unexpected triggers: $(Get-PreferredTriggers $boundShortcuts)"
        $saved = Read-E2EJson $settings
        Assert-E2E (Test-StrictBool $saved['global_recording_shortcuts'] $true) (& $savedText)
        Assert-E2E ($saved['linux_global_recording_start'] -ceq 'disabled') (& $savedText)
        Wait-E2ESeconds 0.4
        $previous = $portal.CaptureRequests
        Send-PortalActivated $portal $recordingShortcutsSession 'toggle-recording'
        Wait-E2ESeconds 0.4
        Assert-E2E ($portal.CaptureRequests -eq $previous) 'recording controls must not enable the separate start shortcut by default'

        $portal.ScreenMode = 'hold-start'
        $previous = $portal.ScreenStarts.Count
        $driver.Click(300, 45)
        Wait-E2EUntil { $portal.ScreenStarts.Count -eq $previous + 1 } 'the recording-shortcut test request'
        $recordingSession, $recordingRequest = $portal.ScreenStarts[-1]
        Send-PortalActivated $portal "${recordingShortcutsSession}_foreign" 'stop-recording'
        Send-PortalActivated $portal $recordingShortcutsSession 'pause-recording'
        Wait-E2ESeconds 0.5
        Assert-E2E (-not $portal.ClosedRequests.Contains($recordingRequest)) 'pause or a foreign session stopped the pending recording'
        Send-PortalActivated $portal $recordingShortcutsSession 'stop-recording'
        Wait-E2EUntil { $portal.ClosedRequests.Contains($recordingRequest) } 'the global stop shortcut to cancel the pending request'
        Wait-E2EUntil { $portal.Closes.Contains($recordingSession) } 'the global stop shortcut to close the pending recording session'
        Assert-E2E (Test-EmptyDirectory $captures) 'the global stop saved an empty recording'
        $driver.Click(670, 80)
        Wait-E2EUntil { $portal.Closes.Contains($recordingShortcutsSession) } 'the recording shortcuts session to close'
        Wait-E2ESeconds 0.4

        $reader = New-E2ETextReader
        try {
            $combo = Find-E2ELabel $reader $driver 'No start shortcut'
            if ($null -eq $combo) {
                $label = Find-E2ELabel $reader $driver 'Recording start shortcut'
                if ($null -eq $label) {
                    Write-E2ETrace $driver 'shortcut-start-missing'
                    $observed = $reader.Recognize($driver.Capture(0, 0, 800, 270), 6, $false)
                    throw [System.Exception]::new("AssertionError: the recording start choice must be visible; OCR saw '$observed'")
                }
                $combo = @(($label[0] + 175), $label[1])
            }
            $driver.Click($combo[0], $combo[1])
            $driver.Press('Home')
            $driver.Press('Down')
            $driver.Press('Down')
            $driver.Press('Return')
            $driver.Click(460, 80)
            Wait-E2EUntil { $portal.Bindings.Count -eq 7 } 'the recording start binding'
            $recordingStartSession, $boundShortcuts = $portal.Bindings[-1]
            Assert-E2E (Test-E2EDeepEqual (Get-ShortcutIds $boundShortcuts) @('capture', 'toggle-recording', 'pause-recording', 'stop-recording')) "unexpected shortcuts: $(Get-ShortcutIds $boundShortcuts)"
            Assert-E2E ($boundShortcuts[1][1]['preferred_trigger'].Value -ceq 'SHIFT+F8') "unexpected triggers: $(Get-PreferredTriggers $boundShortcuts)"
            $saved = Read-E2EJson $settings
            Assert-E2E ($saved['linux_global_recording_start'] -ceq 'microphone') (& $savedText)
            Assert-E2E ($saved['capture_directory'] -ceq $captures -and $saved['theme'] -ceq 'Light') (& $savedText)
            Wait-E2ESeconds 0.4

            Test-GlobalRecordingStartRoutesSelectedAudioAndStops $portal $recordingStartSession $driver $reader $captures
            Close-E2EX11Driver $driver
            $driver = $null
            Stop-E2EProcess $app
            $app = $null
            $previousBindings = $portal.Bindings.Count
            $app = Start-E2EProcess @($binaryPath) $environment $work $appLog
            Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'the restarted editor' 12
            $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
            Wait-E2EUntil { $portal.Bindings.Count -eq $previousBindings + 1 } 'the recording start shortcut to restore after restart'
            $restoredStartSession, $restored = $portal.Bindings[-1]
            Assert-E2E (Test-E2EDeepEqual (Get-ShortcutIds $restored) @('capture', 'toggle-recording', 'pause-recording', 'stop-recording')) "unexpected shortcuts: $(Get-ShortcutIds $restored)"
            Assert-E2E ((Read-E2EJson $settings)['linux_global_recording_start'] -ceq 'microphone') 'the recording start choice must persist across restart'
            Wait-E2ESeconds 0.4
            Test-GlobalRecordingStartRoutesSelectedAudioAndStops $portal $restoredStartSession $driver $reader $captures
            $driver.Click(670, 80)
            Wait-E2EUntil { $portal.Closes.Contains($restoredStartSession) } 'the restored shortcut session to close'
            Wait-E2ESeconds 0.4
            $driver.Click($combo[0], $combo[1])
            $driver.Press('Home')
            $driver.Press('Return')
            Write-E2ELine 'PASS test_global_recording_start_routes_selected_audio_and_stops (including restart)'
        }
        finally {
            $reader.Dispose()
        }

        $portal.Mode = 'partial'
        $driver.Click(460, 80)
        Wait-E2EUntil { $portal.Bindings.Count -eq 9 } 'a partial shortcut response'
        $partial = $portal.Bindings[-1][0]
        Wait-E2EUntil { $portal.Closes.Contains($partial) } 'the partially bound shortcut session to close'
        Wait-E2ESeconds 0.4
        $portal.Mode = 'success'
        $driver.Click(20, 175)
        Write-E2ELine 'GUI opted in to recording shortcuts, routed stop, and rejected partial bindings'

        foreach ($pair in @(
                @('create', 'cancel'),
                @('create', 'stop'),
                @('select', 'cancel'),
                @('select', 'stop'),
                @('start', 'cancel'),
                @('start', 'stop')
            )) {
            $phase, $action = $pair
            $portal.ScreenMode = "hold-$phase"
            $requests = @{
                create = $portal.ScreenCreates
                select = $portal.ScreenSelects
                start  = $portal.ScreenStarts
            }[$phase]
            $previous = $requests.Count
            $driver.Click(300, 45)
            Wait-E2EUntil { $requests.Count -eq $previous + 1 } "the pending $phase request"
            $pending, $pendingRequest = $requests[-1]
            if ($action -eq 'cancel') {
                $driver.Click(560, 140)
            }
            else {
                $driver.Click(300, 45)
            }
            Wait-E2EUntil { $portal.ClosedRequests.Contains($pendingRequest) } "the $phase request to close after $action" -Timeout 5
            Wait-E2EUntil { $portal.Closes.Contains($pending) } "the $phase session to close after $action"
            Assert-E2E (Test-EmptyDirectory $captures) "$action during $phase left a capture file"
            Wait-E2ESeconds 0.5
        }

        $portal.ScreenMode = 'hold-select'
        $previous = $portal.ScreenSelects.Count
        $driver.Click(300, 45)
        Wait-E2EUntil { $portal.ScreenSelects.Count -eq $previous + 1 } 'the selection response/cancel race'
        $pending, $pendingRequest = $portal.ScreenSelects[-1]
        Send-PortalResponse $portal $pendingRequest 0 @{}
        $driver.Click(560, 140)
        Wait-E2EUntil { $portal.Closes.Contains($pending) } 'the raced session to close'
        Wait-E2ESeconds 0.5
        foreach ($start in $portal.ScreenStarts) {
            $startSession, $startRequest = $start
            if ($startSession -ceq $pending) {
                Assert-E2E ($portal.ClosedRequests.Contains($startRequest)) 'a start request created during cancellation remained open'
            }
        }
        Assert-E2E (Test-EmptyDirectory $captures) 'the cancellation race left a capture file'
        Write-E2ELine 'GUI cancelled and stopped all pending portal phases and a response race'

        Close-E2EX11Driver $driver
        $driver = $null
        Stop-E2EProcess $app
        $app = $null
        $saved = Read-E2EJson $settings
        Assert-E2E (Test-StrictBool $saved['linux_global_shortcuts_enabled'] $true) (& $savedText)
        $previousBindings = $portal.Bindings.Count
        $app = Start-E2EProcess @($binaryPath) $environment $work $appLog
        Wait-E2EUntil { $portal.Bindings.Count -gt $previousBindings } 'the opted-in shortcuts to bind after restart'
        $restoredSession, $restored = $portal.Bindings[-1]
        Assert-E2E (Test-E2EDeepEqual (Get-ShortcutIds $restored) @('capture', 'pause-recording', 'stop-recording')) "unexpected shortcuts: $(Get-ShortcutIds $restored)"
        Assert-E2E ($restored[0][1]['preferred_trigger'].Value -ceq 'CTRL+ALT+P') "unexpected triggers: $(Get-PreferredTriggers $restored)"
        $previousRequests = $portal.CaptureRequests
        $portal.ScreenMode = 'reject'
        Send-PortalActivated $portal $restoredSession 'capture'
        Wait-E2EUntil { $portal.CaptureRequests -gt $previousRequests } 'the restored shortcut to activate'
        Stop-E2EProcess $app
        $app = $null
        $saved = Read-E2EJson $settings
        Write-E2EJson $settings (Copy-E2EMap $saved @{ linux_global_shortcuts_enabled = $false })
        $previousBindings = $portal.Bindings.Count
        $app = Start-E2EProcess @($binaryPath) $environment $work $appLog
        Wait-E2ESeconds 1.5
        Assert-E2E (-not $app.HasExited) 'the application exited with shortcuts disabled'
        Assert-E2E ($portal.Bindings.Count -eq $previousBindings) 'disabled shortcuts must not bind on restart'
        Stop-E2EProcess $app
        $app = $null
        Write-E2ELine 'Opted-in shortcuts restored after restart and stayed disabled after opt-out'
        $portal.ScreenMode = 'hold-select'
        foreach ($case in @(
                @('--capture', 2, @{}),
                @('--scrolling', 1, @{}),
                @('--scrolling-horizontal', 1, @{}),
                @('--panoramic', 1, @{}),
                @('--record', 2, @{}),
                @('--capture', 1, @{ capture_cursor = $false; recording_cursor = $true }),
                @('--record', 2, @{ capture_cursor = $false; recording_cursor = $true }),
                @('--capture', 2, @{ capture_cursor = $true; recording_cursor = $false }),
                @('--record', 1, @{ capture_cursor = $true; recording_cursor = $false }),
                @('--scrolling', 1, @{ capture_cursor = $true; recording_cursor = $false })
            )) {
            $argument, $expectedCursorMode, $cursorSettings = $case
            $saved = Read-E2EJson $settings
            Write-E2EJson $settings (Copy-E2EMap $saved $cursorSettings)
            $previous = $portal.ScreenSelects.Count
            $app = Start-E2EProcess @($binaryPath, $argument) $environment $work $appLog
            Wait-E2EUntil { $portal.ScreenSelects.Count -eq $previous + 1 } "the $argument startup action"
            $startupSession, $startupRequest = $portal.ScreenSelects[-1]
            $options = $portal.ScreenSelectionOptions[-1]
            Assert-E2E ($options['cursor_mode'].Value -eq $expectedCursorMode) "$argument used cursor_mode $($options['cursor_mode'].Value), expected $expectedCursorMode"
            Send-PortalResponse $portal $startupRequest 1 @{}
            Wait-E2EUntil { $portal.Closes.Contains($startupSession) } "the $argument session to close after rejection"
            Stop-E2EProcess $app
            $app = $null
        }
        foreach ($case in @(@('--capture', 'capture_cursor'), @('--record', 'recording_cursor'))) {
            $argument, $invalidKey = $case
            $saved = Read-E2EJson $settings
            Write-E2EJson $settings (Copy-E2EMap $saved @{ $invalidKey = 'invalid' })
            $previous = $portal.CaptureRequests
            $app = Start-E2EProcess @($binaryPath, $argument) $environment $work $appLog
            Wait-E2EUntil {
                $tree = & xwininfo -display $environment['DISPLAY'] -root -tree 2>&1
                if ($LASTEXITCODE -ne 0) {
                    throw "xwininfo -root -tree failed: $tree"
                }
                ($tree -join "`n").Contains('"Jiffshot"')
            } "the editor for $argument" 12
            Wait-E2ESeconds 0.5
            Assert-E2E ($portal.CaptureRequests -eq $previous) "an invalid $invalidKey must block the portal request"
            Stop-E2EProcess $app
            $app = $null
            Write-E2EJson $settings $saved
        }
        Write-E2ELine 'Command-line actions honored independent cursor settings and rejected invalid values'
    }
    catch {
        Write-E2ELogTail @($xvfbLog, $appLog)
        $state = [ordered]@{
            sessions        = $portal.Sessions
            bindings        = @(foreach ($binding in $portal.Bindings) { , @($binding[0], (Get-ShortcutIds $binding[1])) })
            closes          = $portal.Closes
            screen_creates  = $portal.ScreenCreates
            screen_selects  = $portal.ScreenSelects
            screen_starts   = $portal.ScreenStarts
            closed_requests = $portal.ClosedRequests
        }
        [Console]::Error.WriteLine((@(foreach ($key in $state.Keys) { "$key=$(ConvertTo-Json -InputObject $state[$key] -Depth 6 -Compress)" }) -join ' '))
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
