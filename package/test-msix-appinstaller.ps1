#!/usr/bin/env pwsh
#Requires -Version 7.4
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'MsixAppInstaller.ps1')
Add-Type -AssemblyName System.IO.Compression

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

function New-StagedManifest([string]$Version, [string]$Architecture) {
    [xml]$manifest = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'AppxManifest.xml') -Raw
    $manifest.Package.Identity.SetAttribute('Version', $Version)
    $manifest.Package.Identity.SetAttribute('ProcessorArchitecture', $Architecture)
    $manifest.Package.Identity.SetAttribute('Publisher', 'CN=Example, O=Example')
    $manifest
}

function New-TestPackage([string]$Path, [xml]$Manifest, [byte[]]$Embedded) {
    $archive = [System.IO.Compression.ZipFile]::Open($Path, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        $entry = $archive.CreateEntry('AppxManifest.xml').Open()
        try { $Manifest.Save($entry) } finally { $entry.Dispose() }
        $entry = $archive.CreateEntry('Update.appinstaller').Open()
        try { $entry.Write($Embedded, 0, $Embedded.Length) } finally { $entry.Dispose() }
    } finally {
        $archive.Dispose()
    }
}

$repository = 'Devolutions/jiffshot'
$manifest = New-StagedManifest '1.2.3.0' 'arm64'
$bytes = ConvertTo-MsixAppInstaller -Manifest $manifest -Repository $repository
[xml]$feed = [System.Text.Encoding]::UTF8.GetString($bytes)
Assert-Equal $feed.AppInstaller.NamespaceURI 'http://schemas.microsoft.com/appx/appinstaller/2021'
Assert-Equal $feed.AppInstaller.Version '1.2.3.0'
Assert-Equal $feed.AppInstaller.Uri 'https://github.com/Devolutions/jiffshot/releases/latest/download/Devolutions.Jiffshot-windows-arm64.appinstaller'
Assert-Equal $feed.AppInstaller.MainPackage.Name 'Devolutions.Jiffshot'
Assert-Equal $feed.AppInstaller.MainPackage.Publisher 'CN=Example, O=Example'
Assert-Equal $feed.AppInstaller.MainPackage.Version '1.2.3.0'
Assert-Equal $feed.AppInstaller.MainPackage.ProcessorArchitecture 'arm64'
Assert-Equal $feed.AppInstaller.MainPackage.Uri 'https://github.com/Devolutions/jiffshot/releases/download/v1.2.3/Devolutions.Jiffshot-windows-arm64-1.2.3.msix'
Assert-Equal $feed.AppInstaller.UpdateSettings.OnLaunch.HoursBetweenUpdateChecks '0'
Assert-Equal $feed.AppInstaller.UpdateSettings.OnLaunch.ShowPrompt 'false'
Assert-Equal $feed.AppInstaller.UpdateSettings.OnLaunch.UpdateBlocksActivation 'false'
if ($null -eq $feed.AppInstaller.UpdateSettings.AutomaticBackgroundTask) { throw 'Background update checks are not enabled.' }
if ($bytes[0] -eq 0xEF) { throw 'The App Installer feed must not have a byte-order mark.' }

Assert-Throws { ConvertTo-MsixAppInstaller -Manifest $manifest -Repository 'not a repo' } 'owner/repository'
Assert-Throws { ConvertTo-MsixAppInstaller -Manifest (New-StagedManifest '1.2.3.4' 'x64') -Repository $repository } 'major.minor.build.0'
Assert-Throws { ConvertTo-MsixAppInstaller -Manifest (New-StagedManifest '1.2.3.0' 'neutral') -Repository $repository } 'x64 or arm64'
$withoutAutoUpdate = New-StagedManifest '1.2.3.0' 'x64'
$autoUpdate = $withoutAutoUpdate.Package.Properties.AutoUpdate
[void]$autoUpdate.ParentNode.RemoveChild($autoUpdate)
Assert-Throws { ConvertTo-MsixAppInstaller -Manifest $withoutAutoUpdate -Repository $repository } 'Update.appinstaller'

$scratch = Join-Path ([System.IO.Path]::GetTempPath()) "jiffshot-appinstaller-$([guid]::NewGuid())"
New-Item -ItemType Directory -Path $scratch | Out-Null
try {
    $manifest = New-StagedManifest '1.2.3.0' 'x64'
    $manifestPath = Join-Path $scratch 'AppxManifest.xml'
    $manifest.Save($manifestPath)
    $feedPath = Join-Path $scratch (Get-MsixAppInstallerName -Architecture x64)
    New-MsixAppInstaller -ManifestPath $manifestPath -Repository $repository -OutputPath $feedPath
    $feedBytes = [System.IO.File]::ReadAllBytes($feedPath)
    $packageName = Get-MsixReleasePackageName -Architecture x64 -Version '1.2.3'
    $package = Join-Path $scratch $packageName
    New-TestPackage $package $manifest $feedBytes
    Test-MsixAppInstallerPackage -PackagePath $package -AppInstallerPath $feedPath -Repository $repository

    $ciPackage = Join-Path $scratch 'Devolutions.Jiffshot-x64-1.2.3.msix'
    Copy-Item -LiteralPath $package -Destination $ciPackage
    Assert-Throws { Test-MsixAppInstallerPackage -PackagePath $ciPackage -AppInstallerPath $feedPath -Repository $repository } 'does not name the published package'
    Test-MsixAppInstallerPackage -PackagePath $ciPackage -AppInstallerPath $feedPath -Repository $repository -PublishedPackageName $packageName

    Assert-Throws { Test-MsixAppInstallerPackage -PackagePath $package -AppInstallerPath $feedPath -Repository 'Other/fork' } 'does not match the package identity'

    $stale = Join-Path $scratch 'stale.msix'
    New-TestPackage $stale (New-StagedManifest '1.2.4.0' 'x64') $feedBytes
    Assert-Throws { Test-MsixAppInstallerPackage -PackagePath $stale -AppInstallerPath $feedPath -Repository $repository -PublishedPackageName $packageName } 'does not match the package identity'

    $otherFeed = Join-Path $scratch 'other\Devolutions.Jiffshot-windows-x64.appinstaller'
    New-MsixAppInstaller -ManifestPath $manifestPath -Repository 'Other/fork' -OutputPath $otherFeed
    Assert-Throws { Test-MsixAppInstallerPackage -PackagePath $package -AppInstallerPath $otherFeed -Repository $repository } 'differs from the one embedded'

    $renamedFeed = Join-Path $scratch 'feed.appinstaller'
    Copy-Item -LiteralPath $feedPath -Destination $renamedFeed
    Assert-Throws { Test-MsixAppInstallerPackage -PackagePath $package -AppInstallerPath $renamedFeed -Repository $repository } 'not the published feed file'
} finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force
}
Write-Output 'MSIX App Installer tests passed.'
