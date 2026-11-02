#Requires -Version 7.4

BeforeAll {
    . (Join-Path $PSScriptRoot 'input_selftest.ps1') -Binary 'unused' -Cli 'unused' -HostLibrary 'unused'
}

Describe 'macOS input fixture helpers' {
    BeforeEach {
        $script:InputCleaning = $false
        $script:InputRecoveryBlocked = $false
        $script:ControlCli = '/fixture/jiffshot-cli'
        $script:ControlEnvironment = @{ fixture = 'isolated' }
        $script:InputInterrupt = [pscustomobject]@{ Interrupted = $false }
        $script:InputInterrupt | Add-Member ScriptMethod ThrowIfInterrupted {
            if ($this.Interrupted) { throw 'fixture interrupted' }
        }
        $script:InputProbe = [pscustomobject]@{
            ButtonState = [bool[]]@($false, $false, $false)
            ModifierState = $false
        }
        $script:InputProbe | Add-Member ScriptMethod Buttons { return ,$this.ButtonState }
        $script:InputProbe | Add-Member ScriptMethod Modifiers { return $this.ModifierState }
    }

    It 'validates the fixture binary when invoked as a script' {
        $executable = (Get-Process -Id $PID).Path
        $missing = Join-Path $TestDrive 'missing-jiffshot'
        $result = Invoke-MacCommand -File $executable -Arguments @(
            '-NoLogo', '-NoProfile', '-File', (Join-Path $PSScriptRoot 'input_selftest.ps1'),
            '-Binary', $missing, '-Cli', 'unused', '-HostLibrary', 'unused'
        ) -TimeoutSeconds 10 -AllowFailure
        $result.ExitCode | Should -Not -Be 0
        $result.Stderr | Should -Match 'Fixture file not found'
    }

    It 'formats points and filters events after a sequence boundary' {
        Get-InputPointArgument @(12, -8) | Should -BeExactly '12,-8'
        Test-FixtureEqual (Get-InputPointResult @(12, -8)) @{ x = 12; y = -8 } | Should -BeTrue
        $state = @{ events = @(@{ name = 'move' }, @{ name = 'leftUp' }, @{ name = 'move' }) }
        $events = @(Get-InputEvents $state 1 'move')
        $events.Count | Should -Be 1
        $events[0].name | Should -BeExactly 'move'
    }

    It 'checks fixture ownership using the correct reference grid' {
        Mock Invoke-MacCommand { [pscustomobject]@{ Stdout = "43`n" } }
        $info = @{ window = 43; bounds = @{ Width = 100; Height = 50 } }
        $rect = @{ width = 200; height = 100 }
        Assert-InputOwner '/fixture/helper' $info $rect @(50, 30)
        Should -Invoke Invoke-MacCommand -Times 1 -Exactly -ParameterFilter {
            $File -ceq '/fixture/helper' -and $TimeoutSeconds -eq 5 -and
            $Arguments.Count -eq 3 -and $Arguments[0] -ceq '--owner-at' -and
            $Arguments[1] -ceq '25' -and $Arguments[2] -ceq '15'
        }
        Mock Invoke-MacCommand { [pscustomobject]@{ Stdout = "99`n" } }
        { Assert-InputOwner '/fixture/helper' $info $rect @(50, 30) } | Should -Throw '*different window covers the fixture*'
    }

    It 'rejects inconsistent display scaling before inspecting ownership' {
        Mock Invoke-MacCommand {}
        $info = @{ window = 43; bounds = @{ Width = 100; Height = 50 } }
        { Assert-InputOwner '/fixture/helper' $info @{ width = 220; height = 100 } @(50, 30) } |
            Should -Throw '*reference grid is inconsistent*'
        Should -Invoke Invoke-MacCommand -Times 0 -Exactly
    }

    It 'retries expected Accessibility startup errors before returning focus' {
        $script:FocusCalls = 0
        Mock Start-Sleep {}
        Mock Invoke-MacCommand {
            $script:FocusCalls++
            if ($script:FocusCalls -eq 1) {
                [pscustomobject]@{ ExitCode = 1; Stdout = '{"error":{"code":"not_found","message":"not ready"}}'; Stderr = '' }
            }
            else {
                [pscustomobject]@{ ExitCode = 0; Stdout = '{"result":{"window":{"pid":42}}}'; Stderr = '' }
            }
        }
        (Focus-InputReady '123').window.pid | Should -Be 42
        Should -Invoke Invoke-MacCommand -Times 2 -Exactly -ParameterFilter {
            $File -ceq '/fixture/jiffshot-cli' -and $Arguments[0] -ceq '--no-launch' -and
            $Arguments[1] -ceq 'focus' -and $Arguments[3] -ceq '123' -and
            $Environment['fixture'] -ceq 'isolated'
        }
    }

    It 'reports unexpected focus failures without retrying' {
        Mock Start-Sleep {}
        Mock Invoke-MacCommand {
            [pscustomobject]@{ ExitCode = 1; Stdout = '{"error":{"code":"denied","message":"permission denied"}}'; Stderr = '' }
        }
        { Focus-InputReady '123' } | Should -Throw '*permission denied*'
        Should -Invoke Invoke-MacCommand -Times 1 -Exactly
        Should -Invoke Start-Sleep -Times 0 -Exactly
    }

    It 'accepts expected covered-target failures and cleans up the fixture' {
        $other = [pscustomobject]@{
            Process = [pscustomobject]@{ Name = 'covered' }
            StatePath = Join-Path $TestDrive 'covered-state.json'
        }
        Mock Start-InputFixture { return $other }
        Mock Control {
            if ($Arguments[0] -ceq 'cursor') { return @{ x = 12; y = 34 } }
            return @{ code = 'conflict' }
        }
        Mock Stop-FixtureProcess {}
        { Invoke-CoveredAccessibilityCheck $TestDrive '/fixture/helper' '123' } | Should -Not -Throw
        Should -Invoke Control -Times 2 -Exactly -ParameterFilter { $Arguments[0] -ceq 'cursor' }
        foreach ($action in 'click', 'move', 'scroll') {
            Should -Invoke Control -Times 1 -Exactly -ParameterFilter {
                $Arguments[0] -ceq $action -and $Arguments[-1] -ceq '--no-activate' -and $Expected -eq 1
            }
        }
        Should -Invoke Control -Times 1 -Exactly -ParameterFilter {
            $Arguments[0] -ceq 'capture' -and $Arguments[1] -ceq 'scroll' -and $Expected -eq 1
        }
        Should -Invoke Stop-FixtureProcess -Times 1 -Exactly -ParameterFilter { $Fixture -eq $other.Process }
    }

    It 'preserves a covered-target assertion failure and its cleanup failure' {
        $other = [pscustomobject]@{
            Process = [pscustomobject]@{ Name = 'covered' }
            StatePath = Join-Path $TestDrive 'covered-state.json'
        }
        Mock Start-InputFixture { return $other }
        Mock Control {
            if ($Arguments[0] -ceq 'cursor') { return @{ x = 12; y = 34 } }
            if ($Arguments[0] -ceq 'capture') { return @{ code = 'unexpected' } }
            return @{ code = 'conflict' }
        }
        Mock Stop-FixtureProcess { throw 'covered fixture cleanup failed' }
        { Invoke-CoveredAccessibilityCheck $TestDrive '/fixture/helper' '123' } |
            Should -Throw '*scrolling capture accepted a covered window*covered fixture cleanup failed*'
        Should -Invoke Stop-FixtureProcess -Times 1 -Exactly -ParameterFilter { $Fixture -eq $other.Process }
    }

    It 'runs the Accessibility checks through covered and background-window cases' {
        $library = Join-Path $TestDrive 'library'
        [void][IO.Directory]::CreateDirectory($library)
        $statePath = Join-Path $TestDrive 'input-state.json'
        [IO.File]::WriteAllText($statePath, '{}')
        $fixture = [pscustomobject]@{ StatePath = $statePath }
        $other = [pscustomobject]@{
            Process = [pscustomobject]@{ Name = 'other' }
            StatePath = Join-Path $TestDrive 'other-state.json'
        }
        $point = @{ x = 10; y = 20 }
        $save = @{ name = 'Synthetic Save'; control_type = 'button'; center = @(10, 20) }
        $saveAs = @{ name = 'Synthetic Save As'; control_type = 'button'; center = @(20, 20) }
        Mock Start-InputFixture { return $other }
        Mock Stop-FixtureProcess {}
        Mock Assert-InputOwner {}
        Mock Focus-InputReady {}
        Mock Read-FixtureJson { return @{ events = @(); text = '' } }
        Mock Wait-InputState {
            $name = if ($Description -clike '*second*') { 'button-save-as' } else { 'button-save' }
            $state = @{ events = @(@{ name = $name }); text = '' }
            if (-not (& $Predicate $state)) { throw "the fixture did not accept the expected $name event" }
            return $state
        }
        Mock Control {
            switch ($Arguments[0]) {
                'elements' {
                    if ($Arguments -contains '--max') { return @{ elements = @($save); truncated = $true } }
                    if ($Arguments -contains '--name') {
                        if ($Arguments -contains 'synthetic save') { return @{ elements = @($save) } }
                        return @{ elements = @($save, $saveAs) }
                    }
                    return @{ window = @{ hwnd = 123 }; truncated = $false; elements = @($save, $saveAs) }
                }
                'cursor' { return $point }
                'capture' { return @{ code = 'conflict' } }
                'scroll' { return @{ code = 'conflict' } }
                'type' { return @{ code = 'conflict' } }
                'key' { return @{ code = 'invalid_request' } }
                'focus' { return @{} }
                'move' {
                    if ($Arguments -contains '--no-activate') { return @{ code = 'conflict' } }
                    if ($Arguments -contains '--at') { return @{ code = 'invalid_request' } }
                    return @{}
                }
                'click' {
                    if ($Arguments -contains '--no-activate') { return @{ code = 'conflict' } }
                    if ($Arguments -contains '--element') {
                        $name = if ($Arguments -contains '--nth') { 'Synthetic Save As' } else { 'Synthetic Save' }
                        return @{ matched = @{ element = @{ name = $name } } }
                    }
                    if ($Arguments -contains '--step') { return @{ code = 'not_found' } }
                    if ($Arguments -contains '--count') { return @{ code = 'invalid_request' } }
                    if ($Arguments -contains '--description') {
                        return @{ at = $point; step = @{ at = $point; index = 1; description = 'Fixture click' } }
                    }
                    return @{}
                }
                'steps' {
                    if ($Arguments[1] -ceq 'start') { return @{ id = 'guide' } }
                    if ($Arguments[1] -ceq 'status') { return @{ id = 'guide'; steps = @(@{ index = 1 }) } }
                    return @{}
                }
                default { throw "unexpected fixture command: $($Arguments -join ' ')" }
            }
        }

        { Invoke-AccessibilityInputChecks $TestDrive '/fixture/helper' $fixture @{ window = 123 } `
            @{} @(10, 20) @(30, 20) '123' } | Should -Not -Throw
        Should -Invoke Control -Times 1 -Exactly -ParameterFilter { $Arguments[0] -ceq 'capture' }
        Should -Invoke Control -Times 1 -Exactly -ParameterFilter { $Arguments[0] -ceq 'type' }
        Should -Invoke Start-InputFixture -Times 2 -Exactly
        Should -Invoke Stop-FixtureProcess -Times 2 -Exactly
    }

    It 'refuses pointer restoration while a button or modifier is active' {
        Mock Control {}
        $app = [pscustomobject]@{ Process = [pscustomobject]@{ HasExited = $false } }
        $script:InputProbe.ButtonState[1] = $true
        { Restore-InputPointer $TestDrive $app @{ x = 12; y = 34 } } | Should -Throw '*not restoring the pointer*'
        $script:InputProbe.ButtonState[1] = $false
        $script:InputProbe.ModifierState = $true
        { Restore-InputPointer $TestDrive $app @{ x = 12; y = 34 } } | Should -Throw '*not restoring the pointer*'
        Should -Invoke Control -Times 0 -Exactly
    }

    It 'refuses pointer restoration while another input source may still be active' {
        $script:InputRecoveryBlocked = $true
        Mock Control {}
        Mock Start-FixtureProcess {}
        { Restore-InputPointer $TestDrive ([pscustomobject]@{}) @{ x = 12; y = 34 } } |
            Should -Throw '*another input source may still be active*not restoring*'
        Should -Invoke Control -Times 0 -Exactly
        Should -Invoke Start-FixtureProcess -Times 0 -Exactly
    }

    It 'does not restore the pointer while the original input source is still running' {
        Mock Control {}
        $process = [pscustomobject]@{}
        $process | Add-Member ScriptMethod get_HasExited { return $false }
        $app = [pscustomobject]@{ Process = $process }
        Mock Start-FixtureProcess {}
        { Restore-InputPointer $TestDrive $app @{ x = 12; y = -34 } } |
            Should -Throw '*original input source is still active*not restoring*'
        Should -Invoke Control -Times 0 -Exactly
        Should -Invoke Start-FixtureProcess -Times 0 -Exactly
    }

    It 'restores the original point through an owned replacement fixture' {
        $process = [pscustomobject]@{}
        $process | Add-Member ScriptMethod get_HasExited { return $true }
        $app = [pscustomobject]@{ Process = $process; File = '/fixture/jiffshot' }
        $reopened = [pscustomobject]@{ Process = [pscustomobject]@{ Id = 42 } }
        Mock Start-FixtureProcess { return $reopened }
        Mock Wait-ControlServer { @{ pid = 42 } }
        Mock Control {}
        Mock Wait-FixtureProcess { [pscustomobject]@{ ExitCode = 0 } }
        Mock Stop-FixtureProcess {}
        Restore-InputPointer $TestDrive $app @{ x = 12; y = -34 }
        Should -Invoke Control -Times 1 -Exactly -ParameterFilter {
            $Arguments.Count -eq 4 -and $Arguments[0] -ceq 'move' -and $Arguments[1] -ceq '12,-34' -and
            $Arguments[2] -ceq '--glide-ms' -and $Arguments[3] -ceq '0'
        }
        Should -Invoke Control -Times 1 -Exactly -ParameterFilter { $Arguments[0] -ceq 'quit' }
        Should -Invoke Stop-FixtureProcess -Times 1 -Exactly
    }

    It 'does not restore the pointer when the original app exit state is unavailable' {
        Mock Control {}
        Mock Start-FixtureProcess {}
        $process = [pscustomobject]@{}
        $process | Add-Member ScriptMethod get_HasExited { throw 'exit state unavailable' }
        $app = [pscustomobject]@{ Process = $process }
        { Restore-InputPointer $TestDrive $app @{ x = 12; y = 34 } } |
            Should -Throw '*exit state unavailable*'
        Should -Invoke Control -Times 0 -Exactly
        Should -Invoke Start-FixtureProcess -Times 0 -Exactly
    }

    It 'does not move the pointer through a different restoration app' {
        $process = [pscustomobject]@{}
        $process | Add-Member ScriptMethod get_HasExited { return $true }
        $app = [pscustomobject]@{ Process = $process; File = '/fixture/jiffshot' }
        $reopened = [pscustomobject]@{ Process = [pscustomobject]@{ Id = 42 } }
        Mock Start-FixtureProcess { return $reopened }
        Mock Wait-ControlServer { @{ pid = 99 } }
        Mock Control {}
        Mock Stop-FixtureProcess {}
        { Restore-InputPointer $TestDrive $app @{ x = 12; y = 34 } } | Should -Throw '*connected to the wrong app*'
        Should -Invoke Control -Times 0 -Exactly
        Should -Invoke Stop-FixtureProcess -Times 1 -Exactly
    }

    It 'rechecks buttons and modifiers after the replacement app starts' {
        $process = [pscustomobject]@{}
        $process | Add-Member ScriptMethod get_HasExited { return $true }
        $app = [pscustomobject]@{ Process = $process; File = '/fixture/jiffshot' }
        $reopened = [pscustomobject]@{ Process = [pscustomobject]@{ Id = 42 } }
        Mock Start-FixtureProcess { return $reopened }
        Mock Wait-ControlServer {
            $script:InputProbe.ButtonState[0] = $true
            return @{ pid = 42 }
        }
        Mock Control {}
        Mock Stop-FixtureProcess {}
        { Restore-InputPointer $TestDrive $app @{ x = 12; y = 34 } } |
            Should -Throw '*became active*not restoring*'
        Should -Invoke Control -Times 0 -Exactly
        Should -Invoke Stop-FixtureProcess -Times 1 -Exactly
    }

    It 'blocks a restoration retry when the restoration app cannot be stopped' {
        $process = [pscustomobject]@{}
        $process | Add-Member ScriptMethod get_HasExited { return $true }
        $app = [pscustomobject]@{ Process = $process; File = '/fixture/jiffshot' }
        $reopened = [pscustomobject]@{ Process = [pscustomobject]@{ Id = 42 } }
        Mock Start-FixtureProcess { return $reopened }
        Mock Wait-ControlServer { @{ pid = 42 } }
        Mock Control { throw 'move failed' }
        Mock Stop-FixtureProcess { throw 'restoration app cleanup failed' }
        { Restore-InputPointer $TestDrive $app @{ x = 12; y = 34 } } |
            Should -Throw '*move failed*restoration app cleanup failed*'
        $script:InputRecoveryBlocked | Should -BeTrue
    }

    It 'preserves a startup failure when stopping the fixture also fails' {
        $process = [pscustomobject]@{}
        $process | Add-Member ScriptMethod get_HasExited { return $true }
        $fixture = [pscustomobject]@{ Process = $process }
        Mock Start-FixtureProcess { return $fixture }
        Mock Wait-FixtureProcess { [pscustomobject]@{ Stderr = 'fixture startup failed' } }
        Mock Stop-FixtureProcess { throw 'fixture cleanup failed' }
        { Start-InputFixture '/fixture/helper' $TestDrive 'startup' } |
            Should -Throw '*fixture startup failed*fixture cleanup failed*'
        Should -Invoke Stop-FixtureProcess -Times 1 -Exactly
    }

    It 'reports an unavailable fixture exit state during startup and still attempts cleanup' {
        $process = [pscustomobject]@{}
        $process | Add-Member ScriptMethod get_HasExited { throw 'exit state unavailable' }
        $fixture = [pscustomobject]@{ Process = $process }
        Mock Start-FixtureProcess { return $fixture }
        Mock Stop-FixtureProcess {}
        { Start-InputFixture '/fixture/helper' $TestDrive 'unknown-exit' } |
            Should -Throw '*exit state unavailable*'
        Should -Invoke Stop-FixtureProcess -Times 1 -Exactly
    }

    It 'does not query or move the pointer when the scrolling process exit state is unavailable' {
        $process = [pscustomobject]@{}
        $process | Add-Member ScriptMethod get_HasExited { throw 'exit state unavailable' }
        $fixture = [pscustomobject]@{ Process = $process }
        Mock Control {}
        { Wait-InputCursor @(12, 34) $fixture } | Should -Throw '*exit state unavailable*'
        Should -Invoke Control -Times 0 -Exactly
    }

    It 'attempts every input-process cleanup and preserves the original error' {
        $script:Stopped = [Collections.Generic.List[string]]::new()
        Mock Stop-FixtureProcess {
            $script:Stopped.Add($Fixture.Name)
            if ($Fixture.Name -ceq 'scroll') { throw 'scroll cleanup failed' }
            if ($Fixture.Name -ceq 'app') { throw 'app cleanup failed' }
        }
        $processes = @([pscustomobject]@{ Name = 'scroll' }, $null, [pscustomobject]@{ Name = 'app' })
        $failedProcesses = [Collections.Generic.List[object]]::new()
        { Complete-InputCleanup $processes 'input failed' $failedProcesses } |
            Should -Throw '*input failed*scroll cleanup failed*app cleanup failed*'
        $script:Stopped | Should -Be @('scroll', 'app')
        $failedProcesses.Count | Should -Be 2
        $failedProcesses[0].Name | Should -BeExactly 'scroll'
        $failedProcesses[1].Name | Should -BeExactly 'app'
        Should -Invoke Stop-FixtureProcess -Times 2 -Exactly
    }

    It 'waits for the input source to exit before allowing recovery' {
        $process = [pscustomobject]@{ Id = 42 }
        $process | Add-Member ScriptMethod get_HasExited { return $false }
        $app = [pscustomobject]@{ Process = $process }
        Mock Control {
            if ($Arguments[0] -ceq 'status') { return @{ pid = 42 } }
            return @{ quitting = $true }
        }
        Mock Wait-FixtureProcess { [pscustomobject]@{ ExitCode = 0 } }
        $result = Stop-InputSourceForRecovery $app
        $result.Exited | Should -BeTrue
        $result.Error | Should -BeNullOrEmpty
        Should -Invoke Control -Times 1 -Exactly -ParameterFilter { $Arguments[0] -ceq 'status' }
        Should -Invoke Control -Times 1 -Exactly -ParameterFilter { $Arguments[0] -ceq 'quit' }
        Should -Invoke Wait-FixtureProcess -Times 1 -Exactly -ParameterFilter {
            $Fixture -eq $app -and $TimeoutSeconds -eq 45
        }
    }

    It 'observes a real source process exit before allowing recovery' {
        $app = Start-FixtureProcess -File '/bin/sleep' -Arguments @('30')
        Mock Control {
            if ($Arguments[0] -ceq 'status') { return @{ pid = $app.Process.Id } }
            if ([JiffshotFixture.Native]::Kill($app.Process.Id, 15) -ne 0) { throw 'could not signal the test process' }
            return @{ quitting = $true }
        }
        try {
            $result = Stop-InputSourceForRecovery $app
            $result.Exited | Should -BeTrue
            $app.Completed | Should -BeTrue
            $result.Error | Should -Match 'exited unsuccessfully'
        }
        finally { Stop-FixtureProcess $app }
    }

    It 'does not request another exit after the input source has stopped' {
        $process = [pscustomobject]@{}
        $process | Add-Member ScriptMethod get_HasExited { return $true }
        $app = [pscustomobject]@{ Process = $process }
        Mock Control {}
        Mock Wait-FixtureProcess {}
        $result = Stop-InputSourceForRecovery $app
        $result.Exited | Should -BeTrue
        $result.Error | Should -BeNullOrEmpty
        Should -Invoke Control -Times 0 -Exactly
        Should -Invoke Wait-FixtureProcess -Times 0 -Exactly
    }

    It 'preserves shutdown and wait failures when the input source remains active' {
        $process = [pscustomobject]@{ Id = 42 }
        $process | Add-Member ScriptMethod get_HasExited { return $false }
        $app = [pscustomobject]@{ Process = $process }
        Mock Control {
            if ($Arguments[0] -ceq 'status') { return @{ pid = 42 } }
            throw 'quit failed'
        }
        Mock Wait-FixtureProcess { throw 'wait failed' }
        $result = Stop-InputSourceForRecovery $app
        $result.Exited | Should -BeFalse
        $result.Error | Should -Match 'quit failed.*wait failed'
        Should -Invoke Control -Times 2 -Exactly
        Should -Invoke Wait-FixtureProcess -Times 1 -Exactly
    }

    It 'recognizes a source that exits despite a failed shutdown response' {
        $process = [pscustomobject]@{ Id = 42 }
        $process | Add-Member ScriptMethod get_HasExited { return $false }
        $app = [pscustomobject]@{ Process = $process }
        Mock Control {
            if ($Arguments[0] -ceq 'status') { return @{ pid = 42 } }
            throw 'quit response failed'
        }
        Mock Wait-FixtureProcess { [pscustomobject]@{ ExitCode = 0 } }
        $result = Stop-InputSourceForRecovery $app
        $result.Exited | Should -BeTrue
        $result.Error | Should -Match 'quit response failed'
    }

    It 'reports an unsuccessful source exit without treating it as still running' {
        $process = [pscustomobject]@{ Id = 42 }
        $process | Add-Member ScriptMethod get_HasExited { return $false }
        $app = [pscustomobject]@{ Process = $process }
        Mock Control {
            if ($Arguments[0] -ceq 'status') { return @{ pid = 42 } }
            return @{ quitting = $true }
        }
        Mock Wait-FixtureProcess { [pscustomobject]@{ ExitCode = 7 } }
        $result = Stop-InputSourceForRecovery $app
        $result.Exited | Should -BeTrue
        $result.Error | Should -Match 'exited unsuccessfully: 7'
    }

    It 'rechecks source exit after a wait failure and retains the failure' {
        $process = [pscustomobject]@{ Id = 42; Checks = 0 }
        $process | Add-Member ScriptMethod get_HasExited { $this.Checks++; return $this.Checks -gt 1 }
        $app = [pscustomobject]@{ Process = $process }
        Mock Control {
            if ($Arguments[0] -ceq 'status') { return @{ pid = 42 } }
            return @{ quitting = $true }
        }
        Mock Wait-FixtureProcess { throw 'log drain failed' }
        $result = Stop-InputSourceForRecovery $app
        $result.Exited | Should -BeTrue
        $result.Error | Should -Match 'log drain failed'
        $process.Checks | Should -Be 2
    }

    It 'refuses recovery if the source exit recheck fails after a wait failure' {
        $process = [pscustomobject]@{ Id = 42; Checks = 0 }
        $process | Add-Member ScriptMethod get_HasExited {
            $this.Checks++
            if ($this.Checks -eq 1) { return $false }
            throw 'exit recheck failed'
        }
        $app = [pscustomobject]@{ Process = $process }
        Mock Control {
            if ($Arguments[0] -ceq 'status') { return @{ pid = 42 } }
            return @{ quitting = $true }
        }
        Mock Wait-FixtureProcess { throw 'wait failed' }
        $result = Stop-InputSourceForRecovery $app
        $result.Exited | Should -BeFalse
        $result.Error | Should -Match 'wait failed.*exit recheck failed'
    }

    It 'does not ask a different control-server owner to exit' {
        $process = [pscustomobject]@{ Id = 42 }
        $process | Add-Member ScriptMethod get_HasExited { return $false }
        $app = [pscustomobject]@{ Process = $process }
        Mock Control { @{ pid = 99 } }
        Mock Wait-FixtureProcess { throw 'wait failed' }
        $result = Stop-InputSourceForRecovery $app
        $result.Exited | Should -BeFalse
        $result.Error | Should -Match 'no longer owns the control server.*wait failed'
        Should -Invoke Control -Times 1 -Exactly -ParameterFilter { $Arguments[0] -ceq 'status' }
        Should -Invoke Control -Times 0 -Exactly -ParameterFilter { $Arguments[0] -ceq 'quit' }
    }

    It 'refuses recovery when the source exit state cannot be verified' {
        $process = [pscustomobject]@{}
        $process | Add-Member ScriptMethod get_HasExited { throw 'exit state unavailable' }
        $app = [pscustomobject]@{ Process = $process }
        Mock Control {}
        Mock Wait-FixtureProcess {}
        $result = Stop-InputSourceForRecovery $app
        $result.Exited | Should -BeFalse
        $result.Error | Should -Match 'exit state unavailable'
        Should -Invoke Control -Times 0 -Exactly
        Should -Invoke Wait-FixtureProcess -Times 0 -Exactly
    }

    It 'stops both concurrent typing processes and reports all failures' {
        $script:Stopped = [Collections.Generic.List[string]]::new()
        Mock Start-FixtureProcess {
            [pscustomobject]@{ Name = $Arguments[2] }
        }
        Mock Wait-InputState {}
        Mock Wait-FixtureProcess { throw 'typing wait failed' }
        Mock Stop-FixtureProcess {
            $script:Stopped.Add($Fixture.Name)
            if ($Fixture.Name -ceq 'XYZ') { throw 'second cleanup failed' }
            if ($Fixture.Name -ceq 'ABC') { throw 'first cleanup failed' }
        }
        { Invoke-ConcurrentTypingCheck '/fixture/state' '123' 'typed' } |
            Should -Throw '*typing wait failed*second cleanup failed*first cleanup failed*'
        $script:Stopped | Should -Be @('XYZ', 'ABC')
        Should -Invoke Stop-FixtureProcess -Times 2 -Exactly
    }

    It 'stops the first typing process if starting the second one fails' {
        Mock Start-FixtureProcess {
            if ($Arguments[2] -ceq 'XYZ') { throw 'second start failed' }
            return [pscustomobject]@{ Name = 'ABC' }
        }
        Mock Wait-InputState {}
        Mock Stop-FixtureProcess {}
        { Invoke-ConcurrentTypingCheck '/fixture/state' '123' 'typed' } |
            Should -Throw '*second start failed*'
        Should -Invoke Stop-FixtureProcess -Times 1 -Exactly -ParameterFilter { $Fixture.Name -ceq 'ABC' }
    }

    It 'stops both concurrent typing processes after a successful check' {
        $script:Stopped = [Collections.Generic.List[string]]::new()
        Mock Start-FixtureProcess { [pscustomobject]@{ Name = $Arguments[2] } }
        Mock Wait-InputState {
            if (-not (& $Predicate @{ text = 'typedA' }) -or (& $Predicate @{ text = 'differentA' })) {
                throw 'the concurrent input readiness predicate is incorrect'
            }
        }
        Mock Wait-FixtureProcess { [pscustomobject]@{ ExitCode = 0 } }
        Mock Stop-FixtureProcess { $script:Stopped.Add($Fixture.Name) }
        { Invoke-ConcurrentTypingCheck '/fixture/state' '123' 'typed' } | Should -Not -Throw
        $script:Stopped | Should -Be @('XYZ', 'ABC')
        Should -Invoke Stop-FixtureProcess -Times 2 -Exactly
    }

    It 'stops the shutdown app even when scrolling-process cleanup fails' {
        $script:Stopped = [Collections.Generic.List[string]]::new()
        $app = [pscustomobject]@{ Name = 'app'; Process = [pscustomobject]@{ Id = 42 } }
        $scroll = [pscustomobject]@{ Name = 'scroll' }
        Mock Start-FixtureProcess {
            if ($File -ceq '/fixture/jiffshot') { return $app }
            return $scroll
        }
        Mock Wait-ControlServer { @{ pid = 42 } }
        Mock Focus-InputReady {}
        Mock Get-NativeCursor { return ,([double[]]@(1, 2)) }
        Mock Wait-InputCursor {}
        Mock Control {
            if ($Arguments[0] -ceq 'quit') { throw 'quit failed' }
        }
        Mock Stop-FixtureProcess {
            $script:Stopped.Add($Fixture.Name)
            if ($Fixture.Name -ceq 'scroll') { throw 'scroll cleanup failed' }
        }
        { Invoke-ScrollShutdownCheck $TestDrive '/fixture/jiffshot' '/fixture/helper' '123' @(10, 20) @(30, 20) } |
            Should -Throw '*quit failed*scroll cleanup failed*'
        $script:Stopped | Should -Be @('scroll', 'app')
        $script:InputRecoveryBlocked | Should -BeFalse
        Should -Invoke Stop-FixtureProcess -Times 2 -Exactly
    }

    It 'blocks restoration if the scrolling shutdown app cannot be stopped' {
        $app = [pscustomobject]@{ Name = 'app'; Process = [pscustomobject]@{ Id = 42 } }
        $scroll = [pscustomobject]@{ Name = 'scroll' }
        Mock Start-FixtureProcess {
            if ($File -ceq '/fixture/jiffshot') { return $app }
            return $scroll
        }
        Mock Wait-ControlServer { @{ pid = 42 } }
        Mock Focus-InputReady {}
        Mock Get-NativeCursor { return ,([double[]]@(1, 2)) }
        Mock Wait-InputCursor {}
        Mock Control {
            if ($Arguments[0] -ceq 'quit') { throw 'quit failed' }
        }
        Mock Stop-FixtureProcess {
            if ($Fixture.Name -ceq 'app') { throw 'shutdown app cleanup failed' }
        }
        { Invoke-ScrollShutdownCheck $TestDrive '/fixture/jiffshot' '/fixture/helper' '123' @(10, 20) @(30, 20) } |
            Should -Throw '*quit failed*shutdown app cleanup failed*'
        $script:InputRecoveryBlocked | Should -BeTrue
        Should -Invoke Stop-FixtureProcess -Times 2 -Exactly
    }

    It 'cleans up both processes after a successful scrolling-shutdown check' {
        $script:Stopped = [Collections.Generic.List[string]]::new()
        $app = [pscustomobject]@{ Name = 'app'; Process = [pscustomobject]@{ Id = 42 } }
        $scroll = [pscustomobject]@{ Name = 'scroll' }
        Mock Start-FixtureProcess {
            if ($File -ceq '/fixture/jiffshot') { return $app }
            return $scroll
        }
        Mock Wait-ControlServer { @{ pid = 42 } }
        Mock Focus-InputReady {}
        Mock Get-NativeCursor { return ,([double[]]@(1, 2)) }
        Mock Wait-InputCursor {}
        Mock Control {
            if ($Arguments[0] -ceq 'quit') { return @{ quitting = $true } }
        }
        Mock Wait-FixtureProcess {
            if ($Fixture.Name -ceq 'scroll') {
                return [pscustomobject]@{ ExitCode = 1; Stdout = '{"error":{"code":"conflict"}}'; Stderr = '' }
            }
            return [pscustomobject]@{ ExitCode = 0 }
        }
        Mock Stop-FixtureProcess { $script:Stopped.Add($Fixture.Name) }
        { Invoke-ScrollShutdownCheck $TestDrive '/fixture/jiffshot' '/fixture/helper' '123' @(10, 20) @(30, 20) } |
            Should -Not -Throw
        $script:Stopped | Should -Be @('scroll', 'app')
        Should -Invoke Stop-FixtureProcess -Times 2 -Exactly
    }

    It 'stops the shutdown app if its control server never starts' {
        $app = [pscustomobject]@{ Name = 'app'; Process = [pscustomobject]@{ Id = 42 } }
        Mock Start-FixtureProcess { return $app }
        Mock Wait-ControlServer { throw 'server failed to start' }
        Mock Stop-FixtureProcess {}
        { Invoke-ScrollShutdownCheck $TestDrive '/fixture/jiffshot' '/fixture/helper' '123' @(10, 20) @(30, 20) } |
            Should -Throw '*server failed to start*'
        Should -Invoke Stop-FixtureProcess -Times 1 -Exactly -ParameterFilter { $Fixture.Name -ceq 'app' }
    }

    It 'does not move the pointer if the input fixture fails before starting' {
        Mock Control { @{ x = 12; y = 34 } }
        Mock Start-InputFixture { throw 'fixture startup failed' }
        Mock Restore-InputPointer {}
        Mock Stop-FixtureProcess {}
        $app = [pscustomobject]@{}
        { Invoke-InputChecks $TestDrive $app '/fixture/helper' } | Should -Throw '*fixture startup failed*'
        Should -Invoke Restore-InputPointer -Times 0 -Exactly
        Should -Invoke Stop-FixtureProcess -Times 0 -Exactly
    }

    It 'reports input, restoration, and fixture cleanup failures together' {
        Mock Control {
            if ($Arguments[0] -ceq 'cursor') { return @{ x = 12; y = 34 } }
            throw 'input failed'
        }
        Mock Start-InputFixture {
            [pscustomobject]@{
                Process = [pscustomobject]@{ Process = [pscustomobject]@{ Id = 42 } }
                Info = @{ window = 123 }
                StatePath = '/fixture/state'
            }
        }
        Mock Focus-InputReady { @{ window = @{ pid = 42; rect = @{ x = 0; y = 0; width = 90; height = 60 } } } }
        Mock Assert-InputOwner {}
        Mock Stop-InputSourceForRecovery { [pscustomobject]@{ Exited = $true; Error = '' } }
        Mock Restore-InputPointer {
            $script:InputCleaning | Should -BeTrue
            throw 'restoration failed'
        }
        Mock Stop-FixtureProcess { throw 'cleanup failed' }
        $app = [pscustomobject]@{}
        { Invoke-InputChecks $TestDrive $app '/fixture/helper' } |
            Should -Throw '*input failed*restoration failed*cleanup failed*'
        Should -Invoke Restore-InputPointer -Times 1 -Exactly
        Should -Invoke Stop-FixtureProcess -Times 1 -Exactly
    }

    It 'does not restore the pointer while the input source may still be active' {
        Mock Control {
            if ($Arguments[0] -ceq 'cursor') { return @{ x = 12; y = 34 } }
            throw 'input failed'
        }
        Mock Start-InputFixture {
            [pscustomobject]@{
                Process = [pscustomobject]@{ Process = [pscustomobject]@{ Id = 42 } }
                Info = @{ window = 123 }
                StatePath = '/fixture/state'
            }
        }
        Mock Focus-InputReady { @{ window = @{ pid = 42; rect = @{ x = 0; y = 0; width = 90; height = 60 } } } }
        Mock Assert-InputOwner {}
        Mock Stop-InputSourceForRecovery {
            $script:InputCleaning | Should -BeTrue
            [pscustomobject]@{ Exited = $false; Error = 'shutdown failed' }
        }
        Mock Restore-InputPointer {}
        Mock Stop-FixtureProcess {}
        { Invoke-InputChecks $TestDrive ([pscustomobject]@{}) '/fixture/helper' } |
            Should -Throw '*input failed*shutdown failed*source may still be active*not restoring*'
        Should -Invoke Stop-InputSourceForRecovery -Times 1 -Exactly
        Should -Invoke Restore-InputPointer -Times 0 -Exactly
        Should -Invoke Stop-FixtureProcess -Times 1 -Exactly
    }

    It 'does not restore the pointer if another fixture source could not be stopped' {
        Mock Control {
            if ($Arguments[0] -ceq 'cursor') { return @{ x = 12; y = 34 } }
            throw 'input failed'
        }
        Mock Start-InputFixture {
            [pscustomobject]@{
                Process = [pscustomobject]@{ Process = [pscustomobject]@{ Id = 42 } }
                Info = @{ window = 123 }
                StatePath = '/fixture/state'
            }
        }
        Mock Focus-InputReady { @{ window = @{ pid = 42; rect = @{ x = 0; y = 0; width = 90; height = 60 } } } }
        Mock Assert-InputOwner {}
        Mock Stop-InputSourceForRecovery {
            $script:InputRecoveryBlocked = $true
            [pscustomobject]@{ Exited = $true; Error = '' }
        }
        Mock Restore-InputPointer {}
        Mock Stop-FixtureProcess {}
        { Invoke-InputChecks $TestDrive ([pscustomobject]@{}) '/fixture/helper' } |
            Should -Throw '*input failed*source may still be active*not restoring*'
        Should -Invoke Restore-InputPointer -Times 0 -Exactly
    }

    It 'honors interruptions before sending a command but permits cleanup' {
        Mock Invoke-ControlCommand { @{ value = 42 } }
        $script:InputInterrupt.Interrupted = $true
        { Control @('cursor') } | Should -Throw '*fixture interrupted*'
        Should -Invoke Invoke-ControlCommand -Times 0 -Exactly
        $script:InputCleaning = $true
        (Control @('cursor')).value | Should -Be 42
        Should -Invoke Invoke-ControlCommand -Times 1 -Exactly -ParameterFilter {
            $Cli -ceq '/fixture/jiffshot-cli' -and $Arguments[0] -ceq 'cursor' -and
            $Environment['fixture'] -ceq 'isolated'
        }
    }
}
