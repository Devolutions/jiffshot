#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Ensure the private X server retains state between GUI test probes.
#>

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force

$log = [System.IO.Path]::GetTempFileName()
$server = $null
try {
    $server = Start-E2EXvfb $log
    $displayName = $server.Display
    $property = '_JIFFSHOT_E2E_PROBE'
    $null = & xprop -display $displayName -root -f $property 8s -set $property retained
    Assert-E2E ($LASTEXITCODE -eq 0) 'could not set the X server probe property'
    Wait-E2ESeconds 0.25
    $null = & xwininfo -display $displayName -root -tree
    Assert-E2E ($LASTEXITCODE -eq 0) 'could not inspect the X server'
    Wait-E2ESeconds 0.25
    $result = & xprop -display $displayName -root $property
    Assert-E2E ($LASTEXITCODE -eq 0) 'could not read the X server probe property'
    Assert-E2E ($result -ceq '_JIFFSHOT_E2E_PROBE(STRING) = "retained"') "X server reset between clients: $result"
    Write-E2ELine 'PASS xvfb_preserves_state_between_window_probes'
}
catch {
    Write-E2ELogTail @($log)
    throw
}
finally {
    if ($null -ne $server) { Stop-E2EProcess $server.Process }
    Remove-Item -LiteralPath $log -Force
}
