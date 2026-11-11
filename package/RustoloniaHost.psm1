#Requires -Version 7.4
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-JiffshotRustoloniaRevision {
    param([Parameter(Mandatory)][string]$Manifest)

    $text = Get-Content -LiteralPath $Manifest -Raw
    $revisions = foreach ($crate in 'rustolonia', 'rustolonia-sys') {
        $line = [regex]::Match($text, "(?m)^$crate\s*=\s*\{[^\r\n]+\}")
        if (-not $line.Success -or
            $line.Value -notmatch 'git\s*=\s*"https://github.com/Devolutions/rustolonia"' -or
            $line.Value -notmatch 'rev\s*=\s*"(?<revision>[a-f0-9]{40})"') {
            throw "$crate must pin a full Rustolonia git revision in $Manifest"
        }
        $Matches['revision']
    }
    if ($revisions[0] -cne $revisions[1]) {
        throw 'rustolonia and rustolonia-sys must use the same revision'
    }
    $revisions[0]
}

function Expand-JiffshotRustoloniaHost {
    param(
        [Parameter(Mandatory)][string]$Archive,
        [Parameter(Mandatory)][string]$Destination,
        [Parameter(Mandatory)][string]$Revision,
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
    if ($manifest.sourceRevision -cne $Revision -or $manifest.rid -cne $Rid -or
        $manifest.linkage -cne $Linkage) {
        throw "Rustolonia host revision, RID or linkage does not match $Revision / $Rid / $Linkage"
    }
}

Export-ModuleMember -Function Get-JiffshotRustoloniaRevision, Expand-JiffshotRustoloniaHost
