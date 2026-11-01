#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Exercise GIF video export through the Linux editor and file chooser.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.Editor.psm1') -Force

function New-VideoPortal {
    $portal = New-EffectsPortal
    $portal.Handler = ${function:Invoke-VideoPortalMessage}
    $portal
}

function Invoke-VideoPortalMessage {
    param($Portal, [JiffshotE2E.IncomingCall]$Call)
    if ($Call.Interface -eq $PortalFileChooserInterface -and $Call.Member -eq 'SaveFile') {
        $parent, $title, $options = $Call.Body
        $request = Get-PortalRequestPath $Call $options
        $Portal.SaveParents.Add($parent)
        $Portal.SaveRequests.Add(@($title, $options, $request))
        $Call.Reply('o', @($request))
        return $true
    }
    Invoke-EffectsPortalMessage $Portal $Call
}

function Complete-VideoPortalSave {
    param([Parameter(Mandatory)]$Portal, [Parameter(Mandatory)][int]$Index, [Parameter(Mandatory)][string]$Destination)
    $request = $Portal.SaveRequests[$Index][2]
    Send-PortalResponseLater $Portal $request 0 @{ uris = (New-DBusVariant 'as' ([string[]]@(ConvertTo-E2EFileUri $Destination))) }
}

function Write-Video {
    param([Parameter(Mandatory)][string]$Path)
    $null = Invoke-E2ECommand @(
        'gst-launch-1.0', '-q',
        'videotestsrc', 'num-buffers=60', '!',
        'video/x-raw,width=32,height=24,framerate=60/1', '!',
        'jpegenc', '!', 'avimux', '!', 'filesink', "location=$Path"
    ) -Timeout 10 -Check
}

function Get-GifDelays {
    <# Returns @{ Dimensions = @(width, height); Delays = int[] } for a GIF. #>
    param([Parameter(Mandatory)][string]$Path)
    $data = [System.IO.File]::ReadAllBytes($Path)
    $signature = [System.Text.Encoding]::ASCII.GetString($data, 0, [Math]::Min(6, $data.Length))
    Assert-E2E ($signature -in @('GIF87a', 'GIF89a')) 'output must be a GIF'
    $width = [BitConverter]::ToUInt16($data, 6)
    $height = [BitConverter]::ToUInt16($data, 8)
    $packed = $data[10]
    $position = 13
    if ($packed -band 0x80) {
        $position += 3 * (1 -shl (($packed -band 7) + 1))
    }
    $delays = [System.Collections.Generic.List[int]]::new()
    $delay = $null
    $skipBlocks = {
        param([int]$Index)
        while ($true) {
            $length = $data[$Index]
            $Index++
            if ($length -eq 0) {
                return $Index
            }
            $Index += $length
        }
    }
    while ($position -lt $data.Length) {
        $kind = $data[$position]
        $position++
        if ($kind -eq 0x3B) {
            return [pscustomobject]@{ Dimensions = @([int]$width, [int]$height); Delays = $delays.ToArray() }
        }
        if ($kind -eq 0x21) {
            $label = $data[$position]
            $position++
            if ($label -eq 0xF9) {
                Assert-E2E ($data[$position] -eq 4) 'invalid GIF graphics control extension'
                $delay = [BitConverter]::ToUInt16($data, $position + 2) * 10
            }
            $position = & $skipBlocks $position
        }
        elseif ($kind -eq 0x2C) {
            $packed = $data[$position + 8]
            $position += 9
            if ($packed -band 0x80) {
                $position += 3 * (1 -shl (($packed -band 7) + 1))
            }
            $position = & $skipBlocks ($position + 1)
            Assert-E2E ($null -ne $delay) 'every GIF frame must specify a delay'
            $delays.Add($delay)
            $delay = $null
        }
        else {
            throw [System.Exception]::new(('AssertionError: unexpected GIF block 0x{0:x2}' -f $kind))
        }
    }
    throw [System.Exception]::new('AssertionError: GIF is missing its trailer')
}

function Invoke-ChooseExport {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [Parameter(Mandatory)]$Portal, [Parameter(Mandatory)][string]$Source)
    $openIndex = $Portal.OpenRequests.Count
    $saveIndex = $Portal.SaveRequests.Count
    $Driver.Click(670, 207)
    Wait-E2EUntil { $Portal.OpenRequests.Count -gt $openIndex } 'the video chooser'
    $title = $Portal.OpenRequests[$openIndex][0]
    Assert-E2E ($title -ceq 'Select video to export as animated GIF') $title
    Assert-E2E ((ConvertFrom-E2EWindowHandle $Portal.OpenParents[$openIndex]) -eq $Driver.Window) 'the video chooser must be parented to the editor'
    Complete-EffectsPortalOpen $Portal $openIndex 0 -Uri (ConvertTo-E2EFileUri $Source)
    Wait-E2EUntil { $Portal.SaveRequests.Count -gt $saveIndex } 'the GIF save chooser'
    Assert-E2E ((ConvertFrom-E2EWindowHandle $Portal.SaveParents[$saveIndex]) -eq $Driver.Window) 'the GIF save chooser must be parented to the editor'
    Assert-E2E ($Portal.SaveRequests[$saveIndex][0] -ceq 'Save animated GIF') "$($Portal.SaveRequests[$saveIndex][0])"
    $saveIndex
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$app = $server = $driver = $portal = $null
$work = New-E2EWorkDirectory 'jiffshot-video-export-ui-'
try {
    $portal = New-VideoPortal
    New-E2EPrivateDirectory (Join-Path $work 'runtime') | Out-Null
    $source = Join-Path $work 'source.avi'
    Write-Video $source
    $original = [System.IO.File]::ReadAllBytes($source)
    $alias = Join-Path $work 'source-alias.gif'
    [JiffshotE2E.Posix]::Link($source, $alias)
    $image = Join-Path $work 'image.png'
    [System.IO.File]::WriteAllBytes($image, (Get-E2ESolidPng 30 120 190))
    $config = Join-Path $work 'config/Jiffshot'
    New-Item -ItemType Directory -Path $config | Out-Null
    Write-E2EJson (Join-Path $config 'settings.json') ([ordered]@{ theme = 'Light' })
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $displayName = $server.Display
        $environment = New-E2EEnvironment $displayName $work $runtime
        $app = Start-E2EProcess @($binaryPath, $image) $environment $work $appLog
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'the editor' 12
        $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
        Wait-E2EUntil { (Measure-E2ECanvasColor $driver 30 120 190) -gt 5000 } 'the source image'
        Wait-E2ESeconds 0.5
        Write-E2ETrace $driver 'video-export-controls'

        $destination = Join-Path $work 'animation.gif'
        $saveIndex = Invoke-ChooseExport $driver $portal $source
        Complete-VideoPortalSave $portal $saveIndex $destination
        Wait-E2EUntil { [System.IO.File]::Exists($destination) } 'the completed GIF' 15
        $gif = Get-GifDelays $destination
        Assert-E2E (Test-E2EDeepEqual $gif.Dimensions @(32, 24)) "$($gif.Dimensions -join ', ')"
        $delays = $gif.Delays
        Assert-E2E ($delays.Count -eq 60) "expected 60 GIF frames, got $($delays.Count)"
        $distinct = @($delays | Sort-Object -Unique)
        Assert-E2E (Test-E2EDeepEqual $distinct @(10, 20)) "$($delays -join ', ')"
        $total = ($delays | Measure-Object -Sum).Sum
        Assert-E2E ($total -eq 1000) "expected one second of GIF timing, got $total"

        $saveIndex = Invoke-ChooseExport $driver $portal $source
        $beforeStatus = Get-E2EStatusFrame $driver
        Complete-VideoPortalSave $portal $saveIndex $alias
        Wait-E2EUntil { -not (Get-E2EStatusFrame $driver).SameAs($beforeStatus) } 'the source-overwrite error'
        Assert-E2E ([System.Linq.Enumerable]::SequenceEqual[byte]([System.IO.File]::ReadAllBytes($source), $original)) 'GIF export must not overwrite the source'
        Assert-E2E ([System.Linq.Enumerable]::SequenceEqual[byte]([System.IO.File]::ReadAllBytes($alias), $original)) 'GIF export must not overwrite a source alias'
        Write-E2ELine 'GUI GIF export preserved 60 fps timing and rejected a source alias'
    }
    catch {
        if ($null -ne $driver) {
            Write-E2ETrace $driver 'video-export-failure'
        }
        Write-E2ELogTail @($appLog, $xvfbLog)
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
