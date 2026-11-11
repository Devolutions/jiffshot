#Requires -Version 7.4
BeforeAll {
    Import-Module (Join-Path $PSScriptRoot 'RustoloniaHost.psm1') -Force
}

Describe 'Pinned Rustolonia source and host' {
    BeforeEach {
        $script:Revision = '841feb8799e1bcfb7d70b92e1395ef2311df0136'
        $script:Case = Join-Path $TestDrive ([guid]::NewGuid().ToString('N'))
        [void][IO.Directory]::CreateDirectory($script:Case)
        $script:Manifest = Join-Path $script:Case 'Cargo.toml'
        @(
            "rustolonia = { git = `"https://github.com/Devolutions/rustolonia`", rev = `"$script:Revision`" }"
            "rustolonia-sys = { git = `"https://github.com/Devolutions/rustolonia`", rev = `"$script:Revision`" }"
        ) | Set-Content -LiteralPath $script:Manifest
    }

    It 'reads the shared full revision without a separately maintained host pin' {
        Get-JiffshotRustoloniaRevision -Manifest $script:Manifest | Should -BeExactly $script:Revision
    }

    It 'rejects mismatched crate revisions' {
        (Get-Content -LiteralPath $script:Manifest) -replace '^(rustolonia-sys.*)841feb8', '${1}941feb8' |
            Set-Content -LiteralPath $script:Manifest
        { Get-JiffshotRustoloniaRevision -Manifest $script:Manifest } | Should -Throw '*same revision*'
    }

    It 'rejects a release tag or a short revision' {
        (Get-Content -LiteralPath $script:Manifest -Raw).Replace("rev = `"$script:Revision`"", 'tag = "v12.1.6"') |
            Set-Content -LiteralPath $script:Manifest
        { Get-JiffshotRustoloniaRevision -Manifest $script:Manifest } | Should -Throw '*full Rustolonia git revision*'
    }

    Context 'archive staging' {
        BeforeEach {
            $script:Bundle = Join-Path $script:Case 'bundle'
            [void][IO.Directory]::CreateDirectory($script:Bundle)
            @{
                sourceRevision = $script:Revision
                rid = 'osx-arm64'
                linkage = 'Static'
            } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $script:Bundle 'host-manifest.json')
            $script:Archive = Join-Path $script:Case 'host.tar.gz'
            & tar -czf $script:Archive -C $script:Bundle host-manifest.json
            if ($LASTEXITCODE -ne 0) { throw 'Cannot create test host archive' }
            "$((Get-FileHash $script:Archive -Algorithm SHA256).Hash.ToLowerInvariant())  host.tar.gz" |
                Set-Content -LiteralPath "$script:Archive.sha256"
            $script:Destination = Join-Path $script:Case 'staged'
        }

        It 'verifies and stages the exact source revision, RID and linkage' {
            Expand-JiffshotRustoloniaHost -Archive $script:Archive -Destination $script:Destination `
                -Revision $script:Revision -Rid osx-arm64 -Linkage Static
            Test-Path (Join-Path $script:Destination 'host-manifest.json') | Should -BeTrue
        }

        It 'rejects a wrong <Field> even when the archive checksum is valid' -ForEach @(
            @{ Field = 'Revision'; Revision = '941feb8799e1bcfb7d70b92e1395ef2311df0136'; Rid = 'osx-arm64'; Linkage = 'Static' }
            @{ Field = 'Rid'; Revision = '841feb8799e1bcfb7d70b92e1395ef2311df0136'; Rid = 'osx-x64'; Linkage = 'Static' }
            @{ Field = 'Linkage'; Revision = '841feb8799e1bcfb7d70b92e1395ef2311df0136'; Rid = 'osx-arm64'; Linkage = 'Shared' }
        ) {
            { Expand-JiffshotRustoloniaHost -Archive $script:Archive -Destination $script:Destination `
                -Revision $Revision -Rid $Rid -Linkage $Linkage } | Should -Throw '*does not match*'
        }

        It 'rejects corrupt output before extracting it' {
            'tampered' | Add-Content -LiteralPath $script:Archive
            { Expand-JiffshotRustoloniaHost -Archive $script:Archive -Destination $script:Destination `
                -Revision $script:Revision -Rid osx-arm64 -Linkage Static } | Should -Throw '*checksum mismatch*'
            Test-Path $script:Destination | Should -BeFalse
        }

        It 'refuses a stale staging directory instead of mixing companions' {
            [void][IO.Directory]::CreateDirectory($script:Destination)
            { Expand-JiffshotRustoloniaHost -Archive $script:Archive -Destination $script:Destination `
                -Revision $script:Revision -Rid osx-arm64 -Linkage Static } | Should -Throw '*already exists*'
        }
    }
}
