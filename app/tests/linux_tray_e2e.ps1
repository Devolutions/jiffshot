#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify tray lifecycle, fallback, and background capture in private X11/D-Bus sessions.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.Desktop.psm1') -Force

function Get-SafeCurrentMinute {
    while ($true) {
        $now = [DateTime]::Now
        if ($now.Second -lt 35) {
            return $now
        }
        Wait-E2ESeconds 1
    }
}

function Get-Settings {
    Read-E2EJson $settingsPath
}

function Test-TrayEnabled {
    param([bool]$Expected)
    $value = (Get-Settings)['linux_tray_enabled']
    $value -is [bool] -and $value -eq $Expected
}

function Start-App {
    Start-E2EProcess @($binaryPath) $environment $work $appLog
}

function Connect-Editor {
    Connect-E2EWindow $environment 'the editor' 12
}

function Test-WindowShown {
    $null -ne (Get-E2EWindowInfo $environment)
}

function Invoke-ClickLabel {
    param([Parameter(Mandatory)][string]$Label)
    $position = Find-E2EScrollingLabel $reader $driver $Label
    $driver.Click($position[0], $position[1])
}

function Find-Notification {
    param([Parameter(Mandatory)][string]$Summary)
    foreach ($message in $notifications.Messages) {
        if ($message[3] -ceq $Summary) {
            return , $message
        }
    }
    $null
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$portal = New-MockPortal
$notifications = New-E2EMockNotifications
$watcher = $app = $server = $driver = $reader = $null
$work = New-E2EWorkDirectory 'jiffshot-tray-ui-'
try {
    New-E2EPrivateDirectory (Join-Path $work 'runtime') | Out-Null
    $config = New-Item -ItemType Directory -Force -Path (Join-Path $work 'config/Jiffshot')
    $settingsPath = Join-Path $config 'settings.json'
    $captures = (New-Item -ItemType Directory -Path (Join-Path $work 'captures')).FullName
    $original = [ordered]@{
        theme              = 'Light'
        capture_directory  = $captures
        linux_tray_enabled = $true
        other              = [ordered]@{ keep = $true }
    }
    Write-E2EJson $settingsPath $original
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $environment = New-E2EEnvironment $server.Display $work $runtime
        $reader = New-E2ETextReader

        $app = Start-App
        $driver = Connect-Editor
        Wait-E2ESeconds 1.5
        $driver.RequestClose()
        Wait-E2EUntil { $app.HasExited } 'normal close without a watcher'
        Assert-E2E (Test-E2EDeepEqual (Get-Settings) $original) 'closing without a watcher changed the settings'
        Close-E2EX11Driver $driver
        $driver = $null
        Write-E2ELine 'Missing watcher keeps the editor normally closable'

        $watcher = New-E2EMockWatcher -HostRegistered $false
        $app = Start-App
        $driver = Connect-Editor
        Wait-E2EUntil { $watcher.Items.Count -gt 0 -and $watcher.HostQueries -gt 0 } 'a hostless watcher'
        Wait-E2ESeconds 0.8
        $driver.RequestClose()
        Wait-E2EUntil { $app.HasExited } 'normal close without a tray host'
        Assert-E2E (Test-E2EDeepEqual (Get-Settings) $original) 'closing without a tray host changed the settings'
        Close-E2EX11Driver $driver
        $driver = $null
        Write-E2ELine 'A watcher without a tray host does not hide the editor'

        $disabled = Copy-E2EMap $original @{ linux_tray_enabled = $false }
        Write-E2EJson $settingsPath $disabled
        $previousItems = $watcher.Items.Count
        $previousQueries = $watcher.HostQueries
        $app = Start-App
        $driver = Connect-Editor
        Wait-E2ESeconds 0.5
        Invoke-ClickLabel 'Enable tray'
        Wait-E2EUntil { $watcher.Items.Count -gt $previousItems -and $watcher.HostQueries -gt $previousQueries } 'a rejected attempt to enable the tray'
        Wait-E2ESeconds 0.4
        Assert-E2E (Test-E2EDeepEqual (Get-Settings) $disabled) 'an unavailable tray must not be persisted as enabled'
        $driver.RequestClose()
        Wait-E2EUntil { $app.HasExited } 'close after a failed tray enable'
        Close-E2EX11Driver $driver
        $driver = $null

        $watcher.HostRegistered = $true
        $previousItems = $watcher.Items.Count
        $previousQueries = $watcher.HostQueries
        $app = Start-App
        $driver = Connect-Editor
        Wait-E2ESeconds 0.5
        Invoke-ClickLabel 'Enable tray'
        Wait-E2EUntil {
            $watcher.Items.Count -gt $previousItems -and $watcher.HostQueries -ge $previousQueries + 2 -and (Test-TrayEnabled $true)
        } 'manually enabling and persisting the tray'
        $trayItem = $watcher.Items[$watcher.Items.Count - 1]
        $driver.RequestClose()
        Wait-E2EUntil { -not (Test-WindowShown) } 'manual close to the tray'
        Invoke-E2ETrayActivate $watcher $trayItem
        Wait-E2EUntil { Test-WindowShown } 'manual tray restoration'
        Invoke-ClickLabel 'Disable tray'
        Wait-E2EUntil { Test-TrayEnabled $false } 'manually disabling and persisting the tray'
        Assert-E2E (Test-E2EDeepEqual (Get-Settings)['other'] ([ordered]@{ keep = $true })) 'toggling the tray changed other settings'
        $driver.RequestClose()
        Wait-E2EUntil { $app.HasExited } 'normal close after disabling the tray'
        Close-E2EX11Driver $driver
        $driver = $null
        Write-E2ELine 'GUI enable and disable persist without changing other settings'

        $now = Get-SafeCurrentMinute
        $scheduled = $now.AddMinutes(1).ToString('HH:mm', [System.Globalization.CultureInfo]::InvariantCulture)
        Write-E2EJson $settingsPath (Copy-E2EMap $original ([ordered]@{ schedule = $scheduled; schedule_default = $scheduled }))
        $previousItems = $watcher.Items.Count
        $previousQueries = $watcher.HostQueries
        $app = Start-App
        $driver = Connect-Editor
        Wait-E2EUntil { $watcher.Items.Count -gt $previousItems -and $watcher.HostQueries -ge $previousQueries + 2 } 'registered tray item with a host'
        Wait-E2ESeconds 0.6
        $trayItem = $watcher.Items[$watcher.Items.Count - 1]
        $pixmap = Invoke-E2EWatcherCall $watcher $trayItem '/StatusNotifierItem' 'org.freedesktop.DBus.Properties' 'Get' 'ss' @('org.kde.StatusNotifierItem', 'IconPixmap')
        $icon = $pixmap[0].Value[0]
        Assert-E2E ($icon[0] -eq 32 -and $icon[1] -eq 32) "unexpected tray icon size $($icon[0])x$($icon[1])"
        Assert-E2E ($icon[2].Length -eq 32 * 32 * 4) "unexpected tray icon data length $($icon[2].Length)"
        $trayMenu = Get-E2ETrayMenu $watcher $trayItem
        $menuLabels = @($trayMenu.Keys | Sort-Object -CaseSensitive)
        Assert-E2E (Test-E2EDeepEqual $menuLabels @('Capture screen or window', 'Open Jiffshot', 'Quit Jiffshot')) "unexpected tray menu: $($menuLabels -join ', ')"
        Invoke-ClickLabel 'Enable global capture shortcut'
        Wait-E2EUntil { $portal.Bindings.Count -gt 0 } 'the capture shortcut to bind'
        $shortcutSession = $portal.Bindings[$portal.Bindings.Count - 1][0]
        Wait-E2ESeconds 0.4
        $driver.RequestClose()
        Wait-E2EUntil { -not (Test-WindowShown) } 'the window to hide in the tray'
        Assert-E2E (-not $app.HasExited) 'closing to tray must not quit'

        $previous = $portal.CaptureRequests
        Send-PortalActivated $portal $shortcutSession 'capture'
        Wait-E2EUntil { $portal.CaptureRequests -gt $previous } 'a shortcut while hidden'
        Wait-E2EUntil { $null -ne (Find-Notification 'Capture failed') } 'the background capture failure notification'
        $notification = Find-Notification 'Capture failed'
        Assert-E2E ($notification[0] -ceq 'Jiffshot') "unexpected notification application $($notification[0])"
        Assert-E2E ($notification[4].Contains('Mock capture is unavailable')) "unexpected notification body $($notification[4])"
        Assert-E2E ($notification[6]['urgency'].Value -eq 2) "unexpected notification urgency $($notification[6]['urgency'].Value)"
        Assert-E2E (-not (Test-WindowShown)) 'a background shortcut should not restore the editor'
        Invoke-E2ETrayActivate $watcher $trayItem
        Wait-E2EUntil { Test-WindowShown } 'tray activation to restore the editor'
        $previous = $portal.CaptureRequests
        $previousNotifications = $notifications.Messages.Count
        Send-PortalActivated $portal $shortcutSession 'capture'
        Wait-E2EUntil { $portal.CaptureRequests -gt $previous } 'a shortcut while visible'
        Wait-E2ESeconds 0.7
        Assert-E2E ($notifications.Messages.Count -eq $previousNotifications) 'visible capture errors should remain in the editor rather than notifying'
        $driver.RequestClose()
        Wait-E2EUntil { -not (Test-WindowShown) } 'the restored editor to hide again'

        $notifications.Responsive = $false
        $previous = $portal.CaptureRequests
        $previousNotifications = $notifications.Messages.Count
        Send-PortalActivated $portal $shortcutSession 'capture'
        Wait-E2EUntil { $portal.CaptureRequests -gt $previous -and $notifications.Messages.Count -gt $previousNotifications } 'the unresponsive notification request'
        Wait-E2EUntil { [System.IO.File]::ReadAllText($appLog).Contains('the notification service did not respond') } 'the bounded notification timeout' 8
        Assert-E2E (-not $app.HasExited -and -not (Test-WindowShown)) 'an unresponsive notification service must not quit or restore the editor'
        $notifications.Responsive = $true

        $portal.ScreenMode = 'hold-start'
        $previousStarts = $portal.ScreenStarts.Count
        Wait-E2EUntil { $portal.ScreenStarts.Count -gt $previousStarts } 'the daily scheduled capture while hidden' 80
        $scheduledSession, $scheduledRequest = $portal.ScreenStarts[$portal.ScreenStarts.Count - 1]
        Assert-E2E (-not (Test-WindowShown)) 'daily capture must not open the editor'
        $today = [DateTime]::Now
        $lastFired = (Get-Settings)['linux_schedule_last_fired']
        $expectedDay = '{0:0000}-{1:000}' -f $today.Year, $today.DayOfYear
        Assert-E2E ($lastFired -ceq $expectedDay) "linux_schedule_last_fired is $lastFired, expected $expectedDay"
        Send-PortalResponse $portal $scheduledRequest 1 @{}
        Wait-E2EUntil { $portal.Closes -ccontains $scheduledSession } 'scheduled cancellation'
        Wait-E2EUntil { $null -ne (Find-Notification 'Daily capture failed') } 'the scheduled capture failure notification'
        Wait-E2ESeconds 0.3

        $watcher.HostRegistered = $false
        Wait-E2EUntil { Test-WindowShown } 'the editor after its tray host disappears' 8
        Assert-E2E (-not $app.HasExited) 'losing the tray host must not quit'
        Assert-E2E (Test-TrayEnabled $true) 'losing the tray host must not disable the tray preference'
        $driver.RequestClose()
        Wait-E2EUntil { $app.HasExited } 'normal close after host loss'
        Close-E2EX11Driver $driver
        $driver = $null
        Write-E2ELine 'Tray restore, shortcuts, scheduled capture, and host-loss fallback worked'

        $watcher.HostRegistered = $true
        $previousItems = $watcher.Items.Count
        $previousQueries = $watcher.HostQueries
        $app = Start-App
        $driver = Connect-Editor
        Wait-E2EUntil { $watcher.Items.Count -gt $previousItems -and $watcher.HostQueries -ge $previousQueries + 2 } 'tray registration before the host stops responding'
        Wait-E2ESeconds 0.5
        $driver.RequestClose()
        Wait-E2EUntil { -not (Test-WindowShown) } 'the editor hidden before a host timeout'
        $watcher.Unresponsive = $true
        Wait-E2EUntil { Test-WindowShown } 'the editor after a host timeout' 10
        Assert-E2E (-not $app.HasExited) 'an unresponsive tray host must not quit'
        $watcher.Unresponsive = $false
        $driver.RequestClose()
        Wait-E2EUntil { $app.HasExited } 'normal close after a host timeout'
        Close-E2EX11Driver $driver
        $driver = $null
        Write-E2ELine 'An unresponsive tray host restores the editor'

        $previousItems = $watcher.Items.Count
        $previousQueries = $watcher.HostQueries
        $app = Start-App
        $driver = Connect-Editor
        Wait-E2EUntil { $watcher.Items.Count -gt $previousItems -and $watcher.HostQueries -ge $previousQueries + 2 } 'tray registration after restart'
        Wait-E2ESeconds 0.5
        $trayItem = $watcher.Items[$watcher.Items.Count - 1]
        $trayMenu = Get-E2ETrayMenu $watcher $trayItem
        $driver.RequestClose()
        Wait-E2EUntil { -not (Test-WindowShown) } 'the editor hidden before tray actions'
        $portal.ScreenMode = 'hold-start'
        $previousStarts = $portal.ScreenStarts.Count
        Invoke-E2ETrayMenuItem $watcher $trayItem $trayMenu['Capture screen or window']
        Wait-E2EUntil { Test-WindowShown } 'the tray capture to restore the editor'
        Wait-E2EUntil { $portal.ScreenStarts.Count -gt $previousStarts } 'the tray capture chooser'
        $captureSession, $captureRequest = $portal.ScreenStarts[$portal.ScreenStarts.Count - 1]
        $driver.RequestClose()
        Wait-E2EUntil { -not (Test-WindowShown) } 'the editor hidden during capture'
        Invoke-E2ETrayMenuItem $watcher $trayItem $trayMenu['Quit Jiffshot']
        Wait-E2EUntil { Test-WindowShown } 'quit to restore an active capture'
        Assert-E2E (-not $app.HasExited) 'quit must not discard an active capture'
        Send-PortalResponse $portal $captureRequest 1 @{}
        Wait-E2EUntil { $portal.Closes -ccontains $captureSession } 'the cancelled tray capture'
        Wait-E2ESeconds 0.5
        $driver.RequestClose()
        Wait-E2EUntil { -not (Test-WindowShown) } 'the editor hidden before menu restore'
        Invoke-E2ETrayMenuItem $watcher $trayItem $trayMenu['Open Jiffshot']
        Wait-E2EUntil { Test-WindowShown } 'the tray menu to restore the editor'
        $driver.RequestClose()
        Wait-E2EUntil { -not (Test-WindowShown) } 'the editor hidden before quit'
        Invoke-E2ETrayMenuItem $watcher $trayItem $trayMenu['Quit Jiffshot']
        Wait-E2EUntil { $app.HasExited } 'the tray menu to quit'
        Close-E2EX11Driver $driver
        $driver = $null
        Write-E2ELine 'Tray menu capture, restore, and safe quit worked'

        $previousItems = $watcher.Items.Count
        $previousQueries = $watcher.HostQueries
        $app = Start-App
        $driver = Connect-Editor
        Wait-E2EUntil { $watcher.Items.Count -gt $previousItems -and $watcher.HostQueries -ge $previousQueries + 2 } 'tray registration before watcher loss'
        Wait-E2ESeconds 0.5
        $driver.RequestClose()
        Wait-E2EUntil { -not (Test-WindowShown) } 'the editor hidden before watcher loss'
        Unregister-E2EService $watcher
        Wait-E2EUntil { Test-WindowShown } 'the editor after its watcher disappears' 8
        $driver.RequestClose()
        Wait-E2EUntil { $app.HasExited } 'normal close after watcher loss'
        Close-E2EX11Driver $driver
        $driver = $null
        Write-E2ELine 'Watcher loss restores the hidden editor'
    }
    catch {
        if ($null -ne $driver -and (Test-WindowShown)) {
            Write-E2ETrace $driver 'tray-failure'
        }
        Write-E2ELogTail @($xvfbLog, $appLog)
        throw
    }
    finally {
        Close-E2EX11Driver $driver
        if ($null -ne $reader) {
            $reader.Dispose()
        }
        Stop-E2EProcess $app
        if ($null -ne $server) {
            Stop-E2EProcess $server.Process
        }
    }
}
finally {
    Unregister-E2EService $watcher
    Unregister-E2EService $notifications
    Unregister-E2EService $portal
    Remove-E2EWorkDirectory $work
}
