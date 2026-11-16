#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify CLI-started macOS applications stay nonactivating until explicitly opened.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$BinaryDirectory,
    [string]$HostDirectory,
    [Parameter(Mandatory)][string]$Output,
    [switch]$ExpectedFailure
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacPackage.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacFixture.psm1') -Force

$root = New-FixtureRoot -Output $Output -Prefix 'jbg-'
Initialize-FixtureDirectories $root
[IO.File]::WriteAllText((Join-Path $root 'config/jiffshot.json'),
    '{"capture":{"show_widget":true,"capture_hotkey":"NotAHotkey"}}')
$probe = Join-Path $root 'background-probe'
[void](Invoke-MacCommand -File 'xcrun' -Arguments @('clang', '-fobjc-arc', '-framework', 'Cocoa', '-framework', 'CoreGraphics',
    (Join-Path $PSScriptRoot 'fixtures/background-launch/Probe.m'), '-o', $probe))
$app = Join-Path $root 'Jiffshot.app'
$macos = Join-Path $app 'Contents/MacOS'
$frameworks = Join-Path $app 'Contents/Frameworks'
[void][IO.Directory]::CreateDirectory($macos)
[void][IO.Directory]::CreateDirectory($frameworks)
foreach ($name in 'jiffshot', 'jiffshot-cli') {
    [IO.File]::Copy((Join-Path $BinaryDirectory $name), (Join-Path $macos $name))
}
if ($HostDirectory) {
    foreach ($name in 'librustolonia_host.dylib', 'libAvaloniaNative.dylib', 'libSkiaSharp.dylib', 'libHarfBuzzSharp.dylib') {
        $path = Join-Path $frameworks $name
        [IO.File]::Copy((Join-Path $HostDirectory $name), $path)
        [void](Invoke-MacCommand -File '/usr/bin/codesign' -Arguments @('--force', '--sign', '-', $path))
    }
}
$plist = & (Get-Module JiffshotMacPackage) { Get-MacInfoPlist (Get-MacWorkspaceVersion) }
$plist.CFBundleIdentifier = 'com.devolutions.jiffshot.background-test'
Write-MacPlist (Join-Path $app 'Contents/Info.plist') $plist
[void](Invoke-MacCommand -File '/usr/bin/codesign' -Arguments @('--force', '--sign', '-',
    (Join-Path $macos 'jiffshot-cli')))
[void](Invoke-MacCommand -File '/usr/bin/codesign' -Arguments @('--force', '--sign', '-',
    '--entitlements', (Join-Path $PSScriptRoot 'entitlements.plist'), $app))
[void](Invoke-MacCommand -File '/usr/bin/codesign' -Arguments @('--verify', '--deep', '--strict', $app))
$environment = New-FixtureEnvironment -Root $root -HostPath (Join-Path $frameworks 'librustolonia_host.dylib') `
    -Mode 'JIFFSHOT_UITEST_CONTROL'
$environment.Remove('RUSTOLONIA_HOST_LIB')
$environment['JIFFSHOT_UITEST_KEEP'] = '1'
$environment['JIFFSHOT_EXE'] = Join-Path $macos 'jiffshot'
$result = Invoke-MacCommand -File $probe `
    -Arguments @((Join-Path $macos 'jiffshot'), (Join-Path $macos 'jiffshot-cli'), $root) `
    -Environment $environment -TimeoutSeconds 150 -AllowFailure
[Console]::Out.Write($result.Stdout)
[Console]::Error.Write($result.Stderr)
if ($ExpectedFailure) {
    if ($result.ExitCode -ne 1 -or $result.Stderr -notmatch 'activation|Foreground application changed|Background') {
        throw 'The baseline did not reproduce a background presentation failure'
    }
    Write-Host 'PASS: the baseline reproduced a background presentation regression'
}
elseif ($result.ExitCode -ne 0) {
    throw "Background regression failed with exit $($result.ExitCode); inspect $root"
}
Write-Host "Fixture output: $root"
