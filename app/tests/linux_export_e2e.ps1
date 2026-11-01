#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify PDF export through the Linux save dialog.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.Editor.psm1') -Force

function New-SavePortal {
    param([Parameter(Mandatory)][string]$Destination)
    $portal = New-MockPortal
    $portal | Add-Member -NotePropertyMembers ([ordered]@{
            Destination  = $Destination
            SaveRequests = [System.Collections.Generic.List[object]]::new()
        })
    $portal.Handler = ${function:Invoke-SavePortalMessage}
    $portal
}

function Invoke-SavePortalMessage {
    param($Portal, [JiffshotE2E.IncomingCall]$Call)
    if ($Call.Interface -eq $PortalFileChooserInterface -and $Call.Member -eq 'SaveFile') {
        $null, $null, $options = $Call.Body
        $request = Get-PortalRequestPath $Call $options
        $Portal.SaveRequests.Add($options)
        Send-PortalResponseLater $Portal $request 0 @{ uris = (New-DBusVariant 'as' @(ConvertTo-E2EFileUri $Portal.Destination)) }
        $Call.Reply('o', @($request))
        return $true
    }
    Invoke-MockPortalMessage $Portal $Call
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$app = $server = $driver = $portal = $null
$work = New-E2EWorkDirectory 'jiffshot-export-ui-'
try {
    New-E2EPrivateDirectory (Join-Path $work 'runtime') | Out-Null
    $image = Join-Path $work 'input.png'
    [System.IO.File]::WriteAllBytes($image, (Get-E2ESolidPng 220 20 20))
    $destination = Join-Path $work 'export.pdf'
    $portal = New-SavePortal $destination
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $displayName = $server.Display
        $environment = New-E2EEnvironment $displayName $work $runtime
        $app = Start-E2EProcess @($binaryPath, $image) $environment $work $appLog
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'the editor' 12
        $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
        Wait-E2EUntil { $driver.RedCanvasPixels() -gt 2000 } 'the image to export'
        Invoke-E2EHotkey $driver 'e'
        Wait-E2EUntil { $portal.SaveRequests.Count -gt 0 } 'the export save dialog' 15
        $options = $portal.SaveRequests[$portal.SaveRequests.Count - 1]
        Assert-E2E $options.ContainsKey('filters') 'the save dialog must expose image and PDF filters'
        $patterns = foreach ($filter in $options['filters'].Value) {
            foreach ($entry in $filter[1]) { $entry[1] }
        }
        Assert-E2E ($patterns -contains '*.pdf') "$($patterns -join ', ')"
        Wait-E2EUntil {
            Test-E2EFileEnd $destination ([System.Text.Encoding]::ASCII.GetBytes("%%EOF`n"))
        } 'the exported PDF' 20
        $output = [System.Text.Encoding]::Latin1.GetString([System.IO.File]::ReadAllBytes($destination))
        Assert-E2E $output.StartsWith('%PDF-1.4', [StringComparison]::Ordinal) 'the selected extension must produce a PDF'
        Assert-E2E $output.Contains('/Width 240 /Height 140', [StringComparison]::Ordinal) 'PDF must contain the selected image'
        Write-E2ELine 'GUI export offered PDF and saved the selected image as a one-page PDF'
    }
    catch {
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
