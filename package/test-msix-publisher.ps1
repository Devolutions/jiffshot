#!/usr/bin/env pwsh
#Requires -Version 7.4
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'MsixPublisher.ps1')

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

$subject = "CN=Example (TEST ONLY), O=Example, S=Qu$([char]0xE9)bec"
$valid = [pscustomobject]@{
    Status = [System.Management.Automation.SignatureStatus]::Valid
    StatusMessage = 'Valid'
    SignerCertificate = [pscustomobject]@{ Subject = $subject }
}
$untrusted = [pscustomobject]@{
    Status = [System.Management.Automation.SignatureStatus]::UnknownError
    StatusMessage = 'The certificate root is not trusted.'
    SignerCertificate = [pscustomobject]@{ Subject = $subject }
}
$unsigned = [pscustomobject]@{
    Status = [System.Management.Automation.SignatureStatus]::NotSigned
    StatusMessage = 'The file is not signed.'
    SignerCertificate = $null
}
$missingSigner = [pscustomobject]@{
    Status = [System.Management.Automation.SignatureStatus]::Valid
    StatusMessage = 'Valid'
    SignerCertificate = $null
}
$missingSubject = [pscustomobject]@{
    Status = [System.Management.Automation.SignatureStatus]::Valid
    StatusMessage = 'Valid'
    SignerCertificate = [pscustomobject]@{ Subject = ' ' }
}

foreach ($requireSigned in @($true, $false)) {
    Assert-Equal (Get-MsixPublisher -Signature $valid -RequireSigned $requireSigned) $subject
    Assert-Throws { Get-MsixPublisher -Signature $missingSigner -RequireSigned $requireSigned } 'no signer subject'
    Assert-Throws { Get-MsixPublisher -Signature $missingSubject -RequireSigned $requireSigned } 'no signer subject'
}
Assert-Equal (Get-MsixPublisher -Signature $unsigned -RequireSigned $false) 'CN=Devolutions Jiffshot CI'
Assert-Equal (Get-MsixPublisher -Signature $untrusted -RequireSigned $false) 'CN=Devolutions Jiffshot CI'
Assert-Throws { Get-MsixPublisher -Signature $unsigned -RequireSigned $true } 'The file is not signed.'
Assert-Throws { Get-MsixPublisher -Signature $untrusted -RequireSigned $true } 'The certificate root is not trusted.'

$workflow = [IO.File]::ReadAllText((Join-Path $PSScriptRoot '../.github/workflows/ci.yml'))
$jobStart = $workflow.IndexOf("`n  msix:")
if ($jobStart -lt 0) { throw 'The MSIX job was not found in CI.' }
$msixJob = $workflow.Substring($jobStart)
$login = [regex]::Match($msixJob, '(?ms)^      - name: Azure login for test MSIX publisher\r?\n.*?(?=^      - |\z)')
$trust = [regex]::Match($msixJob, '(?ms)^      - name: Trust test signing root for MSIX publisher\r?\n.*?(?=^      - |\z)')
$build = [regex]::Match($msixJob, '(?ms)^      - name: Build the MSIX\r?\n.*?(?=^      - |\z)')
$sign = [regex]::Match($msixJob, '(?ms)^      - name: Sign MSIX\r?\n.*?(?=^      - |\z)')
if (-not $login.Success -or -not $trust.Success -or -not $build.Success -or -not $sign.Success -or
    $login.Index -ge $trust.Index -or $trust.Index -ge $build.Index -or $build.Index -ge $sign.Index -or
    $login.Value -notmatch "if: inputs\.signing_environment == 'publish-test'" -or
    $login.Value -notmatch 'uses: azure/login@v3' -or
    $trust.Value -notmatch "if: inputs\.signing_environment == 'publish-test'" -or
    $trust.Value -notmatch 'uses: \./\.github/actions/trust-test-signing-root' -or
    $build.Value -notmatch 'SIGNING_REQUIRED:' -or
    $build.Value -notmatch 'Get-MsixPublisher -Signature \$signature -RequireSigned' -or
    $sign.Value -notmatch "install_test_signing_root: 'false'") {
    throw 'MSIX CI must trust the test signing root before selecting the signed publisher.'
}
Write-Output 'MSIX publisher selection tests passed.'
