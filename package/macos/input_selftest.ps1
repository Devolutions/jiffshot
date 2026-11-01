#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Exercise global macOS input against disposable windows and restore the pointer.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$Cli,
    [Parameter(Mandatory)][string]$HostLibrary,
    [string]$Output
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacPackage.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacFixture.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacControl.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacDrag.psm1') -Force -DisableNameChecking

function Control([string[]]$Arguments, [int]$Expected = 0) {
    if (-not $script:InputCleaning) { $script:InputInterrupt.ThrowIfInterrupted() }
    return Invoke-ControlCommand -Cli $script:ControlCli -Environment $script:ControlEnvironment `
        -Arguments $Arguments -Expected $Expected
}

function Wait-InputState([string]$Path, [scriptblock]$Predicate, [string]$Description, [int]$TimeoutSeconds = 8) {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $state = @{ events = @(); text = '' }
    while ($clock.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
        if (-not $script:InputCleaning) { $script:InputInterrupt.ThrowIfInterrupted() }
        if ([IO.File]::Exists($Path)) {
            $state = Read-FixtureJson $Path
            if (& $Predicate $state) { return $state }
        }
        Start-Sleep -Milliseconds 30
    }
    throw "${Description}: $(ConvertTo-Json -InputObject $state -Compress -Depth 100)"
}

function Start-InputFixture([string]$Executable, [string]$Root, [string]$Name) {
    $infoPath = Join-Path $Root "$Name-info.json"
    $statePath = Join-Path $Root "$Name-state.json"
    $fixture = Start-FixtureProcess -File $Executable -Arguments @($infoPath, $statePath) `
        -Environment $script:ControlEnvironment
    try {
        $clock = [Diagnostics.Stopwatch]::StartNew()
        while (-not [IO.File]::Exists($infoPath)) {
            if ($fixture.Process.get_HasExited()) {
                $result = Wait-FixtureProcess $fixture 5
                throw "the input fixture exited: $($result.Stderr)"
            }
            if ($clock.Elapsed.TotalSeconds -ge 15) { throw 'the input fixture did not appear' }
            Start-Sleep -Milliseconds 50
        }
        $info = Read-FixtureJson $infoPath
        Assert-FixtureCondition ($info.pid -eq $fixture.Process.Id) 'the input fixture returned the wrong PID' $info
        return [pscustomobject]@{ Process = $fixture; Info = $info; StatePath = $statePath }
    }
    catch {
        Complete-InputCleanup @($fixture) $_.Exception.Message
    }
}

function ConvertTo-InputNumber([double]$Value) {
    return $Value.ToString('R', [Globalization.CultureInfo]::InvariantCulture)
}

function Assert-InputOwner([string]$Executable, $Info, $Rect, $Point) {
    $scaleX = $Rect.width / $Info.bounds.Width
    $scaleY = $Rect.height / $Info.bounds.Height
    Assert-FixtureCondition ([Math]::Abs($scaleX - $scaleY) -le 0.02 -and $scaleX -gt 0) `
        "the fixture's reference grid is inconsistent" @($Rect, $Info.bounds)
    $result = Invoke-MacCommand -File $Executable -Arguments @(
        '--owner-at', (ConvertTo-InputNumber ($Point[0] / $scaleX)), (ConvertTo-InputNumber ($Point[1] / $scaleY))
    ) -TimeoutSeconds 5
    $owner = [long]::Parse($result.Stdout.Trim(), [Globalization.CultureInfo]::InvariantCulture)
    Assert-FixtureCondition ($owner -eq $Info.window) "a different window covers the fixture at $($Point -join ',')" `
        @($owner, $Rect, $Info)
}

function Get-InputPointArgument($Point) { return "$($Point[0]),$($Point[1])" }
function Get-InputPointResult($Point) { return @{ x = $Point[0]; y = $Point[1] } }

function Wait-InputCursor($Point, $Fixture) {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $expected = Get-InputPointResult $Point
    while ($clock.Elapsed.TotalSeconds -lt 10) {
        if (-not $script:InputCleaning) { $script:InputInterrupt.ThrowIfInterrupted() }
        if ($Fixture.Process.get_HasExited()) {
            $result = Wait-FixtureProcess $Fixture 5
            throw "the scrolling capture exited before moving the pointer: $($result.Stdout); $($result.Stderr)"
        }
        if (Test-FixtureEqual (Control @('cursor')) $expected) { return }
        Start-Sleep -Milliseconds 30
    }
    throw "the scrolling capture did not move the pointer to $($Point -join ',')"
}

function Get-NativeCursor([string]$Executable) {
    $result = Invoke-MacCommand -File $Executable -Arguments @('--cursor') -TimeoutSeconds 5
    $parts = @($result.Stdout.Trim() -split '\s+')
    if ($parts.Count -ne 2) { throw "invalid native pointer position: $($result.Stdout)" }
    return ,[double[]]@(
        [double]::Parse($parts[0], [Globalization.CultureInfo]::InvariantCulture),
        [double]::Parse($parts[1], [Globalization.CultureInfo]::InvariantCulture)
    )
}

function Get-InputEvents($State, [int]$Start, [string]$Name) {
    return @($State.events | Select-Object -Skip $Start | Where-Object { $_.name -ceq $Name })
}

function Focus-InputReady($Window) {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    while ($true) {
        if (-not $script:InputCleaning) { $script:InputInterrupt.ThrowIfInterrupted() }
        $result = Invoke-MacCommand -File $script:ControlCli -Arguments @('--no-launch', 'focus', '--window', [string]$Window) `
            -Environment $script:ControlEnvironment -TimeoutSeconds 10 -AllowFailure
        try { $response = ConvertFrom-Json -InputObject $result.Stdout -AsHashtable -Depth 100 }
        catch { throw "invalid focus response: $($result.Stdout); $($result.Stderr)" }
        if ($result.ExitCode -eq 0) { return $response.result }
        $focusError = $response.error
        if (($focusError.code -cne 'not_found' -and -not $focusError.message.Contains('not available through Accessibility')) -or
            $clock.Elapsed.TotalSeconds -ge 6) {
            throw "could not focus the input fixture: $($result.Stdout); $($result.Stderr)"
        }
        Start-Sleep -Milliseconds 100
    }
}

function Invoke-ScrollCaptureChecks([string]$Root, [string]$Executable, $Info, $Rect, $Start, $End) {
    $region = '{0},{1},60,60' -f ($Start[0] - 30), ($Start[1] - 30)
    [void](Control @('move', (Get-InputPointArgument $End), '--glide-ms', '0'))
    Start-Sleep -Milliseconds 100
    $original = Control @('cursor')
    $imagePath = Join-Path $Root 'static-scroll.png'
    $result = Control @('capture', 'scroll', '--region', $region, '--out', $imagePath,
        '--max-frames', '2', '--settle-ms', '150', '--notches', '1')
    Assert-FixtureCondition ($result.stopped -ceq 'no_movement' -and $result.frames -eq 1 -and -not $result.limited -and
        $result.path -ceq $imagePath -and $result.target.kind -ceq 'region' -and [IO.File]::Exists($imagePath) -and
        (Test-FixtureEqual (Control @('cursor')) $original)) 'the static CLI scrolling capture failed' $result
    Assert-FixtureCondition ((Control @('capture', 'scroll', '--region', $region, '--notches', '21') 2).code -ceq 'invalid_request') `
        'an invalid scrolling-capture notch count was accepted'
    Assert-FixtureCondition (Test-FixtureEqual (Control @('cursor')) $original) 'a rejected scrolling capture moved the pointer'
    $imagePath = Join-Path $Root 'interrupted-scroll.png'
    $scrolling = Start-FixtureProcess -File $script:ControlCli -Arguments @(
        '--no-launch', 'capture', 'scroll', '--region', $region, '--out', $imagePath,
        '--settle-ms', '3000', '--timeout-ms', '10000'
    ) -Environment $script:ControlEnvironment
    $failure = $null
    try {
        Wait-InputCursor $Start $scrolling
        Start-Sleep -Milliseconds 150
        Assert-InputOwner $Executable $Info $Rect $End
        $scaleX = $Rect.width / $Info.bounds.Width
        $scaleY = $Rect.height / $Info.bounds.Height
        [void](Invoke-MacCommand -File $Executable -Arguments @(
            '--warp', (ConvertTo-InputNumber ($End[0] / $scaleX)), (ConvertTo-InputNumber ($End[1] / $scaleY))
        ) -TimeoutSeconds 5)
        $moved = Control @('cursor')
        Assert-FixtureCondition (-not (Test-FixtureEqual $moved (Get-InputPointResult $Start))) `
            'the simulated user movement did not move the pointer'
        $result = Wait-FixtureProcess $scrolling 10
        $response = ConvertFrom-Json -InputObject $result.Stdout -AsHashtable -Depth 100
        Assert-FixtureCondition ($result.ExitCode -eq 1 -and $response.error.code -ceq 'conflict' -and
            $response.error.message.Contains('pointer moved') -and
            $response.error.message.Contains('original position was not restored') -and
            -not [IO.File]::Exists($imagePath) -and (Test-FixtureEqual (Control @('cursor')) $moved)) `
            "scrolling did not preserve the user's pointer movement" @($result.Stdout, $result.Stderr)
    }
    catch { $failure = $_.Exception.Message }
    finally { Complete-InputCleanup @($scrolling) $failure }
}

function Complete-InputCleanup([object[]]$Processes, [string]$Failure, $FailedProcesses = $null) {
    $failures = [Collections.Generic.List[string]]::new()
    if ($Failure) { $failures.Add($Failure) }
    foreach ($process in $Processes) {
        if ($null -eq $process) { continue }
        try { Stop-FixtureProcess $process }
        catch {
            $failures.Add("could not stop an input fixture process: $($_.Exception.Message)")
            if ($null -ne $FailedProcesses) { $FailedProcesses.Add($process) }
        }
    }
    if ($failures.Count -gt 0) { throw ($failures -join '; ') }
}

function Stop-InputSourceForRecovery($App) {
    $failures = [Collections.Generic.List[string]]::new()
    try { $exited = $App.Process.get_HasExited() }
    catch {
        $failures.Add("could not verify the input source exit: $($_.Exception.Message)")
        return [pscustomobject]@{ Exited = $false; Error = $failures -join '; ' }
    }
    if (-not $exited) {
        try {
            $status = Control @('status')
            Assert-FixtureCondition ($status.pid -eq $App.Process.Id) 'the input source no longer owns the control server'
            $result = Control @('quit')
            Assert-FixtureCondition (Test-FixtureEqual $result @{ quitting = $true }) `
                'the input source did not accept the shutdown request'
        }
        catch { $failures.Add("could not request input source shutdown: $($_.Exception.Message)") }
        try {
            $result = Wait-FixtureProcess $App 45
            $exited = $true
            if ($result.ExitCode -ne 0) {
                $failures.Add("the input source exited unsuccessfully: $($result.ExitCode)")
            }
        }
        catch {
            $failures.Add("could not wait for the input source to stop: $($_.Exception.Message)")
            try { $exited = $App.Process.get_HasExited() }
            catch {
                $exited = $false
                $failures.Add("could not verify the input source exit: $($_.Exception.Message)")
            }
        }
    }
    return [pscustomobject]@{ Exited = $exited; Error = $failures -join '; ' }
}

function Invoke-ScrollShutdownCheck([string]$Root, [string]$BinaryPath, [string]$Executable, $Window, $Start, $End) {
    $app = Start-FixtureProcess -File $BinaryPath -Environment $script:ControlEnvironment `
        -LogPath (Join-Path $Root 'scroll-shutdown.log')
    $scrolling = $null
    $failure = $null
    try {
        $status = Wait-ControlServer $script:ControlCli $script:ControlEnvironment $app
        Assert-FixtureCondition ($status.pid -eq $app.Process.Id) 'the shutdown fixture connected to the wrong app'
        [void](Focus-InputReady $Window)
        [void](Control @('move', (Get-InputPointArgument $End), '--glide-ms', '0'))
        Start-Sleep -Milliseconds 100
        $before = Get-NativeCursor $Executable
        $region = '{0},{1},60,60' -f ($Start[0] - 30), ($Start[1] - 30)
        $imagePath = Join-Path $Root 'shutdown-scroll.png'
        $scrolling = Start-FixtureProcess -File $script:ControlCli -Arguments @(
            '--no-launch', 'capture', 'scroll', '--region', $region, '--out', $imagePath,
            '--settle-ms', '30000', '--timeout-ms', '60000'
        ) -Environment $script:ControlEnvironment
        Wait-InputCursor $Start $scrolling
        Assert-FixtureCondition (Test-FixtureEqual (Control @('quit')) @{ quitting = $true }) `
            'the app did not accept the scrolling shutdown request'
        $result = Wait-FixtureProcess $scrolling 10
        $response = ConvertFrom-Json -InputObject $result.Stdout -AsHashtable -Depth 100
        Assert-FixtureCondition ($result.ExitCode -eq 1 -and $response.error.code -ceq 'conflict' -and
            -not [IO.File]::Exists($imagePath)) 'shutdown did not cancel scrolling capture' @($result.Stdout, $result.Stderr)
        $result = Wait-FixtureProcess $app 45
        Assert-FixtureCondition ($result.ExitCode -eq 0) 'the scrolling shutdown app exited unsuccessfully' $result.ExitCode
        Assert-FixtureCondition (Test-FixtureEqual (Get-NativeCursor $Executable) $before) `
            'shutdown did not restore the pointer after scrolling capture'
    }
    catch { $failure = $_.Exception.Message }
    finally {
        $failedProcesses = [Collections.Generic.List[object]]::new()
        try { Complete-InputCleanup @($scrolling, $app) $failure $failedProcesses }
        catch {
            if ($failedProcesses.Contains($app)) { $script:InputRecoveryBlocked = $true }
            throw
        }
    }
}

function Restore-InputPointer([string]$Root, $App, $Original) {
    if ($script:InputRecoveryBlocked) {
        throw 'another input source may still be active; not restoring the pointer'
    }
    if ((Test-AnyDragButton $script:InputProbe.Buttons()) -or $script:InputProbe.Modifiers()) {
        throw 'mouse buttons or modifiers are active; not restoring the pointer'
    }
    $point = "$($Original.x),$($Original.y)"
    if (-not $App.Process.get_HasExited()) {
        throw 'the original input source is still active; not restoring the pointer'
    }
    $reopened = Start-FixtureProcess -File $App.File -Environment $script:ControlEnvironment `
        -LogPath (Join-Path $Root 'restore.log') -AppendLog
    $failure = $null
    try {
        $status = Wait-ControlServer $script:ControlCli $script:ControlEnvironment $reopened
        Assert-FixtureCondition ($status.pid -eq $reopened.Process.Id) 'the pointer-restoration fixture connected to the wrong app'
        if ((Test-AnyDragButton $script:InputProbe.Buttons()) -or $script:InputProbe.Modifiers()) {
            throw 'mouse buttons or modifiers became active; not restoring the pointer'
        }
        [void](Control @('move', $point, '--glide-ms', '0'))
        [void](Control @('quit'))
        $result = Wait-FixtureProcess $reopened 45
        Assert-FixtureCondition ($result.ExitCode -eq 0) 'the pointer-restoration app exited unsuccessfully' $result.ExitCode
    }
    catch { $failure = $_.Exception.Message }
    finally {
        $failedProcesses = [Collections.Generic.List[object]]::new()
        try { Complete-InputCleanup @($reopened) $failure $failedProcesses }
        catch {
            if ($failedProcesses.Contains($reopened)) { $script:InputRecoveryBlocked = $true }
            throw
        }
    }
}

function Invoke-ConcurrentTypingCheck([string]$StatePath, [string]$Window, [string]$Typed) {
    $first = Start-FixtureProcess -File $script:ControlCli -Arguments @(
        '--no-launch', 'type', 'ABC', '--window', $Window, '--delay-ms', '250'
    ) -Environment $script:ControlEnvironment
    $second = $null
    $failure = $null
    try {
        [void](Wait-InputState $StatePath { param($value) $value.text -ceq ($Typed + 'A') } `
            'the first concurrent input did not start')
        $second = Start-FixtureProcess -File $script:ControlCli -Arguments @(
            '--no-launch', 'type', 'XYZ', '--window', $Window
        ) -Environment $script:ControlEnvironment
        $firstResult = Wait-FixtureProcess $first 20
        $secondResult = Wait-FixtureProcess $second 20
        Assert-FixtureCondition ($firstResult.ExitCode -eq 0 -and $secondResult.ExitCode -eq 0) `
            'concurrent input failed' @($firstResult, $secondResult)
    }
    catch { $failure = $_.Exception.Message }
    finally { Complete-InputCleanup @($second, $first) $failure }
}

function Invoke-InputChecks([string]$Root, $App, [string]$Executable) {
    $fixture = $null
    $other = $null
    $script:InputRecoveryBlocked = $false
    $original = Control @('cursor')
    $restored = $false
    $startedInput = $false
    $failure = $null
    try {
        $fixture = Start-InputFixture $Executable $Root 'input'
        $info = $fixture.Info
        $statePath = $fixture.StatePath
        $window = [string]$info.window
        $focused = Focus-InputReady $window
        Assert-FixtureCondition ($focused.window.pid -eq $fixture.Process.Process.Id) `
            'the wrong input fixture was focused' $focused
        $rect = $focused.window.rect
        $start = @(
            ($rect.x + [Math]::Floor($rect.width / 3)),
            ($rect.y + [Math]::Floor($rect.height * 2 / 3))
        )
        $end = @(($rect.x + [Math]::Floor($rect.width * 2 / 3)), $start[1])
        Assert-InputOwner $Executable $info $rect $start
        Assert-InputOwner $Executable $info $rect $end

        $startedInput = $true
        $moved = Control @('move', (Get-InputPointArgument $start), '--glide-ms', '80')
        Assert-FixtureCondition ((Test-FixtureEqual $moved.at (Get-InputPointResult $start)) -and $null -eq $moved.matched) `
            'pointer movement returned the wrong result' $moved
        $state = Wait-InputState $statePath { param($value) @(Get-InputEvents $value 0 'move').Count -gt 0 } `
            'no mouse move reached the fixture'
        $index = $state.events.Count
        $clicked = Control @('click', (Get-InputPointArgument $start), '--double', '--settle-ms', '100')
        $state = Wait-InputState $statePath { param($value) @(Get-InputEvents $value $index 'leftUp').Count -eq 2 } `
            'the double click did not reach the fixture'
        $downs = @(Get-InputEvents $state $index 'leftDown')
        Assert-FixtureCondition ((Test-FixtureEqual $clicked.at (Get-InputPointResult $start)) -and $clicked.count -eq 2 -and
            (Test-FixtureEqual @($downs | ForEach-Object { $_.clicks }) @(1, 2))) `
            'the click counts were not delivered' @($clicked, $downs)
        foreach ($button in @(
            @{ flag = '--right'; down = 'rightDown'; up = 'rightUp' }
            @{ flag = '--middle'; down = 'otherDown'; up = 'otherUp' }
        )) {
            $index = $state.events.Count
            [void](Control @('click', (Get-InputPointArgument $start), $button.flag, '--settle-ms', '100'))
            $state = Wait-InputState $statePath {
                param($value)
                @(Get-InputEvents $value $index $button.down).Count -gt 0 -and
                    @(Get-InputEvents $value $index $button.up).Count -gt 0
            } "the $($button.flag) click did not reach the fixture"
        }
        $index = $state.events.Count
        $dragged = Control @(
            'drag', (Get-InputPointArgument $start), (Get-InputPointArgument $end), '--glide-ms', '400', '--settle-ms', '100'
        )
        Assert-FixtureCondition (Test-FixtureEqual $dragged @{
            from = Get-InputPointResult $start
            to = Get-InputPointResult $end
        }) 'the drag returned the wrong points' $dragged
        $state = Wait-InputState $statePath {
            param($value)
            @(Get-InputEvents $value $index 'leftDown').Count -gt 0 -and
                @(Get-InputEvents $value $index 'leftDrag').Count -gt 0 -and
                @(Get-InputEvents $value $index 'leftUp').Count -gt 0
        } 'the mouse drag did not reach the fixture'
        foreach ($scroll in @(
            @{ extra = @(); axis = 'dy'; sign = -1 }
            @{ extra = @('--horizontal'); axis = 'dx'; sign = -1 }
        )) {
            $index = $state.events.Count
            $scrolled = Control (@('scroll', '2', '--at', (Get-InputPointArgument $start)) +
                $scroll.extra + @('--settle-ms', '100'))
            Assert-FixtureCondition (Test-FixtureEqual $scrolled.at (Get-InputPointResult $start)) `
                'the scroll returned the wrong point' $scrolled
            $state = Wait-InputState $statePath { param($value) @(Get-InputEvents $value $index 'scroll').Count -ge 2 } `
                'the scroll events did not reach the fixture'
            $events = @(Get-InputEvents $state $index 'scroll')
            Assert-FixtureCondition (@($events | Where-Object { $_[$scroll.axis] * $scroll.sign -le 0 }).Count -eq 0) `
                'the scroll direction is wrong' $events
        }
        Invoke-ScrollCaptureChecks $Root $Executable $info $rect $start $end

        $typed = "SYNTHETIC123`ncafé 🐱"
        $result = Control @('type', $typed, '--window', $window, '--settle-ms', '100')
        $state = Wait-InputState $statePath { param($value) $value.text -ceq $typed } 'Unicode text was not inserted'
        $characterCount = [Text.Encoding]::UTF32.GetByteCount($typed) / 4
        Assert-FixtureCondition ($result.typed -eq $characterCount -and $result.window.hwnd -eq $info.window) `
            'typing returned the wrong result' $result
        $found = Control @('move', '--text', 'SYNTHETIC123', '--window', $window)
        Assert-FixtureCondition ($found.matched.text.text.ToUpperInvariant().Contains('SYNTHETIC123')) `
            'OCR pointer targeting returned the wrong match' $found

        Invoke-ConcurrentTypingCheck $statePath $window $typed
        $state = Wait-InputState $statePath { param($value) $value.text -ceq ($typed + 'ABCXYZ') } `
            'concurrent input sequences interleaved'
        $index = $state.events.Count
        [void](Control @('key', 'shift+q', '--window', $window, '--settle-ms', '100'))
        $state = Wait-InputState $statePath { param($value) @(Get-InputEvents $value $index 'keyDown').Count -gt 0 } `
            'the shifted key did not reach the fixture'
        $keys = @(Get-InputEvents $state $index 'keyDown')
        Assert-FixtureCondition (@($keys | Where-Object { $_.characters -ceq 'Q' -and ($_.modifiers -band (1 -shl 17)) }).Count -gt 0) `
            'keyboard layout or shift state was wrong' $keys
        $index = $state.events.Count
        [void](Control @('key', 'plus', '--window', $window, '--settle-ms', '100'))
        $state = Wait-InputState $statePath { param($value) @(Get-InputEvents $value $index 'keyDown').Count -gt 0 } `
            'the plus key did not reach the fixture'
        $keys = @(Get-InputEvents $state $index 'keyDown')
        Assert-FixtureCondition (@($keys | Where-Object { $_.characters -ceq '+' }).Count -gt 0) `
            "the keyboard layout's implied modifiers were wrong" $keys
        $index = $state.events.Count
        [void](Control @('key', 'cmd+shift+a', '--window', $window, '--settle-ms', '100'))
        $state = Wait-InputState $statePath { param($value) @(Get-InputEvents $value $index 'keyDown').Count -gt 0 } `
            'the command shortcut did not reach the fixture'
        $keys = @(Get-InputEvents $state $index 'keyDown')
        $modifiers = (1 -shl 17) -bor (1 -shl 20)
        Assert-FixtureCondition (@($keys | Where-Object { ($_.modifiers -band $modifiers) -eq $modifiers }).Count -gt 0) `
            'the command shortcut modifiers were wrong' $keys
        [void](Control @('key', 'cmd+down', '--window', $window))

        $other = Start-InputFixture $Executable $Root 'serialized-focus'
        [void](Focus-InputReady $window)
        $beforeText = (Read-FixtureJson $statePath).text
        $typing = Start-FixtureProcess -File $script:ControlCli -Arguments @(
            '--no-launch', 'type', 'LOCK', '--window', $window, '--delay-ms', '350'
        ) -Environment $script:ControlEnvironment
        $focusing = $null
        $childFailure = $null
        try {
            [void](Wait-InputState $statePath { param($value) $value.text -ceq ($beforeText + 'L') } 'paced typing did not start')
            $focusing = Start-FixtureProcess -File $script:ControlCli -Arguments @(
                '--no-launch', 'focus', '--window', [string]$other.Info.window
            ) -Environment $script:ControlEnvironment
            $typeResult = Wait-FixtureProcess $typing 20
            $focusResult = Wait-FixtureProcess $focusing 20
            Assert-FixtureCondition ($typeResult.ExitCode -eq 0 -and $focusResult.ExitCode -eq 0) `
                'focus interrupted serialized input' @($typeResult, $focusResult)
            [void](Wait-InputState $statePath { param($value) $value.text -ceq ($beforeText + 'LOCK') } `
                'focus interrupted paced typing')
            if ([IO.File]::Exists($other.StatePath) -and (Read-FixtureJson $other.StatePath).text) {
                throw 'serialized input reached the other window'
            }
        }
        catch { $childFailure = $_.Exception.Message }
        finally {
            Complete-InputCleanup @($focusing, $typing, $other.Process) $childFailure
            $other = $null
        }
        [void](Focus-InputReady $window)

        $beforeText = (Read-FixtureJson $statePath).text
        $typing = Start-FixtureProcess -File $script:ControlCli -Arguments @(
            '--no-launch', 'type', 'STOP', '--window', $window, '--delay-ms', '1500'
        ) -Environment $script:ControlEnvironment
        $childFailure = $null
        try {
            [void](Wait-InputState $statePath { param($value) $value.text -ceq ($beforeText + 'S') } `
                'focus-loss typing did not start')
            $other = Start-InputFixture $Executable $Root 'focus-loss'
            $result = Wait-FixtureProcess $typing 20
            $response = ConvertFrom-Json -InputObject $result.Stdout -AsHashtable -Depth 100
            Assert-FixtureCondition ($result.ExitCode -eq 1 -and $response.error.code -ceq 'conflict') `
                'typing did not stop after focus changed' @($result.Stdout, $result.Stderr)
            $state = Read-FixtureJson $statePath
            Assert-FixtureCondition ($state.text -ceq ($beforeText + 'S') -and
                (-not [IO.File]::Exists($other.StatePath) -or -not (Read-FixtureJson $other.StatePath).text)) `
                'typing continued after the target lost focus'
        }
        catch { $childFailure = $_.Exception.Message }
        finally {
            $otherProcess = if ($other) { $other.Process } else { $null }
            Complete-InputCleanup @($typing, $otherProcess) $childFailure
            $other = $null
        }
        [void](Focus-InputReady $window)

        Invoke-AccessibilityInputChecks $Root $Executable $fixture $info $rect $start $end $window
        $state = Read-FixtureJson $statePath
        $index = $state.events.Count
        $dragging = Start-FixtureProcess -File $script:ControlCli -Arguments @(
            '--no-launch', '--timeout', '30', 'drag', (Get-InputPointArgument $start), (Get-InputPointArgument $end),
            '--glide-ms', '6000', '--settle-ms', '0'
        ) -Environment $script:ControlEnvironment
        $childFailure = $null
        try {
            [void](Wait-InputState $statePath {
                param($value)
                @(Get-InputEvents $value $index 'leftDown').Count -gt 0 -and
                    @(Get-InputEvents $value $index 'leftDrag').Count -gt 0
            } 'the cancellable drag did not start')
            Assert-FixtureCondition (Test-FixtureEqual (Control @('quit')) @{ quitting = $true }) `
                'the app did not accept the shutdown request'
            $result = Wait-FixtureProcess $dragging 15
            $response = ConvertFrom-Json -InputObject $result.Stdout -AsHashtable -Depth 100
            Assert-FixtureCondition ($result.ExitCode -eq 1 -and $response.error.code -ceq 'conflict') `
                'shutdown did not cancel the drag' @($result.Stdout, $result.Stderr)
            [void](Wait-InputState $statePath { param($value) @(Get-InputEvents $value $index 'leftUp').Count -gt 0 } `
                'shutdown did not release the injected mouse button')
            $result = Wait-FixtureProcess $App 45
            Assert-FixtureCondition ($result.ExitCode -eq 0) 'Jiffshot exited unsuccessfully' $result.ExitCode
        }
        catch { $childFailure = $_.Exception.Message }
        finally { Complete-InputCleanup @($dragging) $childFailure }

        Invoke-ScrollShutdownCheck $Root $App.File $Executable $window $start $end
        Restore-InputPointer $Root $App $original
        $restored = $true
    }
    catch { $failure = $_.Exception.Message }
    finally {
        $script:InputCleaning = $true
        if ($startedInput -and -not $restored) {
            $source = Stop-InputSourceForRecovery $App
            if ($source.Error) {
                $message = "could not cleanly stop the input source: $($source.Error)"
                $failure = if ($failure) { "$failure; $message" } else { $message }
            }
            if ($source.Exited -and -not $script:InputRecoveryBlocked) {
                try { Restore-InputPointer $Root $App $original }
                catch {
                    $message = "could not restore the pointer: $($_.Exception.Message)"
                    $failure = if ($failure) { "$failure; $message" } else { $message }
                }
            }
            else {
                $message = 'an input source may still be active; not restoring the pointer'
                $failure = if ($failure) { "$failure; $message" } else { $message }
            }
        }
        foreach ($process in @($other, $fixture)) {
            if ($null -ne $process) {
                try { Stop-FixtureProcess $process.Process }
                catch {
                    $message = "could not stop an input fixture: $($_.Exception.Message)"
                    $failure = if ($failure) { "$failure; $message" } else { $message }
                }
            }
        }
    }
    if ($failure) { throw $failure }
}

function Invoke-CoveredAccessibilityCheck([string]$Root, [string]$Executable, $Window) {
    $other = Start-InputFixture $Executable $Root 'covered-element'
    $failure = $null
    try {
        $before = Control @('cursor')
        foreach ($action in 'click', 'move', 'scroll') {
            $arguments = @($action)
            if ($action -ceq 'scroll') { $arguments += '1' }
            $arguments += @('--window', $Window, '--element', 'Synthetic Save', '--no-activate')
            Assert-FixtureCondition ((Control $arguments 1).code -ceq 'conflict') `
                "$action accepted a covered Accessibility target"
        }
        $coveredCapture = Join-Path $Root 'covered-scroll.png'
        $response = Control @('capture', 'scroll', '--window', $Window, '--no-activate', '--out', $coveredCapture) 1
        Assert-FixtureCondition ($response.code -ceq 'conflict' -and -not [IO.File]::Exists($coveredCapture)) `
            'scrolling capture accepted a covered window'
        Assert-FixtureCondition ((Test-FixtureEqual (Control @('cursor')) $before) -and -not [IO.File]::Exists($other.StatePath)) `
            'a covered Accessibility target received input'
    }
    catch { $failure = $_.Exception.Message }
    finally { Complete-InputCleanup @($other.Process) $failure }
}

function Invoke-AccessibilityInputChecks([string]$Root, [string]$Executable, $Fixture, $Info, $Rect, $Start, $End, $Window) {
    $statePath = $Fixture.StatePath
    $listing = Control @('elements', '--window', $Window)
    Assert-FixtureCondition ($listing.window.hwnd -eq $Info.window -and -not $listing.truncated) `
        'the Accessibility inventory was incomplete' $listing
    $encoded = ConvertTo-Json -InputObject $listing -Depth 100 -Compress
    Assert-FixtureCondition (-not $encoded.Contains('FIXTURE_PRIVATE_VALUE_9182') -and -not $encoded.Contains('SYNTHETIC123')) `
        'Accessibility enumeration exposed editable text'
    $buttons = @($listing.elements | Where-Object { $_.name -ceq 'Synthetic Save' })
    Assert-FixtureCondition ($buttons.Count -eq 1 -and $buttons[0].control_type) 'the fixture button was not enumerated' $listing
    $buttonType = $buttons[0].control_type
    $exact = Control @('elements', '--window', $Window, '--name', 'synthetic save', '--type', $buttonType)
    Assert-FixtureCondition (Test-FixtureEqual @($exact.elements | ForEach-Object { $_.name }) @('Synthetic Save')) `
        'Accessibility exact matching failed' $exact
    $partial = Control @('elements', '--window', $Window, '--name', 'synthetic sav', '--type', $buttonType)
    Assert-FixtureCondition (Test-FixtureEqual @($partial.elements | ForEach-Object { $_.name }) @('Synthetic Save', 'Synthetic Save As')) `
        'Accessibility substring matching failed' $partial
    $limited = Control @('elements', '--window', $Window, '--max', '1')
    Assert-FixtureCondition ($limited.elements.Count -eq 1 -and $limited.truncated) `
        'the Accessibility result limit was not reported' $limited
    $state = Read-FixtureJson $statePath
    $index = $state.events.Count
    Assert-InputOwner $Executable $Info $Rect $buttons[0].center
    $clicked = Control @('click', '--window', $Window, '--element', 'Synthetic Save', '--type', $buttonType, '--no-step', '--settle-ms', '100')
    Assert-FixtureCondition ($clicked.matched.element.name -ceq 'Synthetic Save') `
        'Accessibility click selected the wrong element' $clicked
    [void](Wait-InputState $statePath { param($value) @(Get-InputEvents $value $index 'button-save').Count -gt 0 } `
        'the Accessibility click did not activate the button')
    $state = Read-FixtureJson $statePath
    $index = $state.events.Count
    $clicked = Control @('click', '--window', $Window, '--element', 'synthetic sav', '--type', $buttonType,
        '--nth', '2', '--no-step', '--settle-ms', '100')
    Assert-FixtureCondition ($clicked.matched.element.name -ceq 'Synthetic Save As') `
        'Accessibility nth matching selected the wrong element' $clicked
    [void](Wait-InputState $statePath { param($value) @(Get-InputEvents $value $index 'button-save-as').Count -gt 0 } `
        'the second Accessibility click did not activate its button')
    Invoke-CoveredAccessibilityCheck $Root $Executable $Window
    [void](Focus-InputReady $Window)

    $before = Control @('cursor')
    Assert-FixtureCondition ((Control @('click', (Get-InputPointArgument $End), '--step') 1).code -ceq 'not_found') `
        'an explicit step was accepted without a guide'
    Assert-FixtureCondition ((Control @('click', (Get-InputPointArgument $End), '--double', '--count', '0') 2).code -ceq 'invalid_request') `
        'an invalid count was accepted with --double'
    Assert-FixtureCondition (Test-FixtureEqual (Control @('cursor')) $before) 'a rejected click moved the pointer'
    $guide = Control @('steps', 'start', '--title', 'Input fixture')
    Assert-InputOwner $Executable $Info $Rect $Start
    $clicked = Control @('click', (Get-InputPointArgument $Start), '--description', 'Fixture click')
    $status = Control @('steps', 'status')
    Assert-FixtureCondition ((Test-FixtureEqual $clicked.at (Get-InputPointResult $Start)) -and
        (Test-FixtureEqual $clicked.step.at (Get-InputPointResult $Start)) -and $clicked.step.index -eq 1 -and
        $clicked.step.description -ceq 'Fixture click' -and $status.id -ceq $guide.id -and $status.steps.Count -eq 1) `
        'the click did not create its guide step' @($clicked, $status)
    [void](Control @('click', (Get-InputPointArgument $Start), '--no-step'))
    Assert-FixtureCondition ((Control @('steps', 'status')).steps.Count -eq 1) '--no-step unexpectedly added a guide step'
    [void](Control @('steps', 'cancel'))
    Assert-FixtureCondition (@(Get-ChildItem -LiteralPath (Join-Path $Root 'library') -Filter '*.png').Count -eq 0) `
        'the discarded guide saved a display screenshot'
    Assert-FixtureCondition ((Control @('move', '--at', '2147483647,0') 2).code -ceq 'invalid_request') `
        'an off-screen pointer was accepted'
    Assert-FixtureCondition ((Control @('key', 'cmd++a') 2).code -ceq 'invalid_request') `
        'an invalid key combination was accepted'
    $beforeBytes = [Convert]::ToBase64String([IO.File]::ReadAllBytes($statePath))
    $other = Start-InputFixture $Executable $Root 'other'
    $failure = $null
    try {
        Assert-FixtureCondition ((Control @('type', 'WRONG', '--window', $Window, '--no-activate') 1).code -ceq 'conflict') `
            'typing into a background window was not rejected'
        Assert-FixtureCondition ([Convert]::ToBase64String([IO.File]::ReadAllBytes($statePath)) -ceq $beforeBytes -and
            -not [IO.File]::Exists($other.StatePath)) 'typing into a background window sent unexpected input'
    }
    catch { $failure = $_.Exception.Message }
    finally { Complete-InputCleanup @($other.Process) $failure }
    [void](Control @('focus', '--window', $Window))
    Assert-InputOwner $Executable $Info $Rect $Start
    [void](Control @('move', (Get-InputPointArgument $Start), '--glide-ms', '0'))
}

if ($MyInvocation.InvocationName -ceq '.') { return }

$binaryPath = Resolve-FixtureFile $Binary
$script:ControlCli = Resolve-FixtureFile $Cli
$hostPath = Resolve-FixtureFile $HostLibrary
$root = New-FixtureRoot $Output 'jiffshot-input-'
try {
    $script:InputProbe = New-DragInputProbe
    if ((Test-AnyDragButton $script:InputProbe.Buttons()) -or $script:InputProbe.Modifiers()) {
        throw 'release held mouse buttons and modifiers before the input fixture'
    }
    Initialize-FixtureDirectories $root
    $script:ControlEnvironment = New-FixtureEnvironment $root $hostPath 'JIFFSHOT_UITEST_CONTROL'
    $script:ControlEnvironment['JIFFSHOT_UITEST_KEEP'] = '1'
    $executable = Join-Path $root 'input-fixture'
    [void](Invoke-MacCommand -File 'xcrun' -Arguments @(
        'clang', '-fobjc-arc', '-framework', 'Cocoa',
        (Join-Path $PSScriptRoot 'fixtures/control-input/Fixture.m'), '-o', $executable
    ))
    $script:InputCleaning = $false
    $script:InputInterrupt = New-DragInterruptGuard
    try {
        $app = Start-FixtureProcess -File $binaryPath -Environment $script:ControlEnvironment `
            -LogPath (Join-Path $root 'process.log')
        $failure = $null
        try {
            $status = Wait-ControlServer $script:ControlCli $script:ControlEnvironment $app
            Assert-FixtureCondition ($status.pid -eq $app.Process.Id) 'the input fixture connected to the wrong app'
            Invoke-InputChecks $root $app $executable
        }
        catch { $failure = $_.Exception.Message }
        finally {
            $script:InputCleaning = $true
            Complete-InputCleanup @($app) $failure
        }
        $script:InputInterrupt.ThrowIfInterrupted()
    }
    finally { $script:InputInterrupt.Dispose() }
}
catch {
    [Console]::Error.WriteLine("$($_.Exception.Message)`nFixture retained at $root")
    throw
}
if ($Output) { Write-Host "Input fixture passed: $root" }
else { Remove-FixtureRoot $root; Write-Host 'Input fixture passed' }
