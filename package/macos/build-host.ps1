#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Build the patched Rustolonia host and its matching native companions from source.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Source,
    [Parameter(Mandatory)][ValidateSet('arm64', 'x86_64')][string]$Arch
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Invoke-HostBuild {
    param([string]$File, [string[]]$Arguments)
    & $File @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$File failed with exit code $LASTEXITCODE"
    }
}

$sourcePath = (Resolve-Path -LiteralPath $Source).Path
$rid = if ($Arch -eq 'arm64') { 'osx-arm64' } else { 'osx-x64' }
$artifacts = Join-Path $sourcePath "rust/target/ci-host-$rid"
$hostDirectory = Join-Path $artifacts "publish/Avalonia.Host/release_$rid"
$previousDeploymentTarget = $env:MACOSX_DEPLOYMENT_TARGET
$env:MACOSX_DEPLOYMENT_TARGET = '13.0'
Push-Location $sourcePath
try {
    Invoke-HostBuild pwsh @('-NoLogo', '-NoProfile', '-File', './avalonia-patches/apply-avalonia-patches.ps1',
        '-AvaloniaRoot', './avalonia-src')
    Invoke-HostBuild pwsh @('-NoLogo', '-NoProfile', '-File', './avalonia-patches/apply-avalonia-patches.ps1',
        '-AvaloniaRoot', './avalonia-src', '-Check')
    Invoke-HostBuild dotnet @('build', 'avalonia-src/src/Avalonia.Build.Tasks/Avalonia.Build.Tasks.csproj',
        '-c', 'Release')
    Invoke-HostBuild dotnet @('build', 'avalonia-src/native/Avalonia.Native/Avalonia.Native.macOS.proj',
        '-t:GenerateMicroComItems', '-p:BuildAvaloniaNativeXcodeProject=false')
    # Publish must retain this patched universal companion for either target RID.
    Invoke-HostBuild xcodebuild @('-project',
        'avalonia-src/native/Avalonia.Native/src/OSX/Avalonia.Native.OSX.xcodeproj',
        '-configuration', 'Release', 'ARCHS=arm64 x86_64', 'ONLY_ACTIVE_ARCH=NO',
        'MACOSX_DEPLOYMENT_TARGET=13.0',
        "CONFIGURATION_BUILD_DIR=$sourcePath/avalonia-src/Build/Products/Release", '-quiet')
    Invoke-HostBuild dotnet @('publish', 'host/Avalonia.Host.csproj', '-c', 'Release', '-r', $rid,
        '-p:AvaloniaRustHostPlatform=OSX', '-p:BuildAvaloniaNativeXcodeProject=false',
        '--artifacts-path', $artifacts)

    $libraries = @('librustolonia_host.dylib', 'libAvaloniaNative.dylib', 'libSkiaSharp.dylib',
        'libHarfBuzzSharp.dylib')
    $checksums = foreach ($library in $libraries) {
        $path = Join-Path $hostDirectory $library
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "Published host is missing $path"
        }
        "$((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant())  $library"
    }
    $checksums | Set-Content -LiteralPath (Join-Path $hostDirectory 'SHA256SUMS.txt') -Encoding utf8NoBOM
    Write-Host "Built $rid host in $hostDirectory"
    if ($env:GITHUB_ENV) {
        @(
            "RUSTOLONIA_HOST_DIR=$hostDirectory"
            "RUSTOLONIA_HOST_LIB=$(Join-Path $hostDirectory 'librustolonia_host.dylib')"
        ) | Out-File -LiteralPath $env:GITHUB_ENV -Encoding utf8 -Append
    }
}
finally {
    Pop-Location
    $env:MACOSX_DEPLOYMENT_TARGET = $previousDeploymentTarget
}
