#Requires -Version 7.4

Describe 'macOS source host build' {
    BeforeAll {
        $script:BuildScript = Join-Path $PSScriptRoot 'build-host.ps1'
    }

    BeforeEach {
        $script:Source = Join-Path $TestDrive ([guid]::NewGuid().ToString('N'))
        [void][IO.Directory]::CreateDirectory($script:Source)
        $script:PreviousDeploymentTarget = $env:MACOSX_DEPLOYMENT_TARGET
        $script:PreviousGitHubEnv = $env:GITHUB_ENV
        $script:OriginalLocation = (Get-Location).Path
        $env:MACOSX_DEPLOYMENT_TARGET = '14.0'
        $env:GITHUB_ENV = Join-Path $TestDrive 'github-env'
        if (Test-Path -LiteralPath $env:GITHUB_ENV) { Remove-Item -LiteralPath $env:GITHUB_ENV }
        Mock pwsh { $global:LASTEXITCODE = 0 }
        Mock dotnet { $global:LASTEXITCODE = 0 }
        Mock xcodebuild { $global:LASTEXITCODE = 0 }
    }

    AfterEach {
        $env:MACOSX_DEPLOYMENT_TARGET | Should -BeExactly '14.0'
        (Get-Location).Path | Should -BeExactly $script:OriginalLocation
        $env:MACOSX_DEPLOYMENT_TARGET = $script:PreviousDeploymentTarget
        $env:GITHUB_ENV = $script:PreviousGitHubEnv
    }

    It 'builds and stages <Arch> with its matching companions' -ForEach @(
        @{ Arch = 'arm64'; Rid = 'osx-arm64' }
        @{ Arch = 'x86_64'; Rid = 'osx-x64' }
    ) {
        $directory = Join-Path $script:Source "rust/target/ci-host-$Rid/publish/Avalonia.Host/release_$Rid"
        [void][IO.Directory]::CreateDirectory($directory)
        foreach ($library in 'librustolonia_host.dylib', 'libAvaloniaNative.dylib',
                 'libSkiaSharp.dylib', 'libHarfBuzzSharp.dylib') {
            [IO.File]::WriteAllText((Join-Path $directory $library), $library)
        }

        & $script:BuildScript -Source $script:Source -Arch $Arch

        Should -Invoke pwsh -Times 2 -Exactly
        Should -Invoke dotnet -Times 3 -Exactly
        Should -Invoke xcodebuild -Times 1 -Exactly -ParameterFilter {
            $args -contains 'ARCHS=arm64 x86_64' -and
            $args -contains 'MACOSX_DEPLOYMENT_TARGET=13.0' -and
            $args -contains "CONFIGURATION_BUILD_DIR=$script:Source/avalonia-src/artifacts/native/Release"
        }
        Should -Invoke dotnet -Times 1 -Exactly -ParameterFilter {
            $args -contains 'publish' -and $args -contains $Rid -and
            $args -contains '-p:BuildAvaloniaNativeXcodeProject=false'
        }
        $checksums = Get-Content -LiteralPath (Join-Path $directory 'SHA256SUMS.txt')
        $checksums.Count | Should -Be 4
        $checksums[0] | Should -Match '^[a-f0-9]{64}  librustolonia_host\.dylib$'
        $environment = Get-Content -LiteralPath $env:GITHUB_ENV
        $environment | Should -Contain "RUSTOLONIA_HOST_DIR=$directory"
        $environment | Should -Contain "RUSTOLONIA_HOST_LIB=$directory/librustolonia_host.dylib"
    }

    It 'fails immediately when a build command fails' {
        Mock dotnet { $global:LASTEXITCODE = 42 }
        { & $script:BuildScript -Source $script:Source -Arch arm64 } |
            Should -Throw '*dotnet failed with exit code 42*'
        Should -Invoke xcodebuild -Times 0
        Test-Path -LiteralPath $env:GITHUB_ENV | Should -BeFalse
    }

    It 'rejects incomplete host output rather than selecting a released companion' {
        { & $script:BuildScript -Source $script:Source -Arch x86_64 } |
            Should -Throw '*Published host is missing*librustolonia_host.dylib*'
        Test-Path -LiteralPath $env:GITHUB_ENV | Should -BeFalse
    }
}
