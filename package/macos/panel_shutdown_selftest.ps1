#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify that quitting cancels pending automatic-share file panels.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$HostLibrary,
    [string]$Output,
    [ValidateSet('all', 'explicit', 'native', 'logout', 'retry')][string]$Case = 'all'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacFixture.psm1') -Force

function Invoke-PanelCase([string]$Root, [string]$BinaryPath, [string]$HostPath, [string]$Name) {
    $caseRoot = New-FixtureRoot (Join-Path $Root $Name) 'unused-'
    Initialize-FixtureDirectories $caseRoot
    $environment = New-FixtureEnvironment $caseRoot $HostPath 'JIFFSHOT_UITEST_PANEL_SHUTDOWN'
    $environment['JIFFSHOT_UITEST_PANEL_SHUTDOWN'] = $Name
    if ($Name -ceq 'logout') { $environment['JIFFSHOT_UITEST_TERMINATION_OS_REASON'] = '1' }
    $process = Start-FixtureProcess -File $BinaryPath -Environment $environment -LogPath (Join-Path $caseRoot 'process.log')
    try { $result = Wait-FixtureProcess $process 30 }
    finally { Stop-FixtureProcess $process }
    if ($result.ExitCode -ne 0) { throw "${Name}: file-panel shutdown exited with $($result.ExitCode)" }
    $markers = @(
        'started an automatic share on the capture engine',
        'requested quit with an automatic share panel open'
    )
    if ($Name -ceq 'retry') {
        $markers += 'unsaved edits cancelled quit without closing the share panel'
        $markers += 'a failed quit restored file panels and the next picker opened'
    }
    else { $markers += 'pending and late share panels were cancelled after saving their captures' }
    if ($Name -cin @('native', 'logout')) { $markers += 'AppKit WillTerminate observed after cleanup' }
    Assert-FixtureMarkers (Join-Path $caseRoot 'ui-log.txt') $markers
    Write-Host "${Name}: passed"
}

$binaryPath = Resolve-FixtureFile $Binary
$hostPath = Resolve-FixtureFile $HostLibrary
$root = New-FixtureRoot $Output 'jiffshot-panel-shutdown-'
try {
    $cases = if ($Case -ceq 'all') { @('explicit', 'native', 'logout', 'retry') } else { @($Case) }
    foreach ($name in $cases) { Invoke-PanelCase $root $binaryPath $hostPath $name }
}
catch {
    [Console]::Error.WriteLine("$($_.Exception.Message)`nFixture retained at $root")
    throw
}
if ($Output) { Write-Host "Fixture output: $root" }
else { Remove-FixtureRoot $root }
