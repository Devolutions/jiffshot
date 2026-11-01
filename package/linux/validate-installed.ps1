#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Assert the architecture of every installed private runtime ELF before smoke testing.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('x64', 'arm64', IgnoreCase = $false)]
    [string]$Arch,
    [string]$RuntimeDir = '/usr/lib/jiffshot'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

Import-Module (Join-Path $PSScriptRoot 'JiffshotLinuxPackage.psm1') -Force

"Installed runtime: Linux $((Confirm-JiffshotBuild -BuildDir $RuntimeDir -Arch $Arch).Name)"
