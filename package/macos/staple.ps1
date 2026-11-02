#!/usr/bin/env pwsh
#Requires -Version 7.4
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Path,
    [switch]$Validate
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacPackage.psm1') -Force

$action = if ($Validate) { 'validate' } else { 'staple' }
try {
    Invoke-MacStapler -Action $action -Path $Path
}
catch {
    $operation = if ($Validate) { 'validation' } else { 'stapling' }
    [Console]::Error.WriteLine("macOS ticket ${operation} failed: $($_.Exception.Message)")
    exit 1
}
