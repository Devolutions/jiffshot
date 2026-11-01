#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify library Copy against synthetic captures on a private pasteboard.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$HostLibrary,
    [string]$Output,
    [switch]$AutosaveFailure
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacFixture.psm1') -Force

$markers = @(
    'library Copy included the current unsaved edits',
    'library Copy included saved document edits',
    'library Copy included the separate editable pointer',
    'library Copy represented a recording as a file',
    'multi-selection copied flattened snapshots and retained the original files',
    'failed preparation left the previous clipboard intact',
    'a newer clipboard change was not overwritten',
    'library Copy retried while the editor was busy',
    'the latest library Copy request won',
    'library Copy included unsaved edits opened directly as a document'
)
$binaryPath = Resolve-FixtureFile $Binary
$hostPath = Resolve-FixtureFile $HostLibrary
$root = New-FixtureRoot $Output 'jiffshot-library-copy-'
try {
    Initialize-FixtureDirectories $root
    $environment = New-FixtureEnvironment $root $hostPath 'JIFFSHOT_UITEST_LIBRARY_COPY'
    $environment['JIFFSHOT_UITEST_SHUTDOWN_POST_FAILURES'] = '1'
    $environment['JIFFSHOT_UITEST_SHUTDOWN_POST_TARGET'] = 'the library copy result'
    if ($AutosaveFailure) { $environment['JIFFSHOT_UITEST_LIBRARY_COPY_AUTOSAVE_FAILURE'] = '1' }
    $process = Start-FixtureProcess -File $binaryPath -Environment $environment -LogPath (Join-Path $root 'process.log')
    try { $result = Wait-FixtureProcess $process 60 }
    finally { Stop-FixtureProcess $process }
    if ($result.ExitCode -ne 0) { throw "Library Copy fixture exited with $($result.ExitCode)" }
    Assert-FixtureMarkers (Join-Path $root 'ui-log.txt') $markers
    $log = [IO.File]::ReadAllText((Join-Path $root 'process.log'))
    if (-not $log.Contains('queued the library copy result after retrying the UI dispatcher')) {
        throw 'The library Copy dispatcher retry was not exercised'
    }
    $edits = Join-Path $root 'library/2026-01/saved.jsd'
    if ($AutosaveFailure) {
        Assert-FixtureMarkers (Join-Path $root 'ui-log.txt') @('blocked the final autosave without changing the prior document')
        if (-not $log.Contains('Final editor autosave failed:')) { throw 'The final autosave failure was not reported' }
        $edits = "$edits.saved"
    }
    $manifest = Read-FixtureZipJson $edits 'document.json'
    $expectedWidth = if ($AutosaveFailure) { 7 } else { 11 }
    $expectedHeight = if ($AutosaveFailure) { 6 } else { 10 }
    if ($manifest.width -ne $expectedWidth -or $manifest.height -ne $expectedHeight) {
        throw "The final document has unexpected dimensions; expected ${expectedWidth}x$expectedHeight"
    }
}
catch {
    [Console]::Error.WriteLine("$($_.Exception.Message)`nFixture retained at $root")
    throw
}
if ($Output) { Write-Host "Library Copy fixture passed: $root" }
else { Remove-FixtureRoot $root; Write-Host 'Library Copy fixture passed' }
