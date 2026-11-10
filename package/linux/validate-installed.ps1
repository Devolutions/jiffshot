#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Assert the architecture of every installed private runtime ELF after installation.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('x64', 'arm64', IgnoreCase = $false)]
    [string]$Arch,
    [string]$RuntimeDir = '/usr/lib/jiffshot',
    [switch]$StaticHost
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

Import-Module (Join-Path $PSScriptRoot 'JiffshotLinuxPackage.psm1') -Force

$runtimeFiles = if ($StaticHost) { @('jiffshot') } else { (Get-JiffshotPackageInfo).RuntimeFiles }
"Installed runtime: Linux $((Confirm-JiffshotBuild -BuildDir $RuntimeDir -Arch $Arch -RuntimeFiles $runtimeFiles).Name)"
