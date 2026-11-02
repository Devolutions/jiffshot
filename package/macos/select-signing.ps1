#!/usr/bin/env pwsh
#Requires -Version 7.4
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Test-Secret([string]$Name) {
    return -not [string]::IsNullOrEmpty([Environment]::GetEnvironmentVariable($Name))
}

$required = [Environment]::GetEnvironmentVariable('REQUIRE_MACOS_NOTARIZATION')
if ($required -cnotin @('true', 'false')) { throw 'REQUIRE_MACOS_NOTARIZATION must be true or false' }
$output = [Environment]::GetEnvironmentVariable('GITHUB_OUTPUT')
if ([string]::IsNullOrEmpty($output)) { throw 'GITHUB_OUTPUT is required' }

$uniCertificate = Test-Secret 'APPLE_APP_DEV_ID_APP_CERTIFICATE'
$uniPassword = Test-Secret 'APPLE_APP_DEV_ID_APP_CERTIFICATE_PASSWORD'
$uniNotary = Test-Secret 'APPLE_BOT_PASSWORD'
$legacyCertificate = Test-Secret 'APPLE_CERTIFICATE_BASE64'
$legacyPassword = Test-Secret 'APPLE_CERTIFICATE_PASSWORD'
$legacyIdentity = Test-Secret 'APPLE_DEVELOPER_ID'
$legacyAppleId = Test-Secret 'APPLE_ID'
$legacyTeamId = Test-Secret 'APPLE_TEAM_ID'
$legacyNotary = Test-Secret 'APPLE_APP_PASSWORD'

$kind = 'none'
$enabled = $false
$notarize = $false
if ($uniCertificate -or $uniPassword -or $uniNotary) {
    if (-not ($uniCertificate -and $uniPassword)) {
        throw 'Apple signing requires APPLE_APP_DEV_ID_APP_CERTIFICATE and APPLE_APP_DEV_ID_APP_CERTIFICATE_PASSWORD.'
    }
    if (-not (Test-Secret 'DEVOLUTIONSBOT_TOKEN')) {
        throw 'Apple signing requires DEVOLUTIONSBOT_TOKEN with read access to Devolutions/actions.'
    }
    $kind = 'unigetui'
    $enabled = $true
    $notarize = $uniNotary
}
elseif ($legacyCertificate -or $legacyPassword -or $legacyIdentity -or $legacyAppleId -or $legacyTeamId -or $legacyNotary) {
    if (-not ($legacyCertificate -and $legacyPassword -and $legacyIdentity)) {
        throw 'Legacy Apple signing requires APPLE_CERTIFICATE_BASE64, APPLE_CERTIFICATE_PASSWORD, and APPLE_DEVELOPER_ID.'
    }
    if (($legacyAppleId -or $legacyTeamId -or $legacyNotary) -and -not ($legacyAppleId -and $legacyTeamId -and $legacyNotary)) {
        throw 'Legacy notarization requires APPLE_ID, APPLE_TEAM_ID, and APPLE_APP_PASSWORD.'
    }
    $kind = 'legacy'
    $enabled = $true
    $notarize = $legacyNotary
}

if ($required -ceq 'true' -and -not $notarize) {
    throw 'Published releases require Apple signing and notarization credentials.'
}

$values = @(
    "kind=$kind"
    "enabled=$($enabled.ToString().ToLowerInvariant())"
    "notarize=$($notarize.ToString().ToLowerInvariant())"
)
[IO.File]::AppendAllText($output, (($values -join "`n") + "`n"), [Text.UTF8Encoding]::new($false))
