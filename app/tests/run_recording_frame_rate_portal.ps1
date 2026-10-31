#!/usr/bin/env pwsh
# Discover and run the fixed-rate tests inside a real portal fixture.

param(
    [Parameter(Mandatory)][string]$Binary
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Expected = @(
    'portal_fixed_rate_gui_restart_and_command_line_use_saved_rate'
    'portal_fixed_rate_pause_preserves_video_webcam_and_virtual_audio'
    'portal_fixed_rate_restart_keeps_original_choice_and_discards_old_segment'
)

function Invoke-Logged {
    param([string[]]$Command)
    Write-Host "+ $($Command -join ' ')"
    $arguments = @($Command | Select-Object -Skip 1)
    $lines = & $Command[0] @arguments 2>&1 | ForEach-Object { "$_" }
    $exitCode = $LASTEXITCODE
    $output = if ($lines) { ($lines -join "`n") + "`n" } else { '' }
    [Console]::Out.Write($output)
    [Console]::Out.Flush()
    [pscustomobject]@{ ExitCode = $exitCode; Output = $output }
}

$binaryPath = (Resolve-Path -LiteralPath $Binary -ErrorAction Stop).ProviderPath
$listing = Invoke-Logged @($binaryPath, 'portal_fixed_rate_', '--list')
if ($listing.ExitCode) {
    exit $listing.ExitCode
}
$actual = @([regex]::Matches($listing.Output, '(?m)^([a-zA-Z0-9_:]+): test$') |
    ForEach-Object { ($_.Groups[1].Value -split '::')[-1] })
$unique = @($actual | Sort-Object -Unique)
$expectedSorted = @($Expected | Sort-Object)
if ($actual.Count -ne $Expected.Count -or ($unique -join "`n") -cne ($expectedSorted -join "`n")) {
    throw "unexpected portal test discovery: [$(($actual | ForEach-Object { "'$_'" }) -join ', ')]"
}
Write-Host "Verified discovery of $($actual.Count) real-portal tests."
$result = Invoke-Logged @($binaryPath, 'portal_fixed_rate_', '--ignored', '--nocapture', '--test-threads=1')
$summary = [regex]::Match(
    $result.Output,
    'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out'
)
if (-not $summary.Success) {
    throw 'portal test run has no parseable summary'
}
$passed = [int]$summary.Groups[2].Value
$failed = [int]$summary.Groups[3].Value
$ignored = [int]$summary.Groups[4].Value
if ($passed + $failed + $ignored -ne $Expected.Count -or $ignored) {
    throw "incomplete portal result: $passed passed, $failed failed, $ignored ignored"
}
if ($result.ExitCode -or $failed) {
    exit $(if ($result.ExitCode) { $result.ExitCode } else { 1 })
}
