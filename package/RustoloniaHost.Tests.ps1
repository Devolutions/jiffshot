#Requires -Version 7.4
BeforeAll {
    Import-Module (Join-Path $PSScriptRoot 'RustoloniaHost.psm1') -Force
}

Describe 'Pinned Rustolonia release and host' {
    BeforeEach {
        $script:Revision = '1e8105ac57d35d1b3a2421e19f543dc68a632fe5'
        $script:Version = '12.1.7'
        $script:Case = Join-Path $TestDrive ([guid]::NewGuid().ToString('N'))
        [void][IO.Directory]::CreateDirectory($script:Case)
        $script:Manifest = Join-Path $script:Case 'Cargo.toml'
        @(
            "rustolonia = { git = `"https://github.com/Devolutions/rustolonia`", tag = `"v$script:Version`" }"
            "rustolonia-sys = { git = `"https://github.com/Devolutions/rustolonia`", tag = `"v$script:Version`" }"
        ) | Set-Content -LiteralPath $script:Manifest
        @(
            '[[package]]'
            'name = "rustolonia"'
            'version = "12.1.7"'
            'source = "git+https://github.com/Devolutions/rustolonia?tag=v12.1.7#1e8105ac57d35d1b3a2421e19f543dc68a632fe5"'
            ''
            '[[package]]'
            'name = "rustolonia-sys"'
            'version = "12.1.7"'
            'source = "git+https://github.com/Devolutions/rustolonia?tag=v12.1.7#1e8105ac57d35d1b3a2421e19f543dc68a632fe5"'
        ) | Set-Content -LiteralPath (Join-Path $script:Case 'Cargo.lock')
    }

    It 'reads the exact source commit resolved from the shared release tag' {
        Get-JiffshotRustoloniaRevision -Manifest $script:Manifest | Should -BeExactly $script:Revision
        Get-JiffshotRustoloniaVersion -Manifest $script:Manifest | Should -BeExactly $script:Version
    }

    It 'rejects mismatched crate tags' {
        (Get-Content -LiteralPath $script:Manifest) -replace '^(rustolonia-sys.*)v12\.1\.7', '${1}v12.1.8' |
            Set-Content -LiteralPath $script:Manifest
        { Get-JiffshotRustoloniaRevision -Manifest $script:Manifest } | Should -Throw '*same release tag*'
    }

    It 'rejects a lockfile resolved from another tag' {
        $lock = Join-Path $script:Case 'Cargo.lock'
        (Get-Content -LiteralPath $lock -Raw).Replace('tag=v12.1.7#', 'tag=v12.1.6#') |
            Set-Content -LiteralPath $lock
        { Get-JiffshotRustoloniaRevision -Manifest $script:Manifest } | Should -Throw '*does not resolve*'
    }

    Context 'archive staging' {
        BeforeEach {
            $script:Bundle = Join-Path $script:Case 'bundle'
            [void][IO.Directory]::CreateDirectory($script:Bundle)
            @{
                version = $script:Version
                sourceRevision = '48e7c513bcc3e85c00aa4a16470bb6390a116d56'
                abiFingerprint = '745f61b346977c8c4618fb8e32fc41cbfb80f764b8bd1a7506763072b43e554f'
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

        It 'verifies and stages the exact release, RID, linkage and ABI metadata' {
            Expand-JiffshotRustoloniaHost -Archive $script:Archive -Destination $script:Destination `
                -Version $script:Version -Rid osx-arm64 -Linkage Static
            Test-Path (Join-Path $script:Destination 'host-manifest.json') | Should -BeTrue
        }

        It 'rejects a mismatched <Field> even when the archive checksum is valid' -ForEach @(
            @{ Field = 'Version'; Version = '12.1.8'; Rid = 'osx-arm64'; Linkage = 'Static'; Abi = '745f61b346977c8c4618fb8e32fc41cbfb80f764b8bd1a7506763072b43e554f' }
            @{ Field = 'Rid'; Version = '12.1.7'; Rid = 'osx-x64'; Linkage = 'Static'; Abi = '745f61b346977c8c4618fb8e32fc41cbfb80f764b8bd1a7506763072b43e554f' }
            @{ Field = 'Linkage'; Version = '12.1.7'; Rid = 'osx-arm64'; Linkage = 'Shared'; Abi = '745f61b346977c8c4618fb8e32fc41cbfb80f764b8bd1a7506763072b43e554f' }
            @{ Field = 'ABI'; Version = '12.1.7'; Rid = 'osx-arm64'; Linkage = 'Static'; Abi = 'not-a-fingerprint' }
        ) {
            if ($Field -eq 'ABI') {
                (Get-Content -LiteralPath (Join-Path $script:Bundle 'host-manifest.json') -Raw).Replace(
                    '745f61b346977c8c4618fb8e32fc41cbfb80f764b8bd1a7506763072b43e554f', $Abi
                ) | Set-Content -LiteralPath (Join-Path $script:Bundle 'host-manifest.json')
                & tar -czf $script:Archive -C $script:Bundle host-manifest.json
                "$((Get-FileHash $script:Archive -Algorithm SHA256).Hash.ToLowerInvariant())  host.tar.gz" |
                    Set-Content -LiteralPath "$script:Archive.sha256"
            }
            { Expand-JiffshotRustoloniaHost -Archive $script:Archive -Destination $script:Destination `
                -Version $Version -Rid $Rid -Linkage $Linkage } | Should -Throw '*manifest is invalid*'
        }

        It 'rejects corrupt output before extracting it' {
            'tampered' | Add-Content -LiteralPath $script:Archive
            { Expand-JiffshotRustoloniaHost -Archive $script:Archive -Destination $script:Destination `
                -Version $script:Version -Rid osx-arm64 -Linkage Static } | Should -Throw '*checksum mismatch*'
            Test-Path $script:Destination | Should -BeFalse
        }

        It 'refuses a stale staging directory instead of mixing companions' {
            [void][IO.Directory]::CreateDirectory($script:Destination)
            { Expand-JiffshotRustoloniaHost -Archive $script:Archive -Destination $script:Destination `
                -Version $script:Version -Rid osx-arm64 -Linkage Static } | Should -Throw '*already exists*'
        }
    }
}
