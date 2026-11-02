#Requires -Version 7.4
# Shared helpers for the Linux GUI end-to-end drivers (app/tests/linux_*_e2e.ps1).
#
# Mock D-Bus services run cooperatively on the PowerShell thread: incoming calls are
# queued by the native connection and dispatched only while a driver waits through
# Wait-E2ESeconds, Wait-E2EUntil, or Invoke-E2EDBusCall, like an asyncio event loop.

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'

$script:TmdsVersion = '0.95.2'
$script:TmdsSha256 = 'E9A0DA3A1CCBEAC214CC91A0CF28383EEFE12D1DC0CC79D5C8F50865D62A6135'
$script:Services = [System.Collections.Generic.List[object]]::new()
$script:Timers = [System.Collections.Generic.List[object]]::new()

$PortalDesktopPath = '/org/freedesktop/portal/desktop'
$PortalShortcutsInterface = 'org.freedesktop.portal.GlobalShortcuts'
$PortalSessionInterface = 'org.freedesktop.portal.Session'
$PortalRequestInterface = 'org.freedesktop.portal.Request'
$PortalScreenCastInterface = 'org.freedesktop.portal.ScreenCast'
$PortalScreenshotInterface = 'org.freedesktop.portal.Screenshot'

function Get-E2EDBusLibrary {
    <# Returns the pinned Tmds.DBus.Protocol assembly, downloading and verifying it once. #>
    if ($env:JIFFSHOT_E2E_TMDS_DBUS) {
        return (Resolve-Path -LiteralPath $env:JIFFSHOT_E2E_TMDS_DBUS).ProviderPath
    }
    $cacheRoot = if ($env:XDG_CACHE_HOME) { $env:XDG_CACHE_HOME } else { Join-Path $HOME '.cache' }
    $directory = Join-Path $cacheRoot "jiffshot-e2e/tmds.dbus.protocol.$script:TmdsVersion"
    $library = Join-Path $directory 'Tmds.DBus.Protocol.dll'
    if (Test-Path -LiteralPath $library) {
        return $library
    }
    $package = Join-Path $directory "tmds.dbus.protocol.$script:TmdsVersion.nupkg"
    New-Item -ItemType Directory -Force -Path $directory | Out-Null
    if (-not (Test-Path -LiteralPath $package)) {
        $download = "$package.download"
        Invoke-WebRequest -Uri "https://www.nuget.org/api/v2/package/Tmds.DBus.Protocol/$script:TmdsVersion" -OutFile $download
        Move-Item -LiteralPath $download -Destination $package -Force
    }
    $hash = (Get-FileHash -LiteralPath $package -Algorithm SHA256).Hash
    if ($hash -ne $script:TmdsSha256) {
        Remove-Item -LiteralPath $package -Force
        throw "Tmds.DBus.Protocol $script:TmdsVersion has SHA256 $hash, expected $script:TmdsSha256"
    }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($package)
    try {
        $entry = $archive.GetEntry('lib/net8.0/Tmds.DBus.Protocol.dll')
        if ($null -eq $entry) {
            throw "Tmds.DBus.Protocol $script:TmdsVersion has no net8.0 library"
        }
        $partial = "$library.partial"
        [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $partial, $true)
        Move-Item -LiteralPath $partial -Destination $library -Force
    }
    finally {
        $archive.Dispose()
    }
    $library
}

if (-not ('JiffshotE2E.Bus' -as [type])) {
    $dbusLibrary = Get-E2EDBusLibrary
    Add-Type -LiteralPath $dbusLibrary
    $references = @(
        $dbusLibrary
        [System.Management.Automation.PSObject].Assembly.Location
        'System.Runtime'
        'System.Collections'
        'System.Collections.Concurrent'
        'System.Collections.NonGeneric'
        'System.ComponentModel.Primitives'
        'System.Console'
        'System.Diagnostics.Process'
        'System.IO.Compression'
        'System.IO.Pipelines'
        'System.Linq'
        'System.Memory'
        'System.Runtime.InteropServices'
        'System.Security.Cryptography'
        'System.Threading'
        'System.Threading.Thread'
    )
    # The net8.0 Tmds build binds to the newer runtime; hide the resulting CS1701 warnings.
    Add-Type -LiteralPath (Join-Path $PSScriptRoot 'LinuxE2E.cs') -ReferencedAssemblies $references -IgnoreWarnings -WarningAction SilentlyContinue
}

#region Assertions and output

function Assert-E2E {
    param([Parameter(Mandatory)][AllowNull()]$Condition, [Parameter(Mandatory)][string]$Message)
    if (-not $Condition) {
        throw [System.Exception]::new("AssertionError: $Message")
    }
}

function Write-E2ELine {
    param([Parameter(Mandatory)][AllowEmptyString()][string]$Text)
    [Console]::Out.WriteLine($Text)
    [Console]::Out.Flush()
}

function Write-E2EPass {
    param([Parameter(Mandatory)][string]$Name)
    Write-E2ELine "PASS $Name"
}

function Write-E2ELogTail {
    <# Writes the last 12000 characters of each log to stderr after a failure. #>
    param([Parameter(Mandatory)][string[]]$Path)
    foreach ($item in $Path) {
        if (Test-Path -LiteralPath $item) {
            $text = [System.IO.File]::ReadAllText($item)
            if ($text.Length -gt 12000) {
                $text = $text.Substring($text.Length - 12000)
            }
            [Console]::Error.WriteLine($text)
        }
    }
    [Console]::Error.Flush()
}

function ConvertTo-E2EComparable {
    param([AllowNull()]$Value)
    if ($Value -is [System.Management.Automation.PSObject]) {
        $Value = $Value.psobject.BaseObject
    }
    if ($Value -is [System.Collections.IEnumerable] -and $Value -isnot [string] -and $Value -isnot [System.Collections.IDictionary]) {
        return , $Value
    }
    $Value
}

function Test-E2EDeepEqual {
    <# Compares JSON-like values the way Python == compares dicts, lists, and numbers. #>
    param([AllowNull()]$Left, [AllowNull()]$Right)
    $Left = ConvertTo-E2EComparable $Left
    $Right = ConvertTo-E2EComparable $Right
    if ($null -eq $Left -or $null -eq $Right) {
        return ($null -eq $Left -and $null -eq $Right)
    }
    if ($Left -is [System.Collections.IDictionary]) {
        if ($Right -isnot [System.Collections.IDictionary] -or $Left.Count -ne $Right.Count) {
            return $false
        }
        foreach ($key in $Left.Keys) {
            if (-not $Right.Contains($key) -or -not (Test-E2EDeepEqual $Left[$key] $Right[$key])) {
                return $false
            }
        }
        return $true
    }
    if ($Left -is [string] -or $Right -is [string]) {
        return ($Left -is [string] -and $Right -is [string] -and [string]::Equals($Left, $Right, [StringComparison]::Ordinal))
    }
    if ($Left -is [bool] -or $Right -is [bool]) {
        return ($Left -is [bool] -and $Right -is [bool] -and $Left -eq $Right)
    }
    if ($Left -is [System.Collections.IEnumerable]) {
        if ($Right -isnot [System.Collections.IEnumerable]) {
            return $false
        }
        $leftItems = @($Left)
        $rightItems = @($Right)
        if ($leftItems.Count -ne $rightItems.Count) {
            return $false
        }
        for ($index = 0; $index -lt $leftItems.Count; $index++) {
            if (-not (Test-E2EDeepEqual $leftItems[$index] $rightItems[$index])) {
                return $false
            }
        }
        return $true
    }
    $numeric = { param($value) $value -is [byte] -or $value -is [int16] -or $value -is [uint16] -or $value -is [int] -or $value -is [uint32] -or $value -is [long] -or $value -is [uint64] -or $value -is [double] -or $value -is [decimal] -or $value -is [single] }
    if ((& $numeric $Left) -and (& $numeric $Right)) {
        return ([decimal]$Left -eq [decimal]$Right)
    }
    $Left.Equals($Right)
}

function Read-E2EJson {
    <# Reads a JSON file as nested ordered hashtables and arrays. #>
    param([Parameter(Mandatory)][string]$Path)
    [System.IO.File]::ReadAllText($Path) | ConvertFrom-Json -AsHashtable -NoEnumerate
}

function Write-E2EJson {
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][AllowNull()]$Value)
    [System.IO.File]::WriteAllText($Path, (ConvertTo-Json -InputObject $Value -Depth 32 -Compress))
}

function Copy-E2EMap {
    <# Returns a shallow ordered copy of a dictionary with optional overrides, like {**a, **b}. #>
    param([Parameter(Mandatory)][System.Collections.IDictionary]$Map, [System.Collections.IDictionary]$Override = @{})
    $copy = [ordered]@{}
    foreach ($key in $Map.Keys) { $copy[$key] = $Map[$key] }
    foreach ($key in $Override.Keys) { $copy[$key] = $Override[$key] }
    $copy
}

#endregion

#region Cooperative event loop and D-Bus services

function New-DBusVariant {
    param([Parameter(Mandatory)][string]$Signature, [AllowNull()]$Value)
    [JiffshotE2E.Variant]::new($Signature, $Value)
}

function Register-E2EService {
    <# Connects $Owner to the session bus; its Handler scriptblock receives ($Owner, $Call). #>
    param([Parameter(Mandatory)]$Owner, [string]$Name)
    $Owner.Bus = [JiffshotE2E.Bus]::Connect()
    if ($Name) {
        $Owner.Bus.RequestName($Name)
    }
    $script:Services.Add($Owner)
}

function Unregister-E2EService {
    param([AllowNull()]$Owner)
    if ($null -ne $Owner -and $null -ne $Owner.Bus) {
        $null = $script:Services.Remove($Owner)
        $Owner.Bus.Dispose()
        $Owner.Bus = $null
    }
}

function Invoke-E2EDispatch {
    param($Owner, [JiffshotE2E.IncomingCall]$Call)
    $result = & $Owner.Handler $Owner $Call
    if ($Call.Finished) {
        return
    }
    foreach ($item in @($result)) {
        if ($item -is [bool] -and $item) {
            return
        }
    }
    $Call.UnknownMethod()
}

function Invoke-E2EPump {
    <# Dispatches queued D-Bus calls and due timers once. #>
    foreach ($owner in @($script:Services)) {
        if ($null -eq $owner.Bus) {
            continue
        }
        while ($null -ne ($call = $owner.Bus.Next())) {
            Invoke-E2EDispatch $owner $call
            if ($null -eq $owner.Bus) {
                break
            }
        }
    }
    if ($script:Timers.Count -gt 0) {
        $now = [System.Diagnostics.Stopwatch]::GetTimestamp()
        $due = @($script:Timers | Where-Object { $_.Due -le $now } | Sort-Object Due)
        foreach ($timer in $due) {
            $null = $script:Timers.Remove($timer)
            $arguments = $timer.Arguments
            & $timer.Action @arguments
        }
    }
}

function Add-E2ETimer {
    <# Runs Action with ArgumentList after Delay seconds of pumping, like loop.call_later. #>
    param(
        [Parameter(Mandatory)][double]$Delay,
        [Parameter(Mandatory)][scriptblock]$Action,
        [object[]]$ArgumentList = @()
    )
    $script:Timers.Add([pscustomobject]@{
            Due       = [System.Diagnostics.Stopwatch]::GetTimestamp() + [long]($Delay * [System.Diagnostics.Stopwatch]::Frequency)
            Action    = $Action
            Arguments = $ArgumentList
        })
}

function Wait-E2ESeconds {
    <# Sleeps while servicing mock D-Bus calls and timers, like asyncio.sleep. #>
    param([Parameter(Mandatory)][double]$Seconds)
    $deadline = [System.Diagnostics.Stopwatch]::StartNew()
    do {
        Invoke-E2EPump
        $remaining = $Seconds * 1000 - $deadline.Elapsed.TotalMilliseconds
        if ($remaining -gt 0) {
            [System.Threading.Thread]::Sleep([int][Math]::Min(10, [Math]::Ceiling($remaining)))
        }
    } while ($deadline.Elapsed.TotalSeconds -lt $Seconds)
    Invoke-E2EPump
}

function Wait-E2EUntil {
    <# Polls Predicate every 50 ms until it is truthy or Timeout seconds pass. #>
    param(
        [Parameter(Mandatory)][scriptblock]$Predicate,
        [Parameter(Mandatory)][string]$Description,
        [double]$Timeout = 20
    )
    $clock = [System.Diagnostics.Stopwatch]::StartNew()
    while ($clock.Elapsed.TotalSeconds -lt $Timeout) {
        Invoke-E2EPump
        if (& $Predicate) {
            return
        }
        Wait-E2ESeconds 0.05
    }
    throw [System.TimeoutException]::new("AssertionError: timed out waiting for $Description")
}

function Wait-E2ETask {
    param([Parameter(Mandatory)][System.Threading.Tasks.Task]$Task, [double]$Timeout = 25)
    $clock = [System.Diagnostics.Stopwatch]::StartNew()
    while (-not $Task.IsCompleted) {
        if ($clock.Elapsed.TotalSeconds -ge $Timeout) {
            throw [System.TimeoutException]::new('timed out waiting for a D-Bus reply')
        }
        Invoke-E2EPump
        [System.Threading.Thread]::Sleep(5)
    }
    $Task.GetAwaiter().GetResult()
}

function Invoke-E2EDBusCall {
    <# Calls a D-Bus method while still servicing mock calls; returns the reply body. #>
    param(
        [Parameter(Mandatory)]$Owner,
        [Parameter(Mandatory)][string]$Destination,
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Interface,
        [Parameter(Mandatory)][string]$Member,
        [string]$Signature = '',
        [object[]]$Body = @()
    )
    $task = $Owner.Bus.CallAsync($Destination, $Path, $Interface, $Member, $Signature, $Body)
    try {
        , (Wait-E2ETask $task)
    }
    catch [System.TimeoutException] {
        throw
    }
    catch {
        throw [System.Exception]::new("AssertionError: $Interface.$Member failed: $($_.Exception.Message)")
    }
}

function Send-DBusSignal {
    param(
        [Parameter(Mandatory)]$Owner,
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Interface,
        [Parameter(Mandatory)][string]$Member,
        [string]$Signature = '',
        [object[]]$Body = @()
    )
    if (-not $Owner.Bus.Signal($Path, $Interface, $Member, $Signature, $Body)) {
        throw "could not send $Interface.$Member"
    }
}

#endregion

#region Mock XDG desktop portal

function Get-PortalSender {
    param([Parameter(Mandatory)][JiffshotE2E.IncomingCall]$Call)
    $Call.Sender.TrimStart(':').Replace('.', '_')
}

function Get-PortalRequestPath {
    param([Parameter(Mandatory)][JiffshotE2E.IncomingCall]$Call, [Parameter(Mandatory)]$Options)
    "$PortalDesktopPath/request/$(Get-PortalSender $Call)/$($Options['handle_token'].Value)"
}

function Get-PortalSessionPath {
    param([Parameter(Mandatory)][JiffshotE2E.IncomingCall]$Call, [Parameter(Mandatory)]$Options)
    "$PortalDesktopPath/session/$(Get-PortalSender $Call)/$($Options['session_handle_token'].Value)"
}

function Send-PortalResponse {
    param([Parameter(Mandatory)]$Portal, [Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][uint32]$Code, [Parameter(Mandatory)][System.Collections.IDictionary]$Result)
    Send-DBusSignal $Portal $Path $PortalRequestInterface 'Response' 'ua{sv}' @($Code, $Result)
}

function Send-PortalResponseLater {
    <# Emits Request.Response after Delay seconds so the caller can subscribe first. #>
    param([Parameter(Mandatory)]$Portal, [Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][uint32]$Code, [Parameter(Mandatory)][System.Collections.IDictionary]$Result, [double]$Delay = 0.1)
    Add-E2ETimer -Delay $Delay -Action { param($p, $r, $c, $res) Send-PortalResponse $p $r $c $res } -ArgumentList @($Portal, $Path, $Code, $Result)
}

function New-MockPortal {
    <# Connects a mock org.freedesktop.portal.Desktop; replace .Handler to extend it and call Invoke-MockPortalMessage as the base. #>
    $portal = [pscustomobject]@{
        Bus                    = $null
        Handler                = ${function:Invoke-MockPortalMessage}
        Sessions               = [System.Collections.Generic.List[object]]::new()
        Bindings               = [System.Collections.Generic.List[object]]::new()
        Closes                 = [System.Collections.Generic.List[object]]::new()
        CaptureRequests        = 0
        Mode                   = 'success'
        ScreenMode             = 'reject'
        ScreenSessions         = [System.Collections.Generic.List[object]]::new()
        ScreenCreates          = [System.Collections.Generic.List[object]]::new()
        ScreenSelects          = [System.Collections.Generic.List[object]]::new()
        ScreenSelectionOptions = [System.Collections.Generic.List[object]]::new()
        ScreenStarts           = [System.Collections.Generic.List[object]]::new()
        ClosedRequests         = [System.Collections.Generic.List[object]]::new()
        ScreenshotMode         = 'success'
        ScreenshotUri          = $null
        ScreenshotRequests     = [System.Collections.Generic.List[object]]::new()
    }
    Register-E2EService $portal 'org.freedesktop.portal.Desktop'
    $portal
}

function Invoke-MockPortalMessage {
    param($Portal, [JiffshotE2E.IncomingCall]$Call)
    $body = $Call.Body
    if ($Call.Interface -eq 'org.freedesktop.DBus.Properties') {
        if ($Call.Member -eq 'Get') {
            $version = if ($body[0] -eq $PortalScreenshotInterface) { 2 } else { 1 }
            $Call.Reply('v', @((New-DBusVariant 'u' $version)))
            return $true
        }
        if ($Call.Member -eq 'GetAll') {
            $Call.Reply('a{sv}', @(@{ version = (New-DBusVariant 'u' 1) }))
            return $true
        }
    }
    if ($Call.Interface -eq $PortalShortcutsInterface -and $Call.Member -eq 'CreateSession') {
        $options = $body[0]
        $session = Get-PortalSessionPath $Call $options
        $request = Get-PortalRequestPath $Call $options
        $Portal.Sessions.Add($session)
        Send-PortalResponseLater $Portal $request 0 @{ session_handle = (New-DBusVariant 'o' $session) }
        $Call.Reply('o', @($request))
        return $true
    }
    if ($Call.Interface -eq $PortalShortcutsInterface -and $Call.Member -eq 'BindShortcuts') {
        $session, $shortcuts, $null, $options = $body
        $request = Get-PortalRequestPath $Call $options
        $Portal.Bindings.Add(@($session, $shortcuts))
        if ($Portal.Mode -ne 'hold') {
            $names = @{
                'capture'          = 'Ctrl+Alt+P'
                'toggle-recording' = 'Shift+F8'
                'pause-recording'  = 'Shift+F9'
                'stop-recording'   = 'Shift+F10'
            }
            $bound = [System.Collections.Generic.List[object]]::new()
            foreach ($shortcut in $shortcuts) {
                $shortcutId, $properties = $shortcut
                $bound.Add(@($shortcutId, [ordered]@{
                            description         = $properties['description']
                            trigger_description = (New-DBusVariant 's' $names[$shortcutId])
                        }))
            }
            if ($Portal.Mode -eq 'partial') {
                $bound.RemoveAt($bound.Count - 1)
            }
            $result = @{ shortcuts = (New-DBusVariant 'a(sa{sv})' $bound.ToArray()) }
            $code = if ($Portal.Mode -eq 'reject') { 1 } else { 0 }
            Send-PortalResponseLater $Portal $request $code $result
        }
        $Call.Reply('o', @($request))
        return $true
    }
    if ($Call.Interface -eq $PortalSessionInterface -and $Call.Member -eq 'Close') {
        $Portal.Closes.Add($Call.Path)
        $Call.Reply('', @())
        return $true
    }
    if ($Call.Interface -eq $PortalRequestInterface -and $Call.Member -eq 'Close') {
        $Portal.ClosedRequests.Add($Call.Path)
        $Call.Reply('', @())
        return $true
    }
    if ($Call.Interface -eq $PortalScreenshotInterface -and $Call.Member -eq 'Screenshot') {
        $options = $body[1]
        $request = Get-PortalRequestPath $Call $options
        $Portal.ScreenshotRequests.Add(@($request, $options))
        if ($Portal.ScreenshotMode -ne 'hold') {
            $code = if ($Portal.ScreenshotMode -eq 'cancel') { 1 } else { 0 }
            $result = if ($code) { @{} } else { @{ uri = (New-DBusVariant 's' $Portal.ScreenshotUri) } }
            Send-PortalResponseLater $Portal $request $code $result
        }
        $Call.Reply('o', @($request))
        return $true
    }
    if ($Call.Interface -eq $PortalScreenCastInterface -and $Call.Member -eq 'CreateSession') {
        $Portal.CaptureRequests++
        if ($Portal.ScreenMode -eq 'reject') {
            $Call.Error('org.freedesktop.DBus.Error.NotSupported', 'Mock capture is unavailable')
            return $true
        }
        $options = $body[0]
        $session = Get-PortalSessionPath $Call $options
        $request = Get-PortalRequestPath $Call $options
        $Portal.ScreenSessions.Add($session)
        $Portal.ScreenCreates.Add(@($session, $request))
        if ($Portal.ScreenMode -ne 'hold-create') {
            Send-PortalResponseLater $Portal $request 0 @{ session_handle = (New-DBusVariant 's' $session) }
        }
        $Call.Reply('o', @($request))
        return $true
    }
    if ($Call.Interface -eq $PortalScreenCastInterface -and $Call.Member -eq 'SelectSources') {
        $session, $options = $body
        $request = Get-PortalRequestPath $Call $options
        $Portal.ScreenSelects.Add(@($session, $request))
        $Portal.ScreenSelectionOptions.Add($options)
        if ($Portal.ScreenMode -ne 'hold-select') {
            Send-PortalResponseLater $Portal $request 0 @{}
        }
        $Call.Reply('o', @($request))
        return $true
    }
    if ($Call.Interface -eq $PortalScreenCastInterface -and $Call.Member -eq 'Start') {
        $session, $null, $options = $body
        $request = Get-PortalRequestPath $Call $options
        $Portal.ScreenStarts.Add(@($session, $request))
        $Call.Reply('o', @($request))
        return $true
    }
    $false
}

function Send-PortalActivated {
    param([Parameter(Mandatory)]$Portal, [Parameter(Mandatory)][string]$Session, [Parameter(Mandatory)][string]$Shortcut)
    Send-DBusSignal $Portal $PortalDesktopPath $PortalShortcutsInterface 'Activated' 'osta{sv}' @($Session, $Shortcut, [uint64]0, @{})
}

function Send-PortalSessionClosed {
    param([Parameter(Mandatory)]$Portal, [Parameter(Mandatory)][string]$Session)
    Send-DBusSignal $Portal $Session $script:PortalSessionInterface 'Closed' 'a{sv}' @(@{})
}

function New-TimedPortal {
    <# A mock portal whose ScreenCast.Start succeeds after 0.1 s (unless HoldStart) and whose OpenPipeWireRemote returns a pipe. #>
    $portal = New-MockPortal
    $portal.ScreenMode = 'fake-remote'
    $portal | Add-Member -NotePropertyMembers @{
        RemoteOpens = 0
        RemoteFds   = [System.Collections.Generic.List[int]]::new()
        HoldStart   = $false
    }
    $portal.Handler = {
        param($Portal, [JiffshotE2E.IncomingCall]$Call)
        if ($Call.Interface -ceq $PortalScreenCastInterface) {
            if ($Call.Member -ceq 'Start') {
                $handled = Invoke-MockPortalMessage $Portal $Call
                $startRequest = $Portal.ScreenStarts[-1][1]
                if (-not $Portal.HoldStart) {
                    Add-E2ETimer -Delay 0.1 -Action { param($p, $r) Send-TimedPortalStart $p $r } -ArgumentList @($Portal, $startRequest)
                }
                return $handled
            }
            if ($Call.Member -ceq 'OpenPipeWireRemote') {
                $Portal.RemoteOpens++
                $fds = [JiffshotE2E.Posix]::Pipe()
                $Portal.RemoteFds.AddRange([int[]]$fds)
                $Call.Reply('h', @($fds[0]))
                return $true
            }
        }
        Invoke-MockPortalMessage $Portal $Call
    }
    $portal
}

function Send-TimedPortalStart {
    <# Completes a held ScreenCast.Start with one stream, like TimedPortal.release_start. #>
    param([Parameter(Mandatory)]$Portal, [Parameter(Mandatory)][string]$RequestPath)
    $streams = New-DBusVariant 'a(ua{sv})' @(, @([uint32]42, @{}))
    Send-PortalResponse $Portal $RequestPath 0 @{ streams = $streams }
}

function Close-TimedPortal {
    param([AllowNull()]$Portal)
    if ($null -eq $Portal) {
        return
    }
    Unregister-E2EService $Portal
    foreach ($fd in $Portal.RemoteFds) {
        [JiffshotE2E.Posix]::Close($fd)
    }
    $Portal.RemoteFds.Clear()
}

#endregion

#region Private X server, app processes, and windows

function Resolve-E2EPath {
    <# Resolves an existing path strictly, like Path.resolve(strict=True). #>
    param([Parameter(Mandatory)][string]$Path)
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.LinkTarget) {
        $item = $item.ResolveLinkTarget($true)
    }
    $item.FullName
}

function New-E2EWorkDirectory {
    param([Parameter(Mandatory)][string]$Prefix)
    $path = Join-Path ([System.IO.Path]::GetTempPath()) ($Prefix + [System.IO.Path]::GetRandomFileName().Replace('.', ''))
    New-Item -ItemType Directory -Path $path | Out-Null
    [JiffshotE2E.Posix]::Chmod($path, [Convert]::ToInt32('700', 8))
    $path
}

function Remove-E2EWorkDirectory {
    param([AllowNull()][string]$Path)
    if ($Path -and (Test-Path -LiteralPath $Path)) {
        Remove-Item -LiteralPath $Path -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function New-E2EPrivateDirectory {
    param([Parameter(Mandatory)][string]$Path)
    New-Item -ItemType Directory -Path $Path | Out-Null
    [JiffshotE2E.Posix]::Chmod($Path, [Convert]::ToInt32('700', 8))
    $Path
}

function Start-E2EXvfb {
    <# Starts a private Xvfb; returns @{ Process; Display }. #>
    param([Parameter(Mandatory)][string]$LogPath, [string]$Screen = '1280x900x24')
    # Window probes are short-lived; their disconnects must not reset the test display.
    $process = [JiffshotE2E.ChildProcess]::StartCapturing(
        @('Xvfb', '-displayfd', '1', '-screen', '0', $Screen, '-nolisten', 'tcp', '-noreset'), $null, $null, $LogPath)
    $clock = [System.Diagnostics.Stopwatch]::StartNew()
    while ($process.Lines().Count -eq 0) {
        if ($clock.Elapsed.TotalSeconds -ge 10 -or $process.OutputClosed) {
            $process.Stop()
            throw 'private Xvfb did not start'
        }
        [System.Threading.Thread]::Sleep(10)
    }
    [pscustomobject]@{ Process = $process; Display = ':' + $process.Lines()[0].Trim() }
}

function New-E2EEnvironment {
    <# Returns the private desktop environment for the app under test. #>
    param(
        [Parameter(Mandatory)][string]$Display,
        [Parameter(Mandatory)][string]$Work,
        [Parameter(Mandatory)][string]$Runtime,
        [string]$RuntimeDirectory = (Join-Path $Work 'runtime')
    )
    $environment = [ordered]@{}
    foreach ($entry in [System.Environment]::GetEnvironmentVariables().GetEnumerator()) {
        $environment[$entry.Key] = $entry.Value
    }
    $environment['DISPLAY'] = $Display
    $environment['HOME'] = $Work
    $environment['XDG_CONFIG_HOME'] = Join-Path $Work 'config'
    $environment['XDG_DATA_HOME'] = Join-Path $Work 'data'
    $environment['XDG_CACHE_HOME'] = Join-Path $Work 'cache'
    $environment['XDG_RUNTIME_DIR'] = $RuntimeDirectory
    $environment['XDG_SESSION_TYPE'] = 'x11'
    $environment['RUSTOLONIA_HOST_LIB'] = Join-Path $Runtime 'librustolonia_host.so'
    $environment['LD_LIBRARY_PATH'] = (@($Runtime, $env:LD_LIBRARY_PATH) | Where-Object { $_ }) -join [System.IO.Path]::PathSeparator
    $environment.Remove('WAYLAND_DISPLAY')
    $environment
}

function Start-E2EProcess {
    <# Starts a child with stdin from /dev/null and output appended to LogPath. #>
    param(
        [Parameter(Mandatory)][string[]]$ArgumentList,
        [System.Collections.IDictionary]$Environment,
        [string]$WorkingDirectory,
        [string]$LogPath
    )
    [JiffshotE2E.ChildProcess]::Start($ArgumentList, $Environment, $WorkingDirectory, $LogPath)
}

function Stop-E2EProcess {
    <# SIGTERM, wait five seconds, then kill. #>
    param([AllowNull()]$Process)
    if ($null -ne $Process) {
        $Process.Stop()
    }
}

function Get-E2EWindowInfo {
    <# Returns @{ Window; X; Y } for the first viewable top-level window titled Title, or $null. #>
    param([Parameter(Mandatory)][System.Collections.IDictionary]$Environment, [string]$Title = 'Jiffshot')
    $display = $Environment['DISPLAY']
    $tree = & xwininfo -display $display -root -tree 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "xwininfo -root -tree failed: $tree"
    }
    foreach ($line in $tree) {
        $line = [string]$line
        if (-not $line.Contains("`"$Title`"")) {
            continue
        }
        $window = [Convert]::ToInt64(($line.Trim() -split '\s+')[0].Substring(2), 16)
        $info = (& xwininfo -display $display -id ('0x{0:x}' -f $window) 2>&1) -join "`n"
        if ($LASTEXITCODE -ne 0) {
            throw "xwininfo -id failed: $info"
        }
        $x = [regex]::Match($info, 'Absolute upper-left X:\s*(-?\d+)')
        $y = [regex]::Match($info, 'Absolute upper-left Y:\s*(-?\d+)')
        if ($info.Contains('Map State: IsViewable') -and $x.Success -and $y.Success) {
            return [pscustomobject]@{ Window = $window; X = [int]$x.Groups[1].Value; Y = [int]$y.Groups[1].Value }
        }
    }
    $null
}

function New-E2EX11Driver {
    param([Parameter(Mandatory)][string]$Display, [Parameter(Mandatory)]$WindowInfo)
    [JiffshotE2E.X11Driver]::new($Display, $WindowInfo.Window, $WindowInfo.X, $WindowInfo.Y)
}

function Close-E2EX11Driver {
    param([AllowNull()]$Driver)
    if ($null -ne $Driver) {
        $Driver.Dispose()
    }
}

#endregion

#region X11 input, pixels, and OCR

function Invoke-E2EScroll {
    <# Wheels the editor sidebar at (750, 150); count defaults to 25 notches. #>
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [Parameter(Mandatory)][ValidateSet('up', 'down')][string]$Direction, [int]$Count = 25)
    $button = if ($Direction -eq 'down') { 5 } else { 4 }
    $Driver.Wheel(750, 150, $button, $Count)
}

function Set-E2EText {
    <# Clicks a field, selects its text, and types replacement text. #>
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [int]$X, [int]$Y, [Parameter(Mandatory)][AllowEmptyString()][string]$Text)
    $Driver.Click($X, $Y)
    $Driver.SelectAll()
    foreach ($character in $Text.ToCharArray()) {
        $key = if ($character -eq '/') { 'slash' } else { [string]$character }
        $Driver.Press($key)
    }
}

function Set-E2EShortcutText {
    <# Replaces the shortcut field at (255, 80) with text such as Ctrl+Alt+P. #>
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [Parameter(Mandatory)][string]$Text)
    $Driver.Click(255, 80)
    $Driver.Key('Control_L', $true)
    $Driver.Key('a', $true)
    $Driver.Key('a', $false)
    $Driver.Key('Control_L', $false)
    $Driver.Key('Delete', $true)
    $Driver.Key('Delete', $false)
    foreach ($character in $Text.ToCharArray()) {
        $shifted = [char]::IsUpper($character) -or $character -eq '+'
        $name = if ($character -eq '+') { 'equal' } else { ([string]$character).ToLowerInvariant() }
        if ($shifted) { $Driver.Key('Shift_L', $true) }
        $Driver.Key($name, $true)
        $Driver.Key($name, $false)
        if ($shifted) { $Driver.Key('Shift_L', $false) }
    }
    $Driver.Sync()
}

function Write-E2ETrace {
    <# Saves an 800x500 window screenshot to $env:JIFFSHOT_GUI_TRACE/<Prefix>-<Label>.png when tracing. #>
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [Parameter(Mandatory)][string]$Label, [string]$Prefix = 'webcam')
    if (-not $env:JIFFSHOT_GUI_TRACE) {
        return
    }
    $png = $Driver.Capture(0, 0, 800, 500).ToPng()
    [System.IO.File]::WriteAllBytes((Join-Path $env:JIFFSHOT_GUI_TRACE "$Prefix-$Label.png"), $png)
}

function New-E2ETextReader {
    [JiffshotE2E.TextReader]::new()
}

function Get-E2EStatusText {
    param([Parameter(Mandatory)][JiffshotE2E.TextReader]$Reader, [Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver)
    $Reader.Recognize($Driver.Capture(0, 480, 800, 20), 7, $false).Trim()
}

function Wait-E2EStatus {
    param(
        [Parameter(Mandatory)][JiffshotE2E.TextReader]$Reader,
        [Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver,
        [Parameter(Mandatory)][string]$Expected,
        [double]$Timeout = 12
    )
    $seen = [ref]''
    try {
        Wait-E2EUntil {
            $seen.Value = Get-E2EStatusText $Reader $Driver
            $seen.Value.ToLowerInvariant().Contains($Expected.ToLowerInvariant())
        } "visible status containing '$Expected'" $Timeout
    }
    catch [System.TimeoutException] {
        throw [System.TimeoutException]::new("$($_.Exception.Message); last visible text: '$($seen.Value)'")
    }
}

function ConvertTo-E2ELabelKey {
    param([AllowEmptyString()][string]$Text)
    -join ($Text.ToLowerInvariant().ToCharArray() | Where-Object { [char]::IsLetterOrDigit($_) })
}

function Find-E2ELabel {
    <# Returns @(x, y) window coordinates of the label's center in the top 270 pixels, or $null. #>
    param([Parameter(Mandatory)][JiffshotE2E.TextReader]$Reader, [Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [Parameter(Mandatory)][string]$Label)
    $scale = 3
    $data = $Reader.Recognize($Driver.Capture(0, 0, 800, 270), 6, $true)
    $lines = [ordered]@{}
    foreach ($row in ($data -split "`n")) {
        $fields = $row.TrimEnd("`r") -split "`t", 12
        if ($fields.Count -lt 12 -or $fields[0] -eq 'level') {
            continue
        }
        $text = $fields[11].Trim()
        if (-not $text) {
            continue
        }
        $key = "$($fields[2])/$($fields[3])/$($fields[4])"
        if (-not $lines.Contains($key)) {
            $lines[$key] = [System.Collections.Generic.List[object]]::new()
        }
        $lines[$key].Add([pscustomobject]@{ Left = [int]$fields[6]; Top = [int]$fields[7]; Width = [int]$fields[8]; Height = [int]$fields[9]; Text = $text })
    }
    $target = ConvertTo-E2ELabelKey $Label
    $span = @($Label.Split([char[]]" `t`n", [StringSplitOptions]::RemoveEmptyEntries)).Count + 2
    foreach ($words in $lines.Values) {
        for ($start = 0; $start -lt $words.Count; $start++) {
            $combined = ''
            for ($end = $start; $end -lt [Math]::Min($words.Count, $start + $span); $end++) {
                $combined += ConvertTo-E2ELabelKey $words[$end].Text
                if ($combined -eq $target) {
                    $left = $words[$start].Left
                    $right = $words[$end].Left + $words[$end].Width
                    $top = ($words[$start..$end] | ForEach-Object Top | Measure-Object -Minimum).Minimum
                    $bottom = ($words[$start..$end] | ForEach-Object { $_.Top + $_.Height } | Measure-Object -Maximum).Maximum
                    return , @([int][Math]::Floor(($left + $right) / (2 * $scale)), [int][Math]::Floor(($top + $bottom) / (2 * $scale)))
                }
                if ($combined.Length -gt $target.Length) {
                    break
                }
            }
        }
    }
    $null
}

function Find-E2EScrollingLabel {
    <# Finds a label, scrolling the sidebar from the top when it is not visible. #>
    param([Parameter(Mandatory)][JiffshotE2E.TextReader]$Reader, [Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [Parameter(Mandatory)][string]$Label)
    $position = Find-E2ELabel $Reader $Driver $Label
    if ($null -ne $position) {
        return , $position
    }
    Invoke-E2EScroll $Driver up
    Wait-E2ESeconds 0.2
    for ($attempt = 0; $attempt -lt 25; $attempt++) {
        $position = Find-E2ELabel $Reader $Driver $Label
        if ($null -ne $position) {
            return , $position
        }
        Invoke-E2EScroll $Driver down 2
        Wait-E2ESeconds 0.12
    }
    Write-E2ETrace $Driver 'label-not-found'
    $observed = $Reader.Recognize($Driver.Capture(0, 0, 800, 270), 6, $false)
    throw [System.Exception]::new("AssertionError: could not find '$Label'; OCR saw '$observed'")
}

#endregion

#region Documents

function Get-E2ESolidPng {
    <# Returns a 240x140 RGB PNG filled with one color. #>
    param([int]$Red, [int]$Green, [int]$Blue, [int]$Width = 240, [int]$Height = 140)
    , [JiffshotE2E.Png]::Solid($Width, $Height, $Red, $Green, $Blue)
}

function New-E2ESelectionDocument {
    <# Writes a 240x140 white .jiffshot document with two rectangles. #>
    param([Parameter(Mandatory)][string]$Path)
    $width = 240
    $height = 140
    $objects = foreach ($pair in @(@(1, 15), @(2, 130))) {
        $identifier, $left = $pair
        [ordered]@{
            id    = $identifier
            kind  = [ordered]@{
                type  = 'Shape'
                rect  = [ordered]@{ x0 = $left; y0 = 15; x1 = $left + 70; y1 = 110 }
                shape = 'Rectangle'
            }
            style = @{}
        }
    }
    $manifest = [ordered]@{
        format = 'jiffshot-document'; version = 1
        width = $width; height = $height; objects = @($objects)
    }
    $archive = [System.IO.Compression.ZipFile]::Open($Path, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        $entries = [ordered]@{
            'document.json'  = [System.Text.Encoding]::UTF8.GetBytes((ConvertTo-Json -InputObject $manifest -Depth 16 -Compress))
            'background.png' = (Get-E2ESolidPng 255 255 255 $width $height)
        }
        foreach ($name in $entries.Keys) {
            $stream = $archive.CreateEntry($name, [System.IO.Compression.CompressionLevel]::NoCompression).Open()
            try {
                $bytes = [byte[]]$entries[$name]
                $stream.Write($bytes, 0, $bytes.Length)
            }
            finally {
                $stream.Dispose()
            }
        }
    }
    finally {
        $archive.Dispose()
    }
}

function Read-E2EPng {
    param([Parameter(Mandatory)][string]$Path)
    [JiffshotE2E.Png]::Decode([System.IO.File]::ReadAllBytes($Path))
}

#endregion

Export-ModuleMember -Function * -Variable PortalDesktopPath, PortalShortcutsInterface, PortalSessionInterface, PortalRequestInterface, PortalScreenCastInterface, PortalScreenshotInterface
