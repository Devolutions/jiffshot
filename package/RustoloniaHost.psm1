#Requires -Version 7.4
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-JiffshotRustoloniaRevision {
    param([Parameter(Mandatory)][string]$Manifest)

    $text = Get-Content -LiteralPath $Manifest -Raw
    $versions = foreach ($crate in 'rustolonia', 'rustolonia-sys') {
        $line = [regex]::Match($text, "(?m)^$crate\s*=\s*\{[^\r\n]+\}")
        if (-not $line.Success -or
            $line.Value -notmatch 'git\s*=\s*"https://github.com/Devolutions/rustolonia"' -or
            $line.Value -notmatch 'tag\s*=\s*"v(?<version>\d+\.\d+\.\d+)"') {
            throw "$crate must pin a versioned Rustolonia release tag in $Manifest"
        }
        $Matches['version']
    }
    if ($versions[0] -cne $versions[1]) {
        throw 'rustolonia and rustolonia-sys must use the same release tag'
    }

    $lock = Get-Content -LiteralPath (Join-Path (Split-Path -Parent $Manifest) 'Cargo.lock') -Raw
    $revisions = foreach ($crate in 'rustolonia', 'rustolonia-sys') {
        $package = [regex]::Match($lock, "(?ms)^\[\[package\]\]\r?\nname = `"$crate`"\r?\n(.*?)(?=^\[\[package\]\]|\z)")
        if (-not $package.Success -or
            $package.Groups[1].Value -notmatch "source = `"git\+https://github\.com/Devolutions/rustolonia\?tag=v$($versions[0])#(?<revision>[a-f0-9]{40})`"") {
            throw "$crate in Cargo.lock does not resolve the v$($versions[0]) release tag"
        }
        $Matches['revision']
    }
    if ($revisions[0] -cne $revisions[1]) {
        throw 'rustolonia and rustolonia-sys must resolve to the same source revision'
    }
    $revisions[0]
}

function Get-JiffshotRustoloniaVersion {
    param([Parameter(Mandatory)][string]$Manifest)

    $text = Get-Content -LiteralPath $Manifest -Raw
    $line = [regex]::Match($text, '(?m)^rustolonia\s*=\s*\{[^\r\n]+\}')
    if (-not $line.Success -or $line.Value -notmatch 'tag\s*=\s*"v(?<version>\d+\.\d+\.\d+)"') {
        throw "rustolonia must pin a versioned release tag in $Manifest"
    }
    $Matches['version']
}

function Expand-JiffshotRustoloniaHost {
    param(
        [Parameter(Mandatory)][string]$Archive,
        [Parameter(Mandatory)][string]$Destination,
        [Parameter(Mandatory)][string]$Version,
        [Parameter(Mandatory)][string]$Rid,
        [Parameter(Mandatory)][ValidateSet('Shared', 'Static')][string]$Linkage
    )

    $checksum = Get-Content -LiteralPath "$Archive.sha256" -Raw
    $actual = (Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($checksum -cnotmatch '^([a-f0-9]{64})\s+' -or $Matches[1] -cne $actual) {
        throw "Rustolonia archive checksum mismatch: $Archive"
    }
    if (Test-Path -LiteralPath $Destination) {
        throw "Host destination already exists; choose a fresh directory: $Destination"
    }
    [void][IO.Directory]::CreateDirectory($Destination)
    & tar -xzf $Archive -C $Destination
    if ($LASTEXITCODE -ne 0) { throw "Cannot extract Rustolonia host: $Archive" }
    $manifest = Get-Content -LiteralPath (Join-Path $Destination 'host-manifest.json') -Raw | ConvertFrom-Json
    if ($manifest.version -cne $Version -or $manifest.rid -cne $Rid -or $manifest.linkage -cne $Linkage -or
        $manifest.abiFingerprint -notmatch '^[a-f0-9]{64}$') {
        throw "Rustolonia host version, RID, linkage or ABI manifest is invalid for $Version / $Rid / $Linkage"
    }
}

Export-ModuleMember -Function Get-JiffshotRustoloniaRevision, Get-JiffshotRustoloniaVersion, Expand-JiffshotRustoloniaHost
