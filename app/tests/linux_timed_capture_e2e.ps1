#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify delayed capture preferences and cancellation through a mock portal.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force

function Get-FileInode {
    param([Parameter(Mandatory)][string]$Path)
    $inode = & stat -c '%i' -- $Path
    if ($LASTEXITCODE -ne 0) {
        throw "stat failed for $Path"
    }
    [long]$inode
}

function Test-EmptyDirectory {
    param([Parameter(Mandatory)][string]$Path)
    @(Get-ChildItem -LiteralPath $Path -Force).Count -eq 0
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$portal = New-TimedPortal
$app = $server = $driver = $text = $null
$work = New-E2EWorkDirectory 'jiffshot-timed-ui-'
try {
    New-E2EPrivateDirectory (Join-Path $work 'runtime') | Out-Null
    $config = (New-Item -ItemType Directory -Force -Path (Join-Path $work 'config/Jiffshot')).FullName
    $captures = (New-Item -ItemType Directory -Path (Join-Path $work 'captures')).FullName
    $settings = Join-Path $config 'settings.json'
    $original = [ordered]@{
        theme                 = 'Light'
        capture_directory     = $captures
        copy_to_clipboard     = $false
        capture_delay_seconds = 3
        capture_cursor        = $false
        recording_cursor      = $true
    }
    Write-E2EJson $settings $original
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $displayName = $server.Display
        $environment = New-E2EEnvironment $displayName $work $runtime
        $app = Start-E2EProcess @($binaryPath) $environment $work $appLog
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'the editor' 12
        $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
        $text = New-E2ETextReader
        Wait-E2ESeconds 0.5

        $clickLabel = {
            param([string]$Label)
            $position = Find-E2EScrollingLabel $text $driver $Label
            $driver.Click($position[0], $position[1])
        }
        $replaceDelay = {
            param([string]$Value)
            $position = Find-E2EScrollingLabel $text $driver 'Save capture delay'
            # The 60-pixel delay textbox is immediately before the save button.
            Set-E2EText $driver ($position[0] - 110) $position[1] $Value
        }
        $savedWith = {
            param([System.Collections.IDictionary]$Expected)
            Test-E2EDeepEqual (Read-E2EJson $settings) $Expected
        }

        $previous = Get-FileInode $settings
        & $clickLabel 'Save cursor preferences'
        Wait-E2EUntil { (Get-FileInode $settings) -ne $previous } 'saving loaded cursors'
        Assert-E2E (& $savedWith $original) 'the cursor controls must load their separate saved values'
        & $clickLabel 'Include cursor in captures'
        & $clickLabel 'Include cursor in recordings'
        $previous = Get-FileInode $settings
        & $clickLabel 'Save cursor preferences'
        Wait-E2EUntil { (Get-FileInode $settings) -ne $previous } 'saving cursor changes'
        Assert-E2E (& $savedWith (Copy-E2EMap $original @{ capture_cursor = $true; recording_cursor = $false })) 'capture and recording cursors must change independently'
        & $clickLabel 'Include cursor in captures'
        & $clickLabel 'Include cursor in recordings'
        $previous = Get-FileInode $settings
        & $clickLabel 'Save cursor preferences'
        Wait-E2EUntil { (Get-FileInode $settings) -ne $previous } 'restoring cursors'
        Assert-E2E (& $savedWith $original) 'restored cursors must match the original preferences'

        $previous = Get-FileInode $settings
        & $clickLabel 'Save capture delay'
        Wait-E2EUntil { (Get-FileInode $settings) -ne $previous } 'saving the loaded delay'
        Assert-E2E (& $savedWith $original) 'saving must retain the loaded delay and other preferences'
        & $replaceDelay '5'
        $previous = Get-FileInode $settings
        & $clickLabel 'Save capture delay'
        Wait-E2EUntil { (Get-FileInode $settings) -ne $previous } 'saving a new delay'
        $saved = Copy-E2EMap $original @{ capture_delay_seconds = 5 }
        Assert-E2E (& $savedWith $saved) 'the new delay must be saved with the other preferences'

        $beforeStatus = $driver.Capture(0, 480, 800, 20)
        & $replaceDelay '61'
        & $clickLabel 'Save capture delay'
        Wait-E2EUntil { -not $driver.Capture(0, 480, 800, 20).SameAs($beforeStatus) } 'the invalid-delay error'
        Assert-E2E (& $savedWith $saved) 'an invalid delay must not overwrite saved preferences'

        Write-E2EJson $settings (Copy-E2EMap $saved @{ capture_delay_seconds = -1 })
        & $clickLabel 'Capture screen or window'
        Wait-E2ESeconds 0.5
        Assert-E2E ($portal.CaptureRequests -eq 0) 'an invalid delay must prevent the screen capture portal request'

        Write-E2EJson $settings $saved
        $portal.HoldStart = $true
        & $clickLabel 'Capture screen or window'
        Wait-E2EUntil { $portal.ScreenStarts.Count -eq 1 } 'the held source chooser'
        $heldSession, $heldRequest = $portal.ScreenStarts[-1]
        & $clickLabel 'Cancel delayed capture'
        Send-TimedPortalStart $portal $heldRequest
        Wait-E2EUntil { $portal.RemoteOpens -eq 1 } 'the selected source'
        Assert-E2E ($portal.ScreenSelectionOptions[-1]['cursor_mode'].Value -eq 1) 'the saved capture setting must hide the portal cursor'
        Wait-E2EUntil { $portal.Closes.Contains($heldSession) } 'closing a capture cancelled during source selection' 3
        Assert-E2E (Test-EmptyDirectory $captures) 'cancellation must not save a capture'
        $portal.HoldStart = $false
        foreach ($expected in 2, 3) {
            & $clickLabel 'Capture screen or window'
            Wait-E2EUntil { $portal.RemoteOpens -eq $expected } "the selected screen's PipeWire remote"
            $countdownSession = $portal.ScreenSessions[-1]
            & $clickLabel 'Capture screen or window'
            Wait-E2ESeconds 0.3
            Assert-E2E ($portal.CaptureRequests -eq $expected) 'another delayed capture must not start during the countdown'
            & $clickLabel 'Cancel delayed capture'
            Wait-E2EUntil { $portal.Closes.Contains($countdownSession) } 'closing the cancelled delayed capture' 3
            Assert-E2E (Test-EmptyDirectory $captures) 'cancellation must not save a capture'
        }
        Write-E2ELine 'GUI persisted and validated the capture delay and cancelled active countdowns'
    }
    catch {
        if ($null -ne $driver) {
            Write-E2ETrace $driver 'timer-failure'
        }
        Write-E2ELogTail @($xvfbLog, $appLog)
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
    Close-TimedPortal $portal
    Remove-E2EWorkDirectory $work
}
