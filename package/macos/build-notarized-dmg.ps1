#!/usr/bin/env pwsh
#Requires -Version 7.4
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$App,
    [Parameter(Mandatory)][string]$Output,
    [Parameter(Mandatory)][ValidateSet('arm64', 'x86_64')][string]$Arch,
    [Parameter(Mandatory)][string]$Identity,
    [string]$Keychain
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacPackage.psm1') -Force

try {
    Invoke-MacNotarizedDiskImage -App $App -Output $Output -Arch $Arch -Identity $Identity -Keychain $Keychain
}
catch {
    [Console]::Error.WriteLine("macOS disk image packaging failed: $($_.Exception.Message)")
    exit 1
}
