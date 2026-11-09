[CmdletBinding()]
param()

# Isolated tests for the harness itself. They do not read/write the registry,
# inspect or manipulate windows, or start Jiffshot.
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'windows_startup_e2e.ps1')

$script:passedCases = 0
function Invoke-Case {
    param([string] $Name, [scriptblock] $Body)
    & $Body
    $script:passedCases++
    Write-Host "PASS $Name"
}
function Assert-True {
    param([bool] $Value, [string] $Message)
    if (-not $Value) { throw $Message }
}
function Assert-Equal {
    param($Expected, $Actual, [string] $Message)
    if ($Expected -cne $Actual) { throw "$Message (expected '$Expected', got '$Actual')" }
}
function Assert-Throws {
    param([scriptblock] $Body, [string] $MessagePart)
    $message = $null
    try { & $Body } catch { $message = $_.Exception.Message }
    if ($null -eq $message -or -not $message.Contains($MessagePart)) {
        throw "Expected an error containing '$MessagePart', got '$message'."
    }
}
function New-TestEntry {
    param(
        [bool] $Present = $false,
        [string] $Value = '',
        [string] $Kind = 'String',
        [string] $View = 'Registry64',
        [string] $KeyPath = 'Software\Microsoft\Windows\CurrentVersion\Run',
        [string] $Name = 'Jiffshot'
    )
    [pscustomobject] @{
        Present = $Present; Value = $Value; Kind = $Kind
        View = $View; KeyPath = $KeyPath; Name = $Name
    }
}
function New-TestRegistryKey {
    param([string[]] $Names, $Value, [Microsoft.Win32.RegistryValueKind] $Kind)
    $key = [pscustomobject] @{
        Names = $Names; Value = $Value; Kind = $Kind; ReadCount = 0; ReadOptions = $null
    }
    $key | Add-Member -MemberType ScriptMethod -Name GetValueNames -Value { return $this.Names }
    $key | Add-Member -MemberType ScriptMethod -Name GetValueKind -Value { param($name) return $this.Kind }
    $key | Add-Member -MemberType ScriptMethod -Name GetValue -Value {
        param($name, $default, $options)
        $this.ReadCount++
        $this.ReadOptions = $options
        return $this.Value
    }
    return $key
}
function New-TestMutex {
    param([bool] $FailRelease = $false)
    $mutex = [pscustomobject] @{ ReleaseCount = 0; DisposeCount = 0; FailRelease = $FailRelease }
    $mutex | Add-Member -MemberType ScriptMethod -Name ReleaseMutex -Value {
        $this.ReleaseCount++
        if ($this.FailRelease) { throw 'simulated mutex release failure' }
    }
    $mutex | Add-Member -MemberType ScriptMethod -Name Dispose -Value { $this.DisposeCount++ }
    return $mutex
}

$binary = 'C:\Portable Jiffshot\' + [char] 0x0394 + '\jiffshot.exe'
$command = '"' + $binary + '" --background'
$owned = New-TestEntry -Present $true -Value $command

Invoke-Case 'consent_requires_explicit_opt_in' {
    Assert-Throws { Assert-DisposableConsent $false 'S-1-5-21-1' 'S-1-5-21-1' $true $true } '-DisposableProfile'
    Assert-Throws {
        & (Join-Path $PSScriptRoot 'windows_startup_e2e.ps1') -Binary 'Z:\not-a-real-test-binary\jiffshot.exe'
    } '-DisposableProfile'
}

Invoke-Case 'consent_requires_matching_sid_and_interactive_windows' {
    Assert-Throws { Assert-DisposableConsent $true '' 'S-1-5-21-1' $true $true } '-ProfileSid'
    Assert-Throws { Assert-DisposableConsent $true 'S-1-5-21-2' 'S-1-5-21-1' $true $true } '-ProfileSid'
    Assert-Throws { Assert-DisposableConsent $true 'S-1-5-21-1' 'S-1-5-21-1' $true $false } 'interactive'
    Assert-Throws { Assert-DisposableConsent $true 'S-1-5-21-1' 'S-1-5-21-1' $false $true } 'interactive'
    Assert-DisposableConsent $true 'S-1-5-21-1' 'S-1-5-21-1' $true $true
}

Invoke-Case 'harness_mutex_is_global_and_user_scoped' {
    Assert-Equal 'Global\Jiffshot.StartupE2E.S-1-5-21-1' `
        (Get-StartupHarnessMutexName 'S-1-5-21-1') 'The harness lock must cover all sessions of this user.'
    Assert-True ((Get-StartupHarnessMutexName 'S-1-5-21-1') -cne
        (Get-StartupHarnessMutexName 'S-1-5-21-2')) 'Different users must not share a profile lock.'
}

Invoke-Case 'binary_location_rejects_msix_and_network_drives' {
    Assert-PortableBinaryLocation 'C:\Portable Jiffshot\jiffshot.exe' ([IO.DriveType]::Fixed)
    Assert-PortableBinaryLocation 'D:\Portable\jiffshot.exe' ([IO.DriveType]::Removable)
    Assert-Throws {
        Assert-PortableBinaryLocation 'Z:\jiffshot.exe' ([IO.DriveType]::Network)
    } 'not an MSIX or network path'
    Assert-Throws {
        Assert-PortableBinaryLocation '\\server\share\jiffshot.exe' ([IO.DriveType]::Unknown)
    } 'not an MSIX or network path'
    Assert-Throws {
        Assert-PortableBinaryLocation 'C:\Program Files\WindowsApps\Jiffshot\jiffshot.exe' ([IO.DriveType]::Fixed)
    } 'not an MSIX or network path'
}

Invoke-Case 'cleanup_releases_mutex_even_when_cleanup_or_release_fails' {
    $mutex = New-TestMutex
    Assert-Equal 'complete' (Invoke-StartupHarnessCleanup $mutex { 'complete' }) 'Cleanup did not run.'
    Assert-Equal 1 $mutex.ReleaseCount 'Successful cleanup did not release the mutex.'
    Assert-Equal 1 $mutex.DisposeCount 'Successful cleanup did not dispose the mutex.'

    $mutex = New-TestMutex
    Assert-Throws { Invoke-StartupHarnessCleanup $mutex { throw 'simulated cleanup failure' } } `
        'simulated cleanup failure'
    Assert-Equal 1 $mutex.ReleaseCount 'A cleanup failure left the mutex held.'
    Assert-Equal 1 $mutex.DisposeCount 'A cleanup failure left the mutex undisposed.'

    $mutex = New-TestMutex $true
    Assert-Throws { Invoke-StartupHarnessCleanup $mutex {} } 'simulated mutex release failure'
    Assert-Equal 1 $mutex.ReleaseCount 'The failing mutex release was not attempted exactly once.'
    Assert-Equal 1 $mutex.DisposeCount 'The mutex was not disposed after its release failed.'
}

Invoke-Case 'preflight_rejects_all_existing_values' {
    $absent = New-TestEntry
    Assert-CleanStartupState @($absent)
    $emptyRun = New-TestEntry -Present $true -Value ''
    Assert-Throws { Assert-CleanStartupState @($absent, $emptyRun) } 'existing Jiffshot value'
    $approval = New-TestEntry -Present $true -Kind 'Binary' `
        -KeyPath 'Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run32'
    Assert-Throws { Assert-CleanStartupState @($approval) } 'existing Jiffshot value'
}

Invoke-Case 'preflight_rejects_existing_instance_indicators' {
    Assert-ClearInstanceSnapshot @() $false 0
    Assert-Throws { Assert-ClearInstanceSnapshot @(73) $false 0 } 'existing Jiffshot instance'
    Assert-Throws { Assert-ClearInstanceSnapshot @() $true 0 } 'existing Jiffshot instance'
    Assert-Throws { Assert-ClearInstanceSnapshot @() $false 73 } 'existing Jiffshot instance'
}

Invoke-Case 'registry_reader_detects_empty_values_without_expanding_data' {
    $keyPath = 'Software\Microsoft\Windows\CurrentVersion\Run'
    $empty = New-TestRegistryKey @('JIFFSHOT') '' ([Microsoft.Win32.RegistryValueKind]::String)
    $entry = Read-StartupValueFromKey $empty $keyPath ([Microsoft.Win32.RegistryView]::Registry64)
    Assert-True $entry.Present 'An empty existing value was treated as absent.'
    Assert-Equal '' $entry.Value 'The empty registry value changed.'
    Assert-Equal 'String' $entry.Kind 'The registry kind was lost.'
    Assert-Equal 'Registry64' $entry.View 'The registry view was lost.'
    Assert-Equal 'DoNotExpandEnvironmentNames' $empty.ReadOptions.ToString() 'Registry data was expanded.'
    $expand = New-TestRegistryKey @('Jiffshot') '%APPDATA%\elsewhere' ([Microsoft.Win32.RegistryValueKind]::ExpandString)
    $entry = Read-StartupValueFromKey $expand $keyPath ([Microsoft.Win32.RegistryView]::Registry32)
    Assert-Equal '%APPDATA%\elsewhere' $entry.Value 'Expandable registry data must remain raw.'
    Assert-Equal 'ExpandString' $entry.Kind 'The expandable kind was lost.'
    $other = New-TestRegistryKey @('Other') 'ignored' ([Microsoft.Win32.RegistryValueKind]::String)
    $entry = Read-StartupValueFromKey $other $keyPath ([Microsoft.Win32.RegistryView]::Registry64)
    Assert-True (-not $entry.Present) 'An unrelated value was mistaken for Jiffshot.'
    Assert-Equal 0 $other.ReadCount 'An unrelated value should not be read.'
}

Invoke-Case 'command_accepts_quoted_unicode_and_space_path' {
    Assert-True (Test-ExpectedStartupCommand $owned $binary) 'The exact quoted command was rejected.'
    $differentPathCase = New-TestEntry -Present $true `
        -Value ('"c:\portable jiffshot\' + [char] 0x03b4 + '\JIFFSHOT.EXE" --background')
    Assert-True (Test-ExpectedStartupCommand $differentPathCase $binary) 'Windows paths should compare case-insensitively.'
}

Invoke-Case 'command_rejects_wrong_arguments_type_or_executable' {
    foreach ($value in @(
        $binary + ' --background',
        '"' + $binary + '"',
        '"' + $binary + '" --BACKGROUND',
        '"' + $binary + '" --background --window',
        '"C:\Other\jiffshot.exe" --background',
        '"' + $binary + '"  --background'
    )) {
        $entry = New-TestEntry -Present $true -Value $value
        Assert-True (-not (Test-ExpectedStartupCommand $entry $binary)) "Accepted unexpected command: $value"
    }
    $expanded = New-TestEntry -Present $true -Value $command -Kind 'ExpandString'
    Assert-True (-not (Test-ExpectedStartupCommand $expanded $binary)) 'REG_EXPAND_SZ was accepted.'
    Assert-True (-not (Test-ExpectedStartupCommand (New-TestEntry) $binary)) 'An absent entry was accepted.'
}

Invoke-Case 'default_registration_requires_a_present_expected_value' {
    Assert-Equal $false (Assert-ExpectedRunValues @((New-TestEntry)) $binary) 'An absent value should keep waiting.'
    Assert-Equal $true (Assert-ExpectedRunValues @($owned) $binary) 'The expected value was rejected.'
    $wrong = New-TestEntry -Present $true -Value '"C:\Other\jiffshot.exe" --background'
    Assert-Throws { Assert-ExpectedRunValues @($owned, $wrong) $binary } 'Unexpected Jiffshot Run value'
}

Invoke-Case 'cleanup_removes_only_the_observed_exact_value' {
    Assert-Equal 'remove' (Get-RunRemovalDecision $true $true $owned $owned) 'An owned unchanged value was not removable.'
    Assert-Equal 'absent' (Get-RunRemovalDecision $true $true $owned (New-TestEntry)) 'An absent value should not be deleted.'
    Assert-Equal 'preserve' (Get-RunRemovalDecision $false $true $owned $owned) 'An unclean baseline must be preserved.'
    Assert-Equal 'preserve' (Get-RunRemovalDecision $true $false $owned $owned) 'An unobserved value must be preserved.'
    Assert-Equal 'preserve' (Get-RunRemovalDecision $true $true $null $owned) 'A missing ownership record must be preserved.'
    Assert-Equal 'preserve' (Get-RunRemovalDecision $true $true (New-TestEntry -Value $command) $owned) `
        'An absent original value must be preserved.'
    foreach ($changed in @(
        (New-TestEntry -Present $true -Value ($command + ' --window')),
        (New-TestEntry -Present $true -Value $command -Kind 'ExpandString'),
        (New-TestEntry -Present $true -Value $command -View 'Registry32'),
        (New-TestEntry -Present $true -Value $command -Name 'Other'),
        (New-TestEntry -Present $true -Value $command -KeyPath 'Software\Other\Run'),
        (New-TestEntry -Present $true -Value ($command.ToUpperInvariant()))
    )) {
        Assert-Equal 'preserve' (Get-RunRemovalDecision $true $true $owned $changed) 'A changed value must be preserved.'
    }
}

Invoke-Case 'cleanup_callback_is_not_called_for_unowned_or_changed_values' {
    $script:deleteCalls = 0
    $delete = { $script:deleteCalls++ }
    Assert-Equal 'preserve' (Invoke-GuardedRunRemoval $false $true $owned $owned $delete) 'An unclean baseline was accepted.'
    $changed = New-TestEntry -Present $true -Value ($command + ' --window')
    Assert-Equal 'preserve' (Invoke-GuardedRunRemoval $true $true $owned $changed $delete) 'A changed value was accepted.'
    Assert-Equal 'absent' (Invoke-GuardedRunRemoval $true $true $owned (New-TestEntry) $delete) 'An absent value was not handled.'
    Assert-Equal 0 $script:deleteCalls 'The delete callback ran without ownership.'
    Assert-Equal 'remove' (Invoke-GuardedRunRemoval $true $true $owned $owned $delete) 'An unchanged owned value was not removed.'
    Assert-Equal 1 $script:deleteCalls 'The delete callback should run exactly once.'
}

Invoke-Case 'launches_refuse_changed_missing_or_new_run_values' {
    $otherViewAbsent = New-TestEntry -View 'Registry32'
    Assert-RecordedRunValues @($owned) @($owned, $otherViewAbsent)
    $changed = New-TestEntry -Present $true -Value ($command + ' --window')
    Assert-Throws { Assert-RecordedRunValues @($owned) @($changed, $otherViewAbsent) } 'changed or disappeared'
    Assert-Throws { Assert-RecordedRunValues @($owned) @((New-TestEntry), $otherViewAbsent) } 'changed or disappeared'
    $newOtherView = New-TestEntry -Present $true -Value $command -View 'Registry32'
    Assert-Throws { Assert-RecordedRunValues @($owned) @($owned, $newOtherView) } 'unowned'
    Assert-Throws { Assert-RecordedRunValues @() @($owned) } 'No test-owned Run value'
}

Invoke-Case 'commands_require_owned_processes_and_tray_window' {
    Assert-OwnedProcessSet @(42, 43) @(42, 43) 42 42
    Assert-Throws { Assert-OwnedProcessSet @(42, 99) @(42) 42 42 } 'unowned Jiffshot PID 99'
    Assert-Throws { Assert-OwnedProcessSet @(42) @(42) 42 99 } 'not owned'
    Assert-Throws { Assert-OwnedProcessSet @(42) @() 42 42 } 'not owned'
    Assert-Throws { Assert-OwnedProcessSet @() @(42) 42 42 } 'not owned'
    Assert-Throws { Assert-OwnedProcessSet @() @() 0 0 } 'not owned'
}

Invoke-Case 'background_rejects_widget_untitled_windows_and_foreground' {
    $hidden = [pscustomobject] @{ Title = ''; ClassName = 'hidden'; Visible = $false; Cloaked = $false }
    $cloaked = [pscustomobject] @{ Title = 'prepared'; ClassName = 'popup'; Visible = $true; Cloaked = $true }
    Assert-BackgroundSnapshot @($hidden, $cloaked) 7 42
    $widget = [pscustomobject] @{ Title = 'Jiffshot Capture Widget'; ClassName = 'widget'; Visible = $true; Cloaked = $false }
    $untitled = [pscustomobject] @{ Title = ''; ClassName = 'popup'; Visible = $true; Cloaked = $false }
    Assert-Throws { Assert-BackgroundSnapshot @($widget) 7 42 } 'Jiffshot Capture Widget'
    Assert-Throws { Assert-BackgroundSnapshot @($untitled) 7 42 } 'visible top-level windows'
    Assert-Throws { Assert-BackgroundSnapshot @($hidden) 42 42 } 'foreground'
    Assert-Throws { Assert-BackgroundSnapshot @($hidden) 7 42 $true } 'foreground event'
}

Invoke-Case 'background_rejects_nonforeground_toasts_and_transient_shows' {
    $toast = [pscustomobject] @{
        Title = 'Jiffshot: Jiffshot hotkeys'; ClassName = 'ToastWindow'; Visible = $true; Cloaked = $false
    }
    Assert-Throws { Assert-BackgroundSnapshot @($toast) 7 42 $false $false } 'Jiffshot: Jiffshot hotkeys'
    Assert-Throws { Assert-BackgroundSnapshot @() 7 42 $false $true } 'window or notification'
    $toast.Cloaked = $true
    Assert-BackgroundSnapshot @($toast) 7 42 $false $false
}

Invoke-Case 'window_events_track_uncloaked_top_level_shows_by_process' {
    Initialize-StartupNative
    $events = [JiffshotStartupE2E.WindowEvents]::new()
    $events.Record(0x8002, 42, $false, $false)
    $events.Record(0x8002, 42, $true, $true)
    $events.Record(0x8002, 0, $true, $false)
    Assert-True (-not $events.SawShown(42)) 'A child or cloaked window was recorded as visible.'
    $events.Record(3, 43, $false, $false)
    Assert-True $events.SawForeground(43) 'A foreground event was lost.'
    Assert-True (-not $events.SawShown(43)) 'A foreground event was mistaken for a show event.'
    $events.Record(0x8002, 42, $true, $false)
    Assert-True $events.SawShown(42) 'A non-foreground top-level show event was lost.'
    Assert-True (-not $events.SawShown(43)) 'A different process inherited the visible event.'
}

Invoke-Case 'duplicate_launch_must_exit_successfully' {
    Wait-OwnedExit ([pscustomobject] @{ IsRunning = $false; ExitCode = 0 }) 1 'duplicate'
    Assert-Throws {
        Wait-OwnedExit ([pscustomobject] @{ IsRunning = $false; ExitCode = 2 }) 1 'duplicate'
    } 'code 2'
}

Invoke-Case 'windows_disable_snapshot_compares_exact_binary_data' {
    $entry = [pscustomobject] @{
        Present = $true; Kind = 'Binary'; Value = [byte[]] @(3, 0, 0, 0)
        View = 'Registry64'; KeyPath = 'StartupApproved\Run'; Name = 'Jiffshot'
    }
    $changed = [pscustomobject] @{
        Present = $true; Kind = 'Binary'; Value = [byte[]] @(2, 0, 0, 0)
        View = 'Registry64'; KeyPath = 'StartupApproved\Run'; Name = 'Jiffshot'
    }
    $signature = @(Get-ApprovalSignature @($entry))
    Assert-Equal 'Registry64|StartupApproved\Run|Binary|AwAAAA==' $signature[0] 'Wrong approval signature.'
    Assert-True ($signature[0] -cne @(Get-ApprovalSignature @($changed))[0]) 'Changed approval bytes were not detected.'
    $entry.Kind = 'String'
    Assert-Throws { Get-ApprovalSignature @($entry) } 'Unexpected StartupApproved value type'
}

Invoke-Case 'scratch_widget_update_preserves_other_settings' {
    $settings = '{"theme":"Dark","startup_initialized":true,"capture":{"show_widget":false,"include_cursor":true}}' |
        ConvertFrom-Json
    $settings = Enable-ScratchWidget $settings
    Assert-True $settings.capture.show_widget 'The widget was not enabled.'
    Assert-Equal 'Dark' $settings.theme 'The theme was not preserved.'
    Assert-True $settings.startup_initialized 'The existing startup marker was not preserved.'
    Assert-True $settings.capture.include_cursor 'Another capture preference was not preserved.'
    $serialized = $settings | ConvertTo-Json -Depth 100 | ConvertFrom-Json
    Assert-True $serialized.capture.show_widget 'The serialized widget preference was not enabled.'
    Assert-True $serialized.startup_initialized 'The serialized startup marker was not preserved.'
    $fresh = Enable-ScratchWidget ([pscustomobject] @{})
    Assert-True $fresh.capture.show_widget 'The widget was not enabled in fresh settings.'
    Assert-Throws { Enable-ScratchWidget ([pscustomobject] @{ capture = 'not-an-object' }) } 'not an object'
}

Invoke-Case 'background_notice_fixture_preserves_other_scratch_settings' {
    $settings = '{"theme":"Dark","windows_startup":{"requested":true},"capture":{"show_widget":false,"include_cursor":true}}' |
        ConvertFrom-Json
    $fixture = Enable-ScratchBackgroundFixture $settings
    Assert-True $fixture.capture.show_widget 'The saved widget fixture was not enabled.'
    Assert-Equal 'jiffshot-e2e-unsupported-hotkey' $fixture.capture.capture_hotkey 'The notice trigger was not installed.'
    Assert-True $fixture.capture.include_cursor 'Another capture preference was changed.'
    Assert-True $fixture.windows_startup.requested 'The startup marker was changed.'
    Assert-Equal 'Dark' $fixture.theme 'Another top-level setting was changed.'
}

Invoke-Case 'notice_fixtures_restore_hotkey_and_startup_record' {
    $settings = '{"capture":{"show_widget":true,"capture_hotkey":"Ctrl+Shift+F9"},"windows_startup":{"backend":"desktop","requested":true}}' |
        ConvertFrom-Json
    $original = $settings.windows_startup
    $settings = Enable-ScratchBackgroundFixture $settings
    $settings = Restore-ScratchHotkey $settings $true 'Ctrl+Shift+F9'
    Assert-Equal 'Ctrl+Shift+F9' $settings.capture.capture_hotkey 'The original hotkey was not restored.'
    $settings = Restore-ScratchHotkey $settings $false $null
    Assert-True ($null -eq $settings.capture.PSObject.Properties['capture_hotkey']) 'An originally absent hotkey remained.'
    $settings = Restore-ScratchHotkey $settings $true $null
    Assert-True ($null -ne $settings.capture.PSObject.Properties['capture_hotkey']) 'An originally null hotkey was removed.'
    Assert-True ($null -eq $settings.capture.capture_hotkey) 'An originally null hotkey changed.'

    $invalid = [pscustomobject] @{ backend = 'jiffshot-e2e-invalid-backend'; requested = $true }
    $settings = Set-ScratchStartupRecord $settings $invalid
    Assert-Equal 'jiffshot-e2e-invalid-backend' $settings.windows_startup.backend 'The startup-warning fixture was not installed.'
    $settings = Set-ScratchStartupRecord $settings $original
    Assert-Equal 'desktop' $settings.windows_startup.backend 'The startup backend was not restored.'
    Assert-True $settings.windows_startup.requested 'The enabled startup setting was not restored.'
    Assert-True $settings.capture.show_widget 'The saved widget preference was changed.'
}

Invoke-Case 'native_declarations_compile_without_invoking_windows' {
    Initialize-StartupNative
    $native = 'JiffshotStartupE2E.Native' -as [type]
    $start = $native.GetMethod('Start')
    Assert-Equal 'JiffshotStartupE2E.OwnedProcess' $start.ReturnType.FullName 'The process handle wrapper is missing.'
    Assert-Equal 'command,directory,config,local,library' `
        (($start.GetParameters() | ForEach-Object { $_.Name }) -join ',') 'The process launch signature changed.'
    Assert-Equal 'JiffshotStartupE2E.WindowInfo[]' $native.GetMethod('Windows').ReturnType.FullName `
        'The window inventory signature changed.'
    Assert-Equal 'JiffshotStartupE2E.WindowObserver' $native.GetMethod('WatchWindows').ReturnType.FullName `
        'The startup window observer signature changed.'
}

Invoke-Case 'child_environment_removes_inherited_self_test_flags' {
    Initialize-StartupNative
    $inherited = @{
        'JIFFSHOT_UITEST' = 'do-not-inherit'
        'JIFFSHOT_UITEST_STARTUP' = '1'
        'JIFFSHOT_CONFIG_DIR' = 'real-settings'
        'RUSTOLONIA_TEST' = '1'
        'PATH' = 'system-path'
        'APPDATA' = 'real-appdata'
    }
    $child = [JiffshotStartupE2E.Native]::ChildEnvironment(
        $inherited, 'scratch-config', 'scratch-local', 'scratch-library')
    Assert-True (-not $child.ContainsKey('JIFFSHOT_UITEST')) 'The UI test flag would block real registration.'
    Assert-True (-not $child.ContainsKey('JIFFSHOT_UITEST_STARTUP')) 'An inherited startup self-test flag remained.'
    Assert-True (-not $child.ContainsKey('RUSTOLONIA_TEST')) 'An inherited UI-test flag remained.'
    Assert-Equal 'scratch-config' $child['JIFFSHOT_CONFIG_DIR'] 'The config directory was not isolated.'
    Assert-Equal 'scratch-local' $child['JIFFSHOT_LOCAL_DIR'] 'The local directory was not isolated.'
    Assert-Equal 'scratch-library' $child['JIFFSHOT_LIBRARY'] 'The library was not isolated.'
    Assert-Equal 'scratch-config' $child['APPDATA'] 'APPDATA was not isolated.'
    Assert-Equal 'scratch-local' $child['LOCALAPPDATA'] 'LOCALAPPDATA was not isolated.'
    Assert-Equal 'system-path' $child['PATH'] 'Unrelated environment variables were lost.'
}

if ($script:passedCases -ne 25) { throw "Expected 25 discovered cases, ran $script:passedCases." }
Write-Host "Isolated harness tests: $script:passedCases passed, 0 failed."
