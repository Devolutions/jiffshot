#Requires -Version 7.4
# Desktop-integration helpers shared by the Linux tray, autostart, screenshot, share,
# and automatic-copy GUI drivers: StatusNotifier and notification mocks, file URIs,
# and attaching to an editor window that may be replaced while it is being found.
# Import LinuxE2E.psm1 (with -Force) before this module so both share one event loop.

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1')

$WatcherInterface = 'org.kde.StatusNotifierWatcher'
$ItemInterface = 'org.kde.StatusNotifierItem'
$MenuInterface = 'com.canonical.dbusmenu'
$NotificationsInterface = 'org.freedesktop.Notifications'

function ConvertTo-E2EFileUri {
    <# Returns the file:// URI for an absolute local path, like Path.as_uri(). #>
    param([Parameter(Mandatory)][string]$Path)
    [System.Uri]::new($Path).AbsoluteUri
}

function Invoke-E2EDrawArrow {
    <# Drags an arrow across the editor canvas from (250, 350) to (520, 430). #>
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver)
    $Driver.Move(250, 350)
    $Driver.Button(1, $true)
    $Driver.Move(520, 430)
    $Driver.Button(1, $false)
    $Driver.Sync()
}

function Read-E2EHandleBytes {
    <# Reads a file descriptor received over D-Bus to its end and closes it. #>
    param([Parameter(Mandatory)][Microsoft.Win32.SafeHandles.SafeFileHandle]$Handle)
    $stream = [System.IO.FileStream]::new($Handle, [System.IO.FileAccess]::Read, 1, $false)
    try {
        $buffer = [System.IO.MemoryStream]::new()
        $stream.CopyTo($buffer)
        , $buffer.ToArray()
    }
    finally {
        $stream.Dispose()
    }
}

function Connect-E2EWindow {
    <#
    Waits for the editor window and attaches an X11 driver to it.
    The app can destroy or replace its window between the xwininfo lookup and the focus
    request; only when the server reports BadWindow and that window is no longer the
    current editor window is the lookup repeated. Any other X error is a real failure.
    #>
    param(
        [Parameter(Mandatory)][System.Collections.IDictionary]$Environment,
        [string]$Description = 'the editor',
        [double]$Timeout = 12
    )
    $clock = [System.Diagnostics.Stopwatch]::StartNew()
    for ($attempt = 1; ; $attempt++) {
        $remaining = [Math]::Max(0.05, $Timeout - $clock.Elapsed.TotalSeconds)
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $Environment) } $Description $remaining
        $info = Get-E2EWindowInfo $Environment
        if ($null -eq $info) {
            continue
        }
        try {
            return New-E2EX11Driver $Environment['DISPLAY'] $info
        }
        catch [JiffshotE2E.X11Exception] {
            $failure = $_
            if (-not $failure.Exception.Message.Contains('BadWindow') -or $attempt -ge 5) {
                throw
            }
            $current = Get-E2EWindowInfo $Environment
            if ($null -ne $current -and $current.Window -eq $info.Window) {
                throw
            }
            Write-E2ELine "editor window 0x$('{0:x}' -f $info.Window) vanished while attaching; looking it up again"
        }
    }
}

#region StatusNotifierWatcher

function New-E2EMockWatcher {
    <# Connects a mock org.kde.StatusNotifierWatcher whose tray host may be absent. #>
    param([bool]$HostRegistered = $true)
    $watcher = [pscustomobject]@{
        Bus            = $null
        Handler        = ${function:Invoke-E2EMockWatcherMessage}
        HostRegistered = $HostRegistered
        Unresponsive   = $false
        Items          = [System.Collections.Generic.List[string]]::new()
        HostQueries    = 0
    }
    Register-E2EService $watcher $WatcherInterface
    $watcher
}

function Invoke-E2EMockWatcherMessage {
    param($Watcher, [JiffshotE2E.IncomingCall]$Call)
    $body = $Call.Body
    if ($Call.Interface -eq $WatcherInterface -and $Call.Member -eq 'RegisterStatusNotifierItem') {
        $Watcher.Items.Add($body[0])
        $Call.Reply('', @())
        return $true
    }
    if ($Call.Interface -eq 'org.freedesktop.DBus.Properties') {
        $values = [System.Collections.Generic.Dictionary[string, object]]::new([System.StringComparer]::Ordinal)
        $values['IsStatusNotifierHostRegistered'] = New-DBusVariant 'b' $Watcher.HostRegistered
        $values['RegisteredStatusNotifierItems'] = New-DBusVariant 'as' $Watcher.Items.ToArray()
        $values['ProtocolVersion'] = New-DBusVariant 'i' 0
        if ($Call.Member -eq 'Get' -and $body[0] -eq $WatcherInterface) {
            if ($body[1] -ceq 'IsStatusNotifierHostRegistered') {
                $Watcher.HostQueries++
            }
            if ($Watcher.Unresponsive) {
                return $true
            }
            if (-not $values.ContainsKey($body[1])) {
                $Call.Error('org.freedesktop.DBus.Error.UnknownProperty', "unknown property $($body[1])")
                return $true
            }
            $Call.Reply('v', @($values[$body[1]]))
            return $true
        }
        if ($Call.Member -eq 'GetAll' -and $body[0] -eq $WatcherInterface) {
            $Watcher.HostQueries++
            $Call.Reply('a{sv}', @($values))
            return $true
        }
    }
    $false
}

function Invoke-E2EWatcherCall {
    <# Calls a method on a registered tray item and returns its reply body. #>
    param(
        [Parameter(Mandatory)]$Watcher,
        [Parameter(Mandatory)][string]$Item,
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Interface,
        [Parameter(Mandatory)][string]$Member,
        [string]$Signature = '',
        [object[]]$Body = @()
    )
    , (Invoke-E2EDBusCall $Watcher $Item $Path $Interface $Member $Signature $Body)
}

function Invoke-E2ETrayActivate {
    param([Parameter(Mandatory)]$Watcher, [Parameter(Mandatory)][string]$Item)
    $null = Invoke-E2EWatcherCall $Watcher $Item '/StatusNotifierItem' $ItemInterface 'Activate' 'ii' @(0, 0)
}

function Get-E2ETrayMenu {
    <# Returns the tray menu's top-level labels mapped to their dbusmenu item ids. #>
    param([Parameter(Mandatory)]$Watcher, [Parameter(Mandatory)][string]$Item)
    $reply = Invoke-E2EWatcherCall $Watcher $Item '/MenuBar' $MenuInterface 'GetLayout' 'iias' @(0, -1, [string[]]@())
    $layout = $reply[1]
    $labels = [System.Collections.Generic.Dictionary[string, int]]::new([System.StringComparer]::Ordinal)
    foreach ($entry in $layout[2]) {
        $identifier, $properties, $null = $entry.Value
        if ($properties.ContainsKey('label')) {
            $labels[$properties['label'].Value] = $identifier
        }
    }
    , $labels
}

function Invoke-E2ETrayMenuItem {
    param([Parameter(Mandatory)]$Watcher, [Parameter(Mandatory)][string]$Item, [Parameter(Mandatory)][int]$Identifier)
    $null = Invoke-E2EWatcherCall $Watcher $Item '/MenuBar' $MenuInterface 'Event' 'isvu' @($Identifier, 'clicked', (New-DBusVariant 'i' 0), [uint32]0)
}

#endregion

#region Notifications

function New-E2EMockNotifications {
    <# Connects a mock org.freedesktop.Notifications that records Notify bodies. #>
    $notifications = [pscustomobject]@{
        Bus        = $null
        Handler    = ${function:Invoke-E2EMockNotificationsMessage}
        Messages   = [System.Collections.Generic.List[object]]::new()
        Responsive = $true
    }
    Register-E2EService $notifications $NotificationsInterface
    $notifications
}

function Invoke-E2EMockNotificationsMessage {
    param($Notifications, [JiffshotE2E.IncomingCall]$Call)
    if ($Call.Interface -eq $NotificationsInterface -and $Call.Member -eq 'Notify') {
        $Notifications.Messages.Add($Call.Body)
        if ($Notifications.Responsive) {
            $Call.Reply('u', @([uint32]$Notifications.Messages.Count))
        }
        return $true
    }
    $false
}

#endregion

Export-ModuleMember -Function *
