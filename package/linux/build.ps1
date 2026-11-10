#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Package a Linux release build with its selected Rustolonia host configuration.

.PARAMETER BuildDir
Directory holding jiffshot and its runtime files (defaults to target/release).

.PARAMETER OutputDir
Directory that receives the packages and SHA256SUMS-linux-<arch>.txt (defaults to package/out/linux).

.PARAMETER Version
Expected version; must match Cargo.toml (defaults to JIFFSHOT_VERSION).

.PARAMETER Format
Package formats: tar.gz, deb, rpm. Accepts repeated or comma-separated values.

.PARAMETER Arch
Assert the ELF architecture of every input (x64 or arm64); never relabel or cross-compile.

.PARAMETER StaticHost
Package an executable linked with Rustolonia's static host, without companion host libraries.
#>
[CmdletBinding()]
param(
    [string]$BuildDir,
    [string]$OutputDir,
    [string]$Version,
    [string[]]$Format = @('tar.gz', 'deb'),
    [ValidateSet('x64', 'arm64', IgnoreCase = $false)]
    [string]$Arch,
    [switch]$StaticHost
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

Import-Module (Join-Path $PSScriptRoot 'JiffshotLinuxPackage.psm1') -Force

$arguments = @{
    # pwsh -File passes "tar.gz,deb" as one string, so split list syntax here.
    Format = [string[]]@($Format | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
}
foreach ($name in 'BuildDir', 'OutputDir', 'Version', 'Arch', 'StaticHost') {
    $value = Get-Variable -Name $name -ValueOnly
    if ($value) { $arguments[$name] = $value }
}
try {
    Invoke-JiffshotLinuxPackageBuild @arguments
}
catch {
    [Console]::Error.WriteLine("Linux packaging failed: $($_.Exception.Message)")
    $data = $_.Exception.Data
    if ($data.Contains('Stdout') -or $data.Contains('Stderr')) {
        [Console]::Error.WriteLine("$($data['Stdout']) $($data['Stderr'])")
    }
    exit 1
}
