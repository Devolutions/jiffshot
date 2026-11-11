#!/usr/bin/env pwsh
#Requires -Version 7.4
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'MsiPlatform.ps1')

function Assert-Equal($Actual, $Expected) {
    if ($Actual -cne $Expected) {
        throw "Expected '$Expected', got '$Actual'."
    }
}

function Assert-Throws([scriptblock]$Action, [string]$ExpectedMessage) {
    $failure = $null
    try {
        & $Action | Out-Null
    } catch {
        $failure = $_.Exception.Message
    }
    if ($null -eq $failure -or -not $failure.Contains($ExpectedMessage)) {
        throw "Expected an error containing '$ExpectedMessage', got '$failure'."
    }
}

$x64 = Get-MsiPlatformExpectation -Platform x64
Assert-Equal $x64.TemplatePrefix 'x64;'
Assert-Equal $x64.Condition 'Installed OR NATIVE_ARCHITECTURE ~= "AMD64"'
Assert-Equal $x64.Message 'This installer requires Windows x64. Download the ARM64 MSI for a Windows on ARM device.'

$arm64 = Get-MsiPlatformExpectation -Platform arm64
Assert-Equal $arm64.TemplatePrefix 'Arm64;'
Assert-Equal $arm64.Condition 'Installed OR NATIVE_ARCHITECTURE ~= "ARM64"'
Assert-Equal $arm64.Message 'This installer requires Windows on ARM64. Download the x64 MSI for an Intel or AMD PC.'

$program = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'Program.cs'))
if ($program -cmatch '"(?:NOT )?Arm64"') {
    throw 'package/Program.cs must not reference nonexistent Windows Installer property Arm64.'
}
foreach ($snippet in @(
    'private const string NativeArchitectureProperty = "NATIVE_ARCHITECTURE";',
    'new XAttribute("Secure", "yes")',
    'new XAttribute("Root", "HKLM")',
    'new XAttribute("Key", @"SYSTEM\CurrentControlSet\Control\Session Manager\Environment")',
    'new XAttribute("Name", "PROCESSOR_ARCHITECTURE")',
    'new XAttribute("Type", "raw")',
    'new XAttribute("Bitness", "always64")',
    '$"Installed OR {NativeArchitectureProperty} ~= \"ARM64\""',
    '$"Installed OR {NativeArchitectureProperty} ~= \"AMD64\""'
)) {
    if (-not $program.Contains($snippet, [StringComparison]::Ordinal)) {
        throw "package/Program.cs is missing expected MSI platform check authoring: $snippet"
    }
}

$workflow = [IO.File]::ReadAllText((Join-Path $PSScriptRoot '../.github/workflows/ci.yml'))
$msiStart = $workflow.IndexOf("`n  msi:")
$msixStart = $workflow.IndexOf("`n  msix:")
if ($msiStart -lt 0 -or $msixStart -le $msiStart) {
    throw 'Could not locate the msi job in .github/workflows/ci.yml.'
}
$msiJob = $workflow.Substring($msiStart, $msixStart - $msiStart)
$unitStep = [regex]::Match($msiJob, '(?ms)^      - name: Test MSI platform checks\r?\n.*?(?=^      - |\z)')
$buildStep = [regex]::Match($msiJob, '(?ms)^      - name: Build the MSI\r?\n.*?(?=^      - |\z)')
$mismatchStep = [regex]::Match($msiJob, '(?ms)^      - name: Test MSI platform mismatch rejection\r?\n.*?(?=^      - |\z)')
$signStep = [regex]::Match($msiJob, '(?ms)^      - name: Sign MSI\r?\n.*?(?=^      - |\z)')
if (-not $unitStep.Success -or -not $buildStep.Success -or -not $mismatchStep.Success -or -not $signStep.Success -or
    $unitStep.Index -ge $buildStep.Index -or $buildStep.Index -ge $mismatchStep.Index -or $mismatchStep.Index -ge $signStep.Index -or
    $unitStep.Value -notmatch 'test-msi-platform\.ps1' -or
    $buildStep.Value -notmatch 'Confirm-MsiPlatformCheck' -or
    $mismatchStep.Value -notmatch 'Set-MsiPlatformConditionForTest') {
    throw 'The CI msi job must run test-msi-platform.ps1, Confirm-MsiPlatformCheck, and the msiexec mismatch check before signing.'
}

if ($IsWindows) {
    $tempBase = if (-not [string]::IsNullOrWhiteSpace($env:RUNNER_TEMP) -and (Test-Path -LiteralPath $env:RUNNER_TEMP)) {
        $env:RUNNER_TEMP
    } elseif (-not [string]::IsNullOrWhiteSpace($env:LOCALAPPDATA) -and (Test-Path -LiteralPath (Join-Path $env:LOCALAPPDATA 'Temp'))) {
        Join-Path $env:LOCALAPPDATA 'Temp'
    } else {
        [IO.Path]::GetTempPath()
    }
    $work = Join-Path $tempBase ("jiffshot-msi-platform-test-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $work | Out-Null
    try {
        function Invoke-MsiMutation([string]$Path, [int]$OpenMode, [scriptblock]$Body) {
            $db = [JiffshotMsiDatabase]::new($Path, $OpenMode)
            try {
                & $Body $db
                $db.Commit()
            } finally {
                $db.Dispose()
            }
        }

        function New-SyntheticMsi([string]$Path, [string]$Template = 'Arm64;1033', [int]$PageCount = 500) {
            Invoke-MsiMutation -Path $Path -OpenMode ([JiffshotMsiDatabase]::OpenCreate) -Body {
                param($db)
                $db.WriteSummary($Template, $PageCount)
                foreach ($ddl in @(
                    'CREATE TABLE `Property` (`Property` CHAR(72) NOT NULL, `Value` LONGCHAR NOT NULL PRIMARY KEY `Property`)',
                    'CREATE TABLE `LaunchCondition` (`Condition` CHAR(255) NOT NULL, `Description` LONGCHAR NOT NULL LOCALIZABLE PRIMARY KEY `Condition`)',
                    'CREATE TABLE `AppSearch` (`Property` CHAR(72) NOT NULL, `Signature_` CHAR(72) NOT NULL PRIMARY KEY `Property`, `Signature_`)',
                    'CREATE TABLE `Signature` (`Signature` CHAR(72) NOT NULL, `FileName` CHAR(255) NOT NULL PRIMARY KEY `Signature`)',
                    'CREATE TABLE `CompLocator` (`Signature_` CHAR(72) NOT NULL, `ComponentId` CHAR(38) NOT NULL, `Type` SHORT PRIMARY KEY `Signature_`)',
                    'CREATE TABLE `RegLocator` (`Signature_` CHAR(72) NOT NULL, `Root` SHORT NOT NULL, `Key` CHAR(255) NOT NULL, `Name` CHAR(255), `Type` SHORT PRIMARY KEY `Signature_`)',
                    'CREATE TABLE `InstallUISequence` (`Action` CHAR(72) NOT NULL, `Condition` CHAR(255), `Sequence` SHORT PRIMARY KEY `Action`)',
                    'CREATE TABLE `InstallExecuteSequence` (`Action` CHAR(72) NOT NULL, `Condition` CHAR(255), `Sequence` SHORT PRIMARY KEY `Action`)'
                )) {
                    $db.Execute($ddl)
                }
                foreach ($sql in @(
                    "INSERT INTO ``Property`` (``Property``, ``Value``) VALUES ('SecureCustomProperties', 'WIX_DOWNGRADE_DETECTED;NATIVE_ARCHITECTURE')",
                    "INSERT INTO ``LaunchCondition`` (``Condition``, ``Description``) VALUES ('Installed OR NATIVE_ARCHITECTURE ~= ""ARM64""', 'This installer requires Windows on ARM64. Download the x64 MSI for an Intel or AMD PC.')",
                    "INSERT INTO ``AppSearch`` (``Property``, ``Signature_``) VALUES ('NATIVE_ARCHITECTURE', 'NativeArchitecture')",
                    "INSERT INTO ``RegLocator`` (``Signature_``, ``Root``, ``Key``, ``Name``, ``Type``) VALUES ('NativeArchitecture', 2, 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'PROCESSOR_ARCHITECTURE', 18)",
                    "INSERT INTO ``InstallUISequence`` (``Action``, ``Condition``, ``Sequence``) VALUES ('AppSearch', '', 50)",
                    "INSERT INTO ``InstallUISequence`` (``Action``, ``Condition``, ``Sequence``) VALUES ('LaunchConditions', '', 100)",
                    "INSERT INTO ``InstallExecuteSequence`` (``Action``, ``Condition``, ``Sequence``) VALUES ('AppSearch', '', 50)",
                    "INSERT INTO ``InstallExecuteSequence`` (``Action``, ``Condition``, ``Sequence``) VALUES ('LaunchConditions', '', 100)"
                )) {
                    $db.Execute($sql)
                }
            }
        }

        $validArm64 = Join-Path $work 'valid-arm64.msi'
        New-SyntheticMsi -Path $validArm64
        Confirm-MsiPlatformCheck -MsiPath $validArm64 -Platform arm64
        Assert-Throws { Confirm-MsiPlatformCheck -MsiPath $validArm64 -Platform x64 } 'SummaryInformation Template'

        $convertedX64 = Join-Path $work 'converted-x64.msi'
        $validX64 = Join-Path $work 'valid-x64.msi'
        New-SyntheticMsi -Path $validX64 -Template 'x64;1033'
        Set-MsiPlatformConditionForTest -SourceMsiPath $validX64 -DestinationMsiPath $convertedX64 -TargetPlatform x64
        Confirm-MsiPlatformCheck -MsiPath $convertedX64 -Platform x64

        # Regression: legacy nonexistent 'Arm64' property must be rejected.
        $legacyBroken = Join-Path $work 'legacy-broken.msi'
        Copy-Item -LiteralPath $validArm64 -Destination $legacyBroken
        Invoke-MsiMutation -Path $legacyBroken -OpenMode ([JiffshotMsiDatabase]::OpenTransact) -Body {
            param($db)
            $db.Execute("DELETE FROM ``LaunchCondition``")
            $db.Execute("INSERT INTO ``LaunchCondition`` (``Condition``, ``Description``) VALUES ('Arm64', 'Legacy broken condition')")
        }
        Assert-Throws { Confirm-MsiPlatformCheck -MsiPath $legacyBroken -Platform arm64 } "nonexistent Windows Installer property 'Arm64'"

        # Regression: missing SecureCustomProperties entry breaks UAC UI-to-server handoff.
        $unsecured = Join-Path $work 'unsecured.msi'
        Copy-Item -LiteralPath $validArm64 -Destination $unsecured
        Invoke-MsiMutation -Path $unsecured -OpenMode ([JiffshotMsiDatabase]::OpenTransact) -Body {
            param($db)
            $db.Execute("UPDATE ``Property`` SET ``Value`` = 'WIX_DOWNGRADE_DETECTED' WHERE ``Property`` = 'SecureCustomProperties'")
        }
        Assert-Throws { Confirm-MsiPlatformCheck -MsiPath $unsecured -Platform arm64 } 'SecureCustomProperties'

        # Regression: AppSearch sequenced after LaunchConditions leaves NATIVE_ARCHITECTURE unset.
        $badOrder = Join-Path $work 'bad-order.msi'
        Copy-Item -LiteralPath $validArm64 -Destination $badOrder
        Invoke-MsiMutation -Path $badOrder -OpenMode ([JiffshotMsiDatabase]::OpenTransact) -Body {
            param($db)
            $db.Execute("UPDATE ``InstallExecuteSequence`` SET ``Sequence`` = 200 WHERE ``Action`` = 'AppSearch'")
        }
        Assert-Throws { Confirm-MsiPlatformCheck -MsiPath $badOrder -Platform arm64 } 'schedules AppSearch'
    } finally {
        if (Test-Path -LiteralPath $work) {
            Remove-Item -LiteralPath $work -Recurse -Force
        }
    }
}

Write-Output 'MSI platform check tests passed.'
