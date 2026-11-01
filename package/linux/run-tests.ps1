#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Run the Linux packaging Pester tests.

.DESCRIPTION
Package content tests read built packages from JIFFSHOT_PACKAGE_DIR (defaults to
package/out/linux) for the formats in JIFFSHOT_TEST_FORMATS (defaults to
tar.gz,deb,rpm) and JIFFSHOT_TEST_ARCH (defaults to the host architecture).
Use -ExcludeTag PackageContent to run only the builder regression tests.
#>
[CmdletBinding()]
param(
    [string[]]$Tag,
    [string[]]$ExcludeTag
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

Import-Module Pester -MinimumVersion 5.5.0 -MaximumVersion 5.99.99
$configuration = New-PesterConfiguration
$configuration.Run.Path = $PSScriptRoot
$configuration.Run.Exit = $true
$configuration.Output.Verbosity = 'Detailed'
if ($Tag) { $configuration.Filter.Tag = $Tag }
if ($ExcludeTag) { $configuration.Filter.ExcludeTag = $ExcludeTag }
Invoke-Pester -Configuration $configuration
