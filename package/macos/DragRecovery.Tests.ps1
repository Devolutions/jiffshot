#Requires -Version 7.4

Import-Module (Join-Path $PSScriptRoot 'JiffshotMacDrag.psm1') -Force -DisableNameChecking
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacFixture.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacPackage.psm1') -Force

Describe 'macOS drag recovery' {
    InModuleScope JiffshotMacDrag {
        BeforeEach {
            $script:Work = Join-Path $TestDrive ([guid]::NewGuid().ToString('N'))
            [void][IO.Directory]::CreateDirectory($script:Work)
            $script:Receiver = Join-Path $script:Work 'receiver'
            $script:FixtureEnvironment = @{ fixture = 'environment' }
            $script:Probe = [pscustomobject]@{
                Point = [double[]]@(120, 200)
                ButtonState = [bool[]]@($true, $false, $false)
                ModifierState = $false
                Restored = [Collections.Generic.List[object]]::new()
            }
            $script:Probe | Add-Member ScriptMethod Position { return ,$this.Point }
            $script:Probe | Add-Member ScriptMethod Buttons { return ,$this.ButtonState }
            $script:Probe | Add-Member ScriptMethod Modifiers { return $this.ModifierState }
            $script:Probe | Add-Member ScriptMethod Restore {
                param($point)
                $this.Restored.Add([double[]]$point)
                $this.Point = [double[]]$point
            }
            [IO.File]::WriteAllText((Join-Path $script:Work 'drag-active.json'), '[10,20,100,200,120,200,true]')
            Mock Invoke-MacCommand {
                $script:Probe.ButtonState = [bool[]]@($false, $false, $false)
                [pscustomobject]@{ ExitCode = 0; Stdout = "released the fixture mouse button`n"; Stderr = '' }
            }
        }

        It 'recovers at a known source and restores the original pointer' {
            $result = Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment
            $result.Attempted | Should -BeTrue
            $result.Restored | Should -BeTrue
            $script:Probe.Restored[0] | Should -Be @(10, 20)
            Test-Path -LiteralPath (Join-Path $script:Work 'drag-active.json') | Should -BeFalse
            Should -Invoke Invoke-MacCommand -Times 1 -Exactly -ParameterFilter {
                $File -ceq $script:Receiver -and $Arguments[0] -ceq '--recover-synthetic-button' -and
                [double]$Arguments[1] -eq 120 -and [double]$Arguments[2] -eq 200 -and
                $Environment['fixture'] -ceq 'environment'
            }
        }

        It 'recovers at the point before a planned warp' {
            $script:Probe.Point = [double[]]@(100, 200)
            $result = Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment
            $result.Attempted | Should -BeTrue
            $result.Restored | Should -BeTrue
        }

        It 'restores an interrupted fixture without a posted press' {
            [IO.File]::WriteAllText((Join-Path $script:Work 'drag-active.json'), '[10,20,100,200,120,200,false]')
            $script:Probe.ButtonState = [bool[]]@($false, $false, $false)
            $result = Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment
            $result.Attempted | Should -BeFalse
            $result.Restored | Should -BeTrue
            Should -Invoke Invoke-MacCommand -Times 0 -Exactly
        }

        It 'refuses a button detected before the fixture press' {
            [IO.File]::WriteAllText((Join-Path $script:Work 'drag-active.json'), '[10,20,100,200,120,200,false]')
            { Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment } |
                Should -Throw '*before the fixture press*'
            Test-Path -LiteralPath (Join-Path $script:Work 'drag-active.json') | Should -BeTrue
            Should -Invoke Invoke-MacCommand -Times 0 -Exactly
        }

        It 'recovers a finished receiver after a verified handoff' {
            [IO.File]::WriteAllText((Join-Path $script:Work 'drag-receiver-started'), '')
            [IO.File]::WriteAllText((Join-Path $script:Work 'drag-receiver-finished'), '')
            [IO.File]::WriteAllText((Join-Path $script:Work 'drag-pointer.json'), '[120,200,300,400,true]')
            $script:Probe.Point = [double[]]@(300, 400)
            $result = Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment
            $result.Attempted | Should -BeTrue
            $result.Restored | Should -BeTrue
        }

        It 'refuses input while a receiver may still be moving' {
            [IO.File]::WriteAllText((Join-Path $script:Work 'drag-receiver-started'), '')
            [IO.File]::WriteAllText((Join-Path $script:Work 'drag-pointer.json'), '[120,200,300,400,true]')
            $script:Probe.Point = [double[]]@(300, 400)
            { Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment } |
                Should -Throw '*receiver may still be active*'
            Test-Path -LiteralPath (Join-Path $script:Work 'drag-active.json') | Should -BeTrue
            Should -Invoke Invoke-MacCommand -Times 0 -Exactly
        }

        It 'recovers after an orphaned receiver exits' {
            [IO.File]::WriteAllText((Join-Path $script:Work 'drag-receiver-started'), '')
            [IO.File]::WriteAllText((Join-Path $script:Work 'drag-receiver-exited'), '')
            [IO.File]::WriteAllText((Join-Path $script:Work 'drag-pointer.json'), '[120,200,300,400,true]')
            $script:Probe.Point = [double[]]@(300, 400)
            $result = Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment
            $result.Attempted | Should -BeTrue
            $result.Restored | Should -BeTrue
        }

        It 'waits for the receiver even without an active drag marker' {
            Remove-Item -LiteralPath (Join-Path $script:Work 'drag-active.json')
            [IO.File]::WriteAllText((Join-Path $script:Work 'drag-receiver-started'), '')
            $script:Probe.ButtonState = [bool[]]@($false, $false, $false)
            { Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment } |
                Should -Throw '*receiver may still be active*'
            Should -Invoke Invoke-MacCommand -Times 0 -Exactly
        }

        It 'refuses input after unrecognized pointer movement' {
            $script:Probe.Point = [double[]]@(500, 600)
            { Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment } |
                Should -Throw '*pointer moved away*'
            Test-Path -LiteralPath (Join-Path $script:Work 'drag-active.json') | Should -BeTrue
            Should -Invoke Invoke-MacCommand -Times 0 -Exactly
        }

        It 'refuses input when another button or modifier is active' {
            $script:Probe.ButtonState = [bool[]]@($true, $true, $false)
            { Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment } |
                Should -Throw '*other input is active*'
            $script:Probe.ButtonState = [bool[]]@($true, $false, $false)
            $script:Probe.ModifierState = $true
            { Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment } |
                Should -Throw '*other input is active*'
            Should -Invoke Invoke-MacCommand -Times 0 -Exactly
        }

        It 'refuses input without an active fixture drag' {
            Remove-Item -LiteralPath (Join-Path $script:Work 'drag-active.json')
            { Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment } |
                Should -Throw '*without an active fixture drag*'
            Should -Invoke Invoke-MacCommand -Times 0 -Exactly
        }

        It 'rejects malformed or unmatched handoff state' {
            [IO.File]::WriteAllText((Join-Path $script:Work 'drag-receiver-started'), '')
            [IO.File]::WriteAllText((Join-Path $script:Work 'drag-receiver-finished'), '')
            [IO.File]::WriteAllText((Join-Path $script:Work 'drag-pointer.json'), '[1,2,300,400,true]')
            { Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment } |
                Should -Throw '*starting point does not match*'
            [IO.File]::WriteAllText((Join-Path $script:Work 'drag-pointer.json'), '[120,200,300,400,"true"]')
            { Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment } |
                Should -Throw '*invalid fixture state*'
            Should -Invoke Invoke-MacCommand -Times 0 -Exactly
        }

        It 'preserves a pointer moved during recovery' {
            Mock Invoke-MacCommand {
                $script:Probe.ButtonState = [bool[]]@($false, $false, $false)
                $script:Probe.Point = [double[]]@(500, 600)
                [pscustomobject]@{ ExitCode = 0; Stdout = 'released the fixture mouse button'; Stderr = '' }
            }
            $result = Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment
            $result.Attempted | Should -BeTrue
            $result.Restored | Should -BeFalse
            $script:Probe.Restored.Count | Should -Be 0
        }

        It 'does not restore when new input starts during recovery' {
            Mock Invoke-MacCommand {
                $script:Probe.ButtonState = [bool[]]@($false, $false, $false)
                $script:Probe.ModifierState = $true
                [pscustomobject]@{ ExitCode = 0; Stdout = 'released the fixture mouse button'; Stderr = '' }
            }
            $result = Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment
            $result.Restored | Should -BeFalse
            $script:Probe.Restored.Count | Should -Be 0
        }

        It 'reports failed or timed-out recovery without clearing the guard' {
            Mock Invoke-MacCommand { [pscustomobject]@{ ExitCode = 1; Stdout = ''; Stderr = 'the recovery window is covered' } }
            { Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment } |
                Should -Throw '*recovery window is covered*'
            Test-Path -LiteralPath (Join-Path $script:Work 'drag-active.json') | Should -BeTrue
            Mock Invoke-MacCommand { throw 'timed out' }
            { Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment } |
                Should -Throw '*recovery timed out*'
            Test-Path -LiteralPath (Join-Path $script:Work 'drag-active.json') | Should -BeTrue
        }

        It 'does not trust invalid UTF-8 recovery state' {
            [IO.File]::WriteAllBytes((Join-Path $script:Work 'drag-active.json'), [byte[]]@(255))
            { Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment } |
                Should -Throw '*invalid fixture state*'
            Should -Invoke Invoke-MacCommand -Times 0 -Exactly
        }

        It 'does not treat an inaccessible state path or invalid marker as absent' {
            $missingDirectory = Join-Path $script:Work 'missing/drag-active.json'
            { Read-DragState $missingDirectory 7 $true } | Should -Throw '*could not read fixture state*'
            [void][IO.Directory]::CreateDirectory((Join-Path $script:Work 'drag-receiver-started'))
            { Recover-InterruptedDrag $script:Work $script:Probe $script:Receiver $script:FixtureEnvironment } |
                Should -Throw '*is not a file*'
            Should -Invoke Invoke-MacCommand -Times 0 -Exactly
        }

        It 'waits for a fixture-owned press before interrupting the source' {
            $process = [pscustomobject]@{ ExitCode = $null }
            $process | Add-Member ScriptMethod get_HasExited { return $false }
            $fixture = [pscustomobject]@{ Process = $process }
            $interrupt = [pscustomobject]@{ Calls = 0 }
            $interrupt | Add-Member ScriptMethod ThrowIfInterrupted { $this.Calls++ }
            Wait-DragFixturePress $fixture $script:Work $script:Probe $interrupt $false
            $interrupt.Calls | Should -Be 1
            Should -Invoke Invoke-MacCommand -Times 0 -Exactly
        }

        It 'waits for the receiver handoff and honors interruption while waiting' {
            Mock Start-Sleep {}
            $process = [pscustomobject]@{ ExitCode = $null }
            $process | Add-Member ScriptMethod get_HasExited { return $false }
            $fixture = [pscustomobject]@{ Process = $process }
            $interrupt = [pscustomobject]@{ Calls = 0; CancelAfter = 2 }
            $interrupt | Add-Member ScriptMethod ThrowIfInterrupted {
                $this.Calls++
                if ($this.Calls -ge $this.CancelAfter) { throw 'fixture interrupted' }
            }
            { Wait-DragFixturePress $fixture $script:Work $script:Probe $interrupt $true } |
                Should -Throw '*fixture interrupted*'
            [IO.File]::WriteAllText((Join-Path $script:Work 'drag-pointer.json'), '[120,200,300,400,true]')
            $interrupt.Calls = 0
            { Wait-DragFixturePress $fixture $script:Work $script:Probe $interrupt $true } | Should -Not -Throw
            $interrupt.Calls | Should -Be 1
            Should -Invoke Invoke-MacCommand -Times 0 -Exactly
        }

        It 'reports source exit before an armed fixture press' {
            $process = [pscustomobject]@{ ExitCode = 7 }
            $process | Add-Member ScriptMethod get_HasExited { return $true }
            $fixture = [pscustomobject]@{ Process = $process }
            $interrupt = [pscustomobject]@{}
            $interrupt | Add-Member ScriptMethod ThrowIfInterrupted {}
            { Wait-DragFixturePress $fixture $script:Work $script:Probe $interrupt $false } |
                Should -Throw '*exited before its synthetic press: 7*'
            Should -Invoke Invoke-MacCommand -Times 0 -Exactly
        }

        It 'reports an unavailable source exit state before waiting for a press' {
            $process = [pscustomobject]@{}
            $process | Add-Member ScriptMethod get_HasExited { throw 'exit state unavailable' }
            $fixture = [pscustomobject]@{ Process = $process }
            $interrupt = [pscustomobject]@{}
            $interrupt | Add-Member ScriptMethod ThrowIfInterrupted {}
            { Wait-DragFixturePress $fixture $script:Work $script:Probe $interrupt $false } |
                Should -Throw '*exit state unavailable*'
            Should -Invoke Invoke-MacCommand -Times 0 -Exactly
        }
    }
}

Describe 'macOS drag fixture process cleanup' {
    InModuleScope JiffshotMacDrag {
        BeforeEach {
            $process = [pscustomobject]@{ Exited = $true }
            $process | Add-Member ScriptMethod get_HasExited { return $this.Exited }
            $script:Fixture = [pscustomobject]@{
                Process = $process
                Completed = $false
                ExitCode = 0
            }
            $script:Probe = [pscustomobject]@{}
            $script:Interrupt = [pscustomobject]@{}
            $script:Environment = @{ fixture = 'isolated' }
            Mock Wait-DragFixtureExit { $true }
            Mock Wait-DragFixturePress {}
            Mock Stop-FixtureProcess {}
            Mock Recover-InterruptedDrag { [pscustomobject]@{ Attempted = $true; Restored = $true } }
        }

        It 'returns the exit status and recovery after a normal run' {
            $result = Invoke-DragFixtureProcess $script:Fixture '/fixture' $script:Probe '/receiver' `
                $script:Environment $script:Interrupt $false $false
            $result.ExitCode | Should -Be 0
            $result.TimedOut | Should -BeFalse
            $result.Recovery.Restored | Should -BeTrue
            Should -Invoke Wait-DragFixtureExit -Times 1 -Exactly -ParameterFilter { $TimeoutSeconds -eq 90 }
            Should -Invoke Stop-FixtureProcess -Times 1 -Exactly
            Should -Invoke Recover-InterruptedDrag -Times 1 -Exactly
        }

        It 'waits for the owned press during timeout simulations' {
            $result = Invoke-DragFixtureProcess $script:Fixture '/fixture' $script:Probe '/receiver' `
                $script:Environment $script:Interrupt $false $true
            $result.TimedOut | Should -BeTrue
            Should -Invoke Wait-DragFixturePress -Times 1 -Exactly -ParameterFilter { $ReceiverActive }
            Should -Invoke Wait-DragFixtureExit -Times 0 -Exactly
            Should -Invoke Recover-InterruptedDrag -Times 1 -Exactly
        }

        It 'cleans up an unexpected timeout before reporting it' {
            Mock Wait-DragFixtureExit { $false }
            $result = Invoke-DragFixtureProcess $script:Fixture '/fixture' $script:Probe '/receiver' `
                $script:Environment $script:Interrupt $false $false
            $result.TimedOut | Should -BeTrue
            Should -Invoke Stop-FixtureProcess -Times 1 -Exactly
            Should -Invoke Recover-InterruptedDrag -Times 1 -Exactly
        }

        It 'does not recover input for a self-directed fixture' {
            $result = Invoke-DragFixtureProcess $script:Fixture '/fixture' $null '/receiver' `
                $script:Environment $script:Interrupt $false $false
            $result.Recovery | Should -BeNullOrEmpty
            Should -Invoke Stop-FixtureProcess -Times 1 -Exactly
            Should -Invoke Recover-InterruptedDrag -Times 0 -Exactly
        }

        It 'recovers before reporting an interrupted wait' {
            Mock Wait-DragFixtureExit { throw 'wait interrupted' }
            { Invoke-DragFixtureProcess $script:Fixture '/fixture' $script:Probe '/receiver' `
                $script:Environment $script:Interrupt $false $false } | Should -Throw '*wait interrupted*'
            Should -Invoke Stop-FixtureProcess -Times 1 -Exactly
            Should -Invoke Recover-InterruptedDrag -Times 1 -Exactly
        }

        It 'recovers an exited source and reports wait, stop, and recovery errors together' {
            Mock Wait-DragFixtureExit { throw 'wait failed' }
            Mock Stop-FixtureProcess { throw 'log drain failed' }
            Mock Recover-InterruptedDrag { throw 'recovery failed' }
            { Invoke-DragFixtureProcess $script:Fixture '/fixture' $script:Probe '/receiver' `
                $script:Environment $script:Interrupt $false $false } |
                Should -Throw '*wait failed*log drain failed*recovery failed*'
            Should -Invoke Recover-InterruptedDrag -Times 1 -Exactly
        }

        It 'does not recover while the source might still be active' {
            $script:Fixture.Process.Exited = $false
            Mock Stop-FixtureProcess { throw 'could not terminate source' }
            { Invoke-DragFixtureProcess $script:Fixture '/fixture' $script:Probe '/receiver' `
                $script:Environment $script:Interrupt $false $false } |
                Should -Throw '*could not terminate source*source may still be active*'
            Should -Invoke Recover-InterruptedDrag -Times 0 -Exactly
        }

        It 'does not recover when source exit cannot be verified' {
            $script:Fixture.Process = [pscustomobject]@{}
            $script:Fixture.Process | Add-Member ScriptMethod get_HasExited { throw 'exit state unavailable' }
            Mock Stop-FixtureProcess { throw 'could not terminate source' }
            { Invoke-DragFixtureProcess $script:Fixture '/fixture' $script:Probe '/receiver' `
                $script:Environment $script:Interrupt $false $false } |
                Should -Throw '*could not terminate source*exit state unavailable*source may still be active*'
            Should -Invoke Recover-InterruptedDrag -Times 0 -Exactly
        }

        It 'recovers after a completed source even if the process object is unavailable' {
            $script:Fixture.Completed = $true
            $script:Fixture.Process = [pscustomobject]@{}
            $script:Fixture.Process | Add-Member ScriptMethod get_HasExited { throw 'disposed process' }
            Mock Stop-FixtureProcess { throw 'log disposal failed' }
            { Invoke-DragFixtureProcess $script:Fixture '/fixture' $script:Probe '/receiver' `
                $script:Environment $script:Interrupt $false $false } | Should -Throw '*log disposal failed*'
            Should -Invoke Recover-InterruptedDrag -Times 1 -Exactly
        }
    }
}

Describe 'macOS drag driver interruptions' {
    It 'finishes cleanup when a second <SignalName> signal arrives' -TestCases @(
        @{ Signal = 15; SignalName = 'termination' }
        @{ Signal = 2; SignalName = 'interrupt' }
    ) {
        param($Signal, $SignalName)
        $environment = @{}
        foreach ($entry in [Environment]::GetEnvironmentVariables().GetEnumerator()) {
            $environment[[string]$entry.Key] = [string]$entry.Value
        }
        $signalRoot = Join-Path $TestDrive $SignalName
        [void][IO.Directory]::CreateDirectory($signalRoot)
        $environment['DRAG_MODULE'] = Join-Path $PSScriptRoot 'JiffshotMacDrag.psm1'
        $environment['DRAG_READY'] = Join-Path $signalRoot 'ready'
        $environment['DRAG_CLEANING'] = Join-Path $signalRoot 'cleaning'
        $environment['DRAG_CLEANED'] = Join-Path $signalRoot 'cleaned'
        $childScript = @'
Import-Module $env:DRAG_MODULE -Force -DisableNameChecking
$guard = New-DragInterruptGuard
try {
    try {
        [IO.File]::WriteAllText($env:DRAG_READY, 'ready')
        while ($true) {
            $guard.ThrowIfInterrupted()
            Start-Sleep -Milliseconds 20
        }
    }
    finally {
        [IO.File]::WriteAllText($env:DRAG_CLEANING, 'cleaning')
        Start-Sleep -Milliseconds 500
        [IO.File]::WriteAllText($env:DRAG_CLEANED, 'cleaned')
    }
}
finally { $guard.Dispose() }
'@
        $executable = (Get-Process -Id $PID).Path
        $child = Start-FixtureProcess -File $executable -Arguments @('-NoLogo', '-NoProfile', '-Command', $childScript) `
            -Environment $environment
        try {
            $clock = [Diagnostics.Stopwatch]::StartNew()
            while (-not [IO.File]::Exists($environment['DRAG_READY']) -and $clock.Elapsed.TotalSeconds -lt 10) {
                if ($child.Process.get_HasExited()) { throw 'drag signal fixture exited before initialization' }
                Start-Sleep -Milliseconds 20
            }
            [IO.File]::Exists($environment['DRAG_READY']) | Should -BeTrue
            [JiffshotFixture.Native]::Kill($child.Process.Id, $Signal) | Should -Be 0
            $clock.Restart()
            while (-not [IO.File]::Exists($environment['DRAG_CLEANING']) -and $clock.Elapsed.TotalSeconds -lt 10) {
                if ($child.Process.get_HasExited()) { throw 'drag signal fixture exited before cleanup' }
                Start-Sleep -Milliseconds 20
            }
            [IO.File]::Exists($environment['DRAG_CLEANING']) | Should -BeTrue
            [JiffshotFixture.Native]::Kill($child.Process.Id, $Signal) | Should -Be 0
            $result = Wait-FixtureProcess $child 10
            if (-not [IO.File]::Exists($environment['DRAG_CLEANED'])) {
                throw "cleanup was interrupted (exit $($result.ExitCode)): $($result.Stdout) $($result.Stderr)"
            }
            $result.ExitCode | Should -Not -Be 0
            if ($Signal -eq 15) { $result.Stderr | Should -Match 'fixture was interrupted' }
        }
        finally { Stop-FixtureProcess $child }
    }

    It 'rejects incompatible failure injections before accessing the fixture' {
        $executable = (Get-Process -Id $PID).Path
        $result = Invoke-MacCommand -File $executable -Arguments @(
            '-NoLogo', '-NoProfile', '-File', (Join-Path $PSScriptRoot 'library_drag_selftest.ps1'),
            '-Binary', 'unused', '-HostLibrary', 'unused', '-ExternalDrop',
            '-SimulateReceiverFailure', '-SimulateSourceOwnerFailure', '-SimulatePointerChange'
        ) -AllowFailure
        $result.ExitCode | Should -Not -Be 0
        $result.Stderr | Should -Match 'cannot be combined'
    }
}
