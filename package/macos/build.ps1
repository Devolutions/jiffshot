#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Package a macOS build as a signed app bundle and disk image.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$Output,
    [Parameter(Mandatory)][ValidateSet('arm64', 'x86_64')][string]$Arch,
    [string]$Identity = '-',
    [string]$Keychain,
    [string]$NotaryProfile,
    [switch]$RequireNativePopupGate,
    [switch]$RequireNativeTermination,
    [switch]$RequireNativeFileDrag,
    [switch]$AllowUncheckedHost
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacPackage.psm1') -Force

if ($AllowUncheckedHost -and ($RequireNativePopupGate -or $RequireNativeTermination -or $RequireNativeFileDrag)) {
    throw '-AllowUncheckedHost cannot be combined with capability requirements'
}
$checked = -not $AllowUncheckedHost
try {
    Invoke-MacPackage -Binary $Binary -Output $Output -Arch $Arch -Identity $Identity `
        -Keychain $Keychain -NotaryProfile $NotaryProfile `
        -RequirePopupGate $checked -RequireTermination $checked -RequireFileDrag $checked
}
catch {
    [Console]::Error.WriteLine("macOS packaging failed: $($_.Exception.Message)")
    exit 1
}
