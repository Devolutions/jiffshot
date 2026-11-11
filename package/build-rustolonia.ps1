#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Build matching shared and static NativeAOT host archives from Jiffshot's pinned revision.
.DESCRIPTION
Source is an optional disposable checkout: this script initializes its submodules,
applies the pinned producer patches and writes build output there. Do not pass an
active development worktree. Linux static builds also require the graphics output
from Rustolonia's rust/build-linux-static-graphics.sh.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('win-x64', 'win-arm64', 'linux-x64', 'linux-arm64', 'osx-x64', 'osx-arm64')]
    [string]$Rid,
    [string]$Source,
    [string]$OutputDirectory,
    [ValidateSet('Shared', 'Static')][string[]]$Linkage = @('Shared', 'Static')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
Import-Module (Join-Path $PSScriptRoot 'RustoloniaHost.psm1') -Force

$repository = Split-Path -Parent $PSScriptRoot
$revision = Get-JiffshotRustoloniaRevision -Manifest (Join-Path $repository 'Cargo.toml')
if (-not $Source) { $Source = Join-Path $repository 'target/rustolonia-source' }
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $repository "target/rustolonia-host/$Rid" }
$Source = [IO.Path]::GetFullPath($Source)
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
if (-not (Test-Path -LiteralPath $Source)) {
    git init --quiet $Source
    git -C $Source remote add origin https://github.com/Devolutions/rustolonia
    git -C $Source fetch --quiet --depth 1 origin $revision
    git -C $Source checkout --quiet --detach $revision
}
$head = git -C $Source rev-parse HEAD
if ($head -cne $revision) { throw "Rustolonia source must be at $revision; found $head" }
if (git -C $Source status --porcelain --untracked-files=no --ignore-submodules) {
    throw 'Rustolonia source has tracked changes; use a clean disposable checkout'
}
git -C $Source submodule update --init --recursive
$producer = Join-Path $Source 'avalonia-src'
& (Join-Path $Source 'avalonia-patches/apply-avalonia-patches.ps1') -AvaloniaRoot $producer
& (Join-Path $Source 'avalonia-patches/apply-avalonia-patches.ps1') -AvaloniaRoot $producer -Check
. (Join-Path $Source 'rust/package-shared.ps1')
Assert-ConsumerSource -ProducerRoot $producer -RustoloniaRoot $Source
dotnet build (Join-Path $producer 'src/Avalonia.Build.Tasks/Avalonia.Build.Tasks.csproj') -c Release

$previousDeploymentTarget = $env:MACOSX_DEPLOYMENT_TARGET
try {
    if ($IsMacOS) { $env:MACOSX_DEPLOYMENT_TARGET = '13.0' }
    foreach ($kind in $Linkage) {
        & (Join-Path $Source 'rust/package-host.ps1') -Rid $Rid -Linkage $kind -OutputRoot $OutputDirectory
    }
}
finally {
    $env:MACOSX_DEPLOYMENT_TARGET = $previousDeploymentTarget
}
