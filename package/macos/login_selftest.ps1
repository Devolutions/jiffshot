#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Exercise SMAppService with a unique, temporary signed test bundle.
#>
[CmdletBinding()]
param([Parameter(Mandatory)][string]$TestBinary)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacPackage.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacFixture.psm1') -Force

function Invoke-LoginTest([string]$BinaryPath, [string]$Name, [System.Collections.IDictionary]$Environment) {
    $test = "tray::startup::tests::native_login_item_${Name}_in_an_isolated_bundle"
    $result = Invoke-MacCommand -File $BinaryPath `
        -Arguments @('--exact', $test, '--ignored', '--nocapture', '--test-threads=1') `
        -Environment $Environment -TimeoutSeconds 60 -AllowFailure
    [Console]::Out.Write($result.Stdout)
    [Console]::Error.Write($result.Stderr)
    if ($result.ExitCode -ne 0 -or -not $result.Stdout.Contains('test result: ok. 1 passed;')) {
        throw "Native login-item $Name did not pass"
    }
}

$binaryPath = Resolve-FixtureFile $TestBinary
$identifier = 'com.devolutions.jiffshot.login-selftest.' + [guid]::NewGuid().ToString('N')
$root = New-FixtureRoot '' 'jiffshot-login-selftest-'
$app = Join-Path $root 'LoginFixture.app'
$contents = Join-Path $app 'Contents'
$macos = Join-Path $contents 'MacOS'
$test = Join-Path $macos 'login-item-tests'
$environment = @{}
foreach ($entry in [Environment]::GetEnvironmentVariables().GetEnumerator()) {
    $environment[[string]$entry.Key] = [string]$entry.Value
}
$environment['JIFFSHOT_LOGIN_SELFTEST_BUNDLE_ID'] = $identifier
$signed = $false
try {
    [void][IO.Directory]::CreateDirectory($macos)
    [IO.File]::Copy($binaryPath, $test)
    Write-MacPlist (Join-Path $contents 'Info.plist') ([ordered]@{
        CFBundleIdentifier = $identifier
        CFBundleName = 'Jiffshot Login Fixture'
        CFBundleExecutable = 'login-item-tests'
        CFBundlePackageType = 'APPL'
        CFBundleVersion = '1'
        LSUIElement = $true
    })
    [void](Invoke-MacCommand -File '/usr/bin/codesign' -Arguments @('--force', '--sign', '-', $app))
    [void](Invoke-MacCommand -File '/usr/bin/codesign' -Arguments @('--verify', '--strict', $app))
    $signed = $true
    Invoke-LoginTest $test 'round_trip' $environment
}
finally {
    if ($signed) {
        try { Invoke-LoginTest $test 'cleanup' $environment }
        catch {
            [Console]::Error.WriteLine("Could not verify removal of login item ${identifier}: $($_.Exception.Message). Test bundle retained at $app.")
            throw
        }
    }
    Remove-FixtureRoot $root
}
