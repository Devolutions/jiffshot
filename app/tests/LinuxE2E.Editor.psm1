#Requires -Version 7.4
# Editor helpers shared by the Linux GUI end-to-end drivers: a FileChooser-capable mock
# portal (EffectsPortal), editor pixel regions, hotkeys, and native command helpers.
# Import LinuxE2E.psm1 first; this module reuses that instance so mock services share one pump.

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'

if (-not (Get-Module LinuxE2E)) {
    Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Global
}

$PortalFileChooserInterface = 'org.freedesktop.portal.FileChooser'

#region Native commands, URIs, and editor regions

function Invoke-E2ECommand {
    <# Runs a native command like subprocess.run; returns @{ ExitCode; Output } and throws on timeout or (with -Check) failure. #>
    param(
        [Parameter(Mandatory)][string[]]$ArgumentList,
        [System.Collections.IDictionary]$Environment,
        [AllowNull()][byte[]]$InputBytes,
        [switch]$CaptureOutput,
        [double]$Timeout = 0,
        [switch]$Check
    )
    $info = [System.Diagnostics.ProcessStartInfo]::new($ArgumentList[0])
    for ($index = 1; $index -lt $ArgumentList.Count; $index++) {
        $info.ArgumentList.Add($ArgumentList[$index])
    }
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = [bool]$CaptureOutput
    if ($null -ne $Environment) {
        $info.Environment.Clear()
        foreach ($key in $Environment.Keys) {
            if ($null -ne $Environment[$key]) {
                $info.Environment[[string]$key] = [string]$Environment[$key]
            }
        }
    }
    $process = [System.Diagnostics.Process]::Start($info)
    try {
        $buffer = [System.IO.MemoryStream]::new()
        $reader = if ($CaptureOutput) { $process.StandardOutput.BaseStream.CopyToAsync($buffer) }
        if ($null -ne $InputBytes) {
            $process.StandardInput.BaseStream.Write($InputBytes, 0, $InputBytes.Length)
        }
        $process.StandardInput.Close()
        $milliseconds = if ($Timeout -gt 0) { [int]($Timeout * 1000) } else { -1 }
        if (-not $process.WaitForExit($milliseconds)) {
            $process.Kill()
            $null = $process.WaitForExit(5000)
            throw [System.TimeoutException]::new("$($ArgumentList -join ' ') timed out after $Timeout seconds")
        }
        $process.WaitForExit()
        $output = [byte[]]@()
        if ($CaptureOutput) {
            $null = $reader.Wait(5000)
            $output = $buffer.ToArray()
        }
        if ($Check -and $process.ExitCode -ne 0) {
            throw "$($ArgumentList -join ' ') returned non-zero exit status $($process.ExitCode)"
        }
        [pscustomobject]@{ ExitCode = $process.ExitCode; Output = $output }
    }
    finally {
        $process.Dispose()
    }
}

function ConvertTo-E2EFileUri {
    <# Returns a file:// URI for an absolute path, like Path.as_uri(). #>
    param([Parameter(Mandatory)][string]$Path)
    [System.Uri]::new($Path).AbsoluteUri
}

function ConvertFrom-E2EWindowHandle {
    <# Parses an XDG portal parent window such as x11:1a00003. #>
    param([Parameter(Mandatory)][string]$Parent)
    $value = if ($Parent.StartsWith('x11:')) { $Parent.Substring(4) } else { $Parent }
    [Convert]::ToInt64($value, 16)
}

function Test-E2EFileEnd {
    <# Returns whether a file exists and ends with Suffix. #>
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][byte[]]$Suffix)
    if (-not [System.IO.File]::Exists($Path)) {
        return $false
    }
    $data = [System.IO.File]::ReadAllBytes($Path)
    if ($data.Length -lt $Suffix.Length) {
        return $false
    }
    $offset = $data.Length - $Suffix.Length
    for ($index = 0; $index -lt $Suffix.Length; $index++) {
        if ($data[$offset + $index] -ne $Suffix[$index]) {
            return $false
        }
    }
    $true
}

function Invoke-E2EHotkey {
    <# Presses Ctrl+Key. #>
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [Parameter(Mandatory)][string]$Key)
    $Driver.Key('Control_L', $true)
    $Driver.Key($Key, $true)
    $Driver.Key($Key, $false)
    $Driver.Key('Control_L', $false)
    $Driver.Sync()
}

function Get-E2EStatusFrame {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver)
    $Driver.Capture(0, 480, 800, 20)
}

function Get-E2ECanvasFrame {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver)
    $Driver.Capture(0, 280, 800, 190)
}

function Measure-E2ECanvasColor {
    <# Counts canvas pixels that exactly match an RGB color. #>
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [int]$Red, [int]$Green, [int]$Blue)
    (Get-E2ECanvasFrame $Driver).CountColor($Red, $Green, $Blue)
}

#endregion

#region Mock portal with a FileChooser (EffectsPortal)

function New-EffectsPortal {
    <# Returns a mock portal that also records FileChooser OpenFile and SaveFile requests. #>
    $portal = New-MockPortal
    $portal | Add-Member -NotePropertyMembers ([ordered]@{
            OpenRequests      = [System.Collections.Generic.List[object]]::new()
            SaveRequests      = [System.Collections.Generic.List[object]]::new()
            SaveDestinations  = [System.Collections.Generic.List[object]]::new()
            SaveResponses     = [System.Collections.Generic.List[object]]::new()
            OpenParents       = [System.Collections.Generic.List[object]]::new()
            SaveParents       = [System.Collections.Generic.List[object]]::new()
            FailNextOpen      = $false
            PendingOpenErrors = @{}
        })
    $portal.Handler = ${function:Invoke-EffectsPortalMessage}
    $portal
}

function Invoke-EffectsPortalMessage {
    param($Portal, [JiffshotE2E.IncomingCall]$Call)
    $body = $Call.Body
    if ($Call.Interface -eq 'org.freedesktop.DBus.Properties' -and $Call.Member -eq 'Get' -and $body[0] -eq $PortalFileChooserInterface) {
        $Call.Reply('v', @((New-DBusVariant 'u' 4)))
        return $true
    }
    if ($Call.Interface -eq $PortalFileChooserInterface -and $Call.Member -in @('OpenFile', 'SaveFile')) {
        $parent, $title, $options = $body
        $request = Get-PortalRequestPath $Call $options
        if ($Call.Member -eq 'OpenFile') {
            $Portal.OpenRequests.Add(@($title, $options, $request))
            $Portal.OpenParents.Add($parent)
            if ($Portal.FailNextOpen) {
                $Portal.FailNextOpen = $false
                $Portal.PendingOpenErrors[$Portal.OpenRequests.Count - 1] = $Call
                return $true
            }
        }
        else {
            $Portal.SaveParents.Add($parent)
            if ($Portal.SaveResponses.Count -gt 0) {
                $code, $result = $Portal.SaveResponses[0]
                $Portal.SaveResponses.RemoveAt(0)
                $Portal.SaveRequests.Add(@($title, $options, $null))
                Send-PortalResponseLater $Portal $request $code $result
            }
            elseif ($Portal.SaveDestinations.Count -eq 0) {
                $Call.Error('org.freedesktop.portal.Error.Failed', 'unexpected save dialog')
                return $true
            }
            else {
                $destination = $Portal.SaveDestinations[0]
                $Portal.SaveDestinations.RemoveAt(0)
                $Portal.SaveRequests.Add(@($title, $options, $destination))
                Send-PortalResponseLater $Portal $request 0 @{ uris = (New-DBusVariant 'as' @(ConvertTo-E2EFileUri $destination)) }
            }
        }
        $Call.Reply('o', @($request))
        return $true
    }
    Invoke-MockPortalMessage $Portal $Call
}

function Complete-EffectsPortalOpen {
    <# Answers OpenFile request Index with Code and an optional URI (finish_open). #>
    param(
        [Parameter(Mandatory)]$Portal,
        [Parameter(Mandatory)][int]$Index,
        [Parameter(Mandatory)][uint32]$Code,
        [string]$Uri,
        [switch]$OmitUris
    )
    $request = $Portal.OpenRequests[$Index][2]
    # The XDG FileChooser frontend includes an empty URI array on non-success.
    $uris = [string[]]@()
    if ($Code -eq 0) {
        $uris = [string[]]@($Uri)
    }
    $result = if ($OmitUris) { @{} } else { @{ uris = (New-DBusVariant 'as' $uris) } }
    # Allow the picker to subscribe to the portal response signal.
    Send-PortalResponseLater $Portal $request $Code $result
}

function Send-EffectsPortalOpenError {
    <# Fails the held OpenFile call Index with a portal error (fail_open). #>
    param([Parameter(Mandatory)]$Portal, [Parameter(Mandatory)][int]$Index)
    $call = $Portal.PendingOpenErrors[$Index]
    $Portal.PendingOpenErrors.Remove($Index)
    $call.Error('org.freedesktop.portal.Error.Failed', 'mock file chooser unavailable')
}

#endregion

Export-ModuleMember -Function * -Variable PortalFileChooserInterface
