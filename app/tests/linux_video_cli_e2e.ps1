#!/usr/bin/env pwsh
#Requires -Version 7.4

param([switch]$DebugBuild)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force
$root = New-E2EWorkDirectory 'jiffshot-video-cli-'
try {
    $env:JIFFSHOT_UITEST = '1'
    $env:JIFFSHOT_TEST_ROOT = $root
    $env:XDG_RUNTIME_DIR = New-E2EPrivateDirectory (Join-Path $root 'runtime')
    $env:JIFFSHOT_LIBRARY = New-E2EPrivateDirectory (Join-Path $root 'library')
    $env:XDG_CONFIG_HOME = New-E2EPrivateDirectory (Join-Path $root 'config')
    $env:JIFFSHOT_LOCAL_DIR = New-E2EPrivateDirectory (Join-Path $root 'local')
    $settings = New-E2EPrivateDirectory (Join-Path $env:XDG_CONFIG_HOME 'Jiffshot')
    @{ capture_directory = $env:JIFFSHOT_LIBRARY } |
        ConvertTo-Json -Compress | Set-Content -LiteralPath (Join-Path $settings 'settings.json') -Encoding utf8
    $profile = if ($DebugBuild) { @() } else { @('--release') }
    $profileName = if ($DebugBuild) { 'debug' } else { 'release' }
    & cargo build @profile --locked -p jiffshot-cli
    if ($LASTEXITCODE -ne 0) { throw 'could not build jiffshot-cli' }
    $target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path (Get-Location) 'target' }
    $env:JIFFSHOT_CLI_E2E = Join-Path (Join-Path $target $profileName) 'jiffshot-cli'
    & cargo test @profile --locked -p jiffshot linux_video_cli_edits_and_verifies -- --ignored --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw 'Linux video CLI test failed' }
}
finally {
    Remove-E2EWorkDirectory $root
}
