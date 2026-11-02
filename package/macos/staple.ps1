#!/usr/bin/env pwsh
#Requires -Version 7.4
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Path
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacPackage.psm1') -Force

try {
    Invoke-MacStaple -Path $Path
}
catch {
    [Console]::Error.WriteLine("macOS ticket stapling failed: $($_.Exception.Message)")
    exit 1
}
