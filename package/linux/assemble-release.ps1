#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Require and verify all six Linux packages before adding them to a release.

.DESCRIPTION
Reads jiffshot-linux-<arch> (tar.gz and DEB) and jiffshot-linux-<arch>-rpm artifact
directories for x64 and arm64, verifies every package against its builder manifest,
then copies the packages and merged SHA256SUMS-linux-<arch>.txt files to OutputDir.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$ArtifactDir,
    [Parameter(Mandatory)][string]$OutputDir,
    [Parameter(Mandatory)][string]$Version
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

Import-Module (Join-Path $PSScriptRoot 'JiffshotLinuxPackage.psm1') -Force

Publish-JiffshotLinuxRelease -ArtifactDir $ArtifactDir -OutputDir $OutputDir -Version $Version
