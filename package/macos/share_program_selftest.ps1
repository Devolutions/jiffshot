#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Exercise the application picker and custom sharing with a disposable app.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$HostLibrary,
    [string]$Output
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacPackage.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacFixture.psm1') -Force

$markers = @(
    'selected application was saved and published',
    'the selected application received the exported image',
    'Open With delivered the capture to the selected application',
    'Open With cancellation was reported',
    'default Open With cancellation was reported',
    'automatic image sharing delivered the editable pointer',
    'automatic recording sharing delivered the recording',
    'automatic image sharing cancellation preserved the capture',
    'automatic recording sharing cancellation preserved the recording',
    'missing application errors were reported',
    'application picker cancellation left the destination unchanged',
    'reserved destination names were rejected',
    'settings failures were reported without changing saved destinations',
    'stale Share preferences preserved concurrent destination changes',
    'failed removal preserved all destinations',
    'malformed settings were not overwritten by changed Share preferences',
    'malformed settings were not overwritten by unrelated preferences',
    'failed Editor preferences preserved the previous settings',
    'Editor preferences saved together without changing Share',
    'FTP and multiple destination removals were saved together',
    'the UI remained responsive while a share was blocked',
    'the sharing worker reported completion',
    'FTP errors were reported by the sharing worker',
    'folder picker cancellation was reported',
    'folder sharing copied the image without replacing an existing file'
)

function New-ShareFixture([string]$Root) {
    Initialize-FixtureDirectories $Root
    $applications = Join-Path $Root 'applications'
    $contents = Join-Path $applications 'Fixture Share.app/Contents'
    $macos = Join-Path $contents 'MacOS'
    [void][IO.Directory]::CreateDirectory($macos)
    $executable = Join-Path $macos 'FixtureShare'
    [void](Invoke-MacCommand -File 'xcrun' -Arguments @(
        'clang', '-fobjc-arc', '-framework', 'Cocoa',
        (Join-Path $PSScriptRoot 'fixtures/share-program/Fixture.m'), '-o', $executable
    ))
    Write-MacPlist (Join-Path $contents 'Info.plist') ([ordered]@{
        CFBundleExecutable = 'FixtureShare'
        CFBundleIdentifier = 'com.devolutions.jiffshot.sharefixture.' + [guid]::NewGuid().ToString('N')
        CFBundleName = 'Fixture Share'
        CFBundlePackageType = 'APPL'
        CFBundleVersion = '1'
        CFBundleDocumentTypes = @([ordered]@{
            CFBundleTypeName = 'Fixture media'
            CFBundleTypeRole = 'Viewer'
            LSItemContentTypes = @('public.png', 'public.mpeg-4')
        })
        LSUIElement = $true
    })
    [void](Invoke-MacCommand -File '/usr/bin/codesign' -Arguments @('--force', '--sign', '-', [IO.Path]::GetDirectoryName($contents)))
    [IO.File]::WriteAllText((Join-Path $applications 'Not an application.exe'), '')
    $folderName = 'Jiffshot Fixture Destination ' + [guid]::NewGuid().ToString('N')
    $destination = Join-Path $applications $folderName
    [void][IO.Directory]::CreateDirectory($destination)
    [IO.File]::WriteAllText((Join-Path $destination 'Untitled.png'), 'existing fixture')
    $driver = Join-Path $Root 'drive'
    [void](Invoke-MacCommand -File 'xcrun' -Arguments @(
        'swiftc', (Join-Path $PSScriptRoot 'fixtures/share-program/drive.swift'), '-o', $driver
    ))
    return [pscustomobject]@{ Driver = $driver; FolderName = $folderName }
}

function Invoke-ShareCase([string]$Root, [string]$BinaryPath, [string]$HostPath, [string]$Driver, [string]$DriverArgument, [string]$Mode, [string[]]$Expected, [int]$DriverTimeout, [int]$ExitTimeout) {
    $environment = New-FixtureEnvironment $Root $HostPath $Mode
    if ($Mode -ceq 'JIFFSHOT_UITEST_SHARE_PROGRAM') { $environment['JIFFSHOT_UITEST_SHARE_FOLDER'] = $DriverArgument }
    $process = Start-FixtureProcess -File $BinaryPath -Environment $environment -LogPath (Join-Path $Root 'process.log')
    Write-Host "Fixture process: $($process.Process.Id)"
    try {
        $result = Invoke-MacCommand -File $Driver -Arguments @([string]$process.Process.Id, $DriverArgument) `
            -TimeoutSeconds $DriverTimeout -AllowFailure
        [Console]::Out.Write($result.Stdout)
        [Console]::Error.Write($result.Stderr)
        if ($result.ExitCode -ne 0) { throw "The sharing fixture driver exited with $($result.ExitCode)" }
        $appResult = Wait-FixtureProcess $process $ExitTimeout
    }
    finally { Stop-FixtureProcess $process }
    if ($appResult.ExitCode -ne 0) { throw "The sharing fixture app exited with $($appResult.ExitCode)" }
    Assert-FixtureMarkers (Join-Path $Root 'ui-log.txt') $Expected
}

$binaryPath = Resolve-FixtureFile $Binary
$hostPath = Resolve-FixtureFile $HostLibrary
$root = New-FixtureRoot $Output 'jiffshot-share-selftest-'
try {
    $fixture = New-ShareFixture $root
    Invoke-ShareCase $root $binaryPath $hostPath $fixture.Driver $fixture.FolderName `
        'JIFFSHOT_UITEST_SHARE_PROGRAM' $markers 360 45
    $preferences = New-FixtureRoot (Join-Path $root 'preferences') 'unused-'
    Initialize-FixtureDirectories $preferences
    Invoke-ShareCase $preferences $binaryPath $hostPath $fixture.Driver "prefs:$preferences" `
        'JIFFSHOT_UITEST_SHARE_PREFS' @(
            'the Preferences window rejected a stale Share removal',
            'the Preferences window removed only the selected destination',
            'the Preferences window corrected the legacy FTP value',
            'the Preferences window cleared the FTP value'
        ) 90 25
    Write-Host 'Custom application sharing passed.'
}
catch {
    [Console]::Error.WriteLine("$($_.Exception.Message)`nFixture retained at $root")
    throw
}
if ($Output) { Write-Host "Fixture output: $root" }
else { Remove-FixtureRoot $root }
