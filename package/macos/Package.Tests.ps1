#Requires -Version 7.4

Import-Module (Join-Path $PSScriptRoot 'JiffshotMacPackage.psm1') -Force

Describe 'macOS packaging' {
    InModuleScope JiffshotMacPackage {
        BeforeEach {
            $script:Work = Join-Path $TestDrive ([guid]::NewGuid().ToString('N'))
            [void][IO.Directory]::CreateDirectory($script:Work)
        }

        It 'reads and validates the workspace version' {
            $version = Get-MacWorkspaceVersion
            $version | Should -Match '^\d+\.\d+\.\d+$'
            $previous = $env:JIFFSHOT_VERSION
            try {
                $env:JIFFSHOT_VERSION = '999.0.0'
                $binary = Join-Path $script:Work 'jiffshot'
                foreach ($name in @('jiffshot', 'jiffshot-cli') + $script:HostLibraries) {
                    [IO.File]::WriteAllText((Join-Path $script:Work $name), 'fixture')
                }
                Mock Invoke-MacRun {}
                Mock Get-MacDeploymentTarget { '13.0' }
                { Invoke-MacPackage -Binary $binary -Output (Join-Path $script:Work 'out') -Arch arm64 } |
                    Should -Throw '*does not match Cargo.toml*'
            }
            finally { $env:JIFFSHOT_VERSION = $previous }
        }

        It 'parses modern and legacy macOS deployment targets' {
            $modern = "Load command 10`n cmd LC_BUILD_VERSION`n platform MACOS`n minos 13.0`n"
            $legacy = "Load command 9`n cmd LC_VERSION_MIN_MACOSX`n version 10.13`n"
            Read-MacDeploymentTarget $modern | Should -BeExactly '13.0'
            Read-MacDeploymentTarget ($modern.Replace('MACOS', '1')) | Should -BeExactly '13.0'
            Read-MacDeploymentTarget $legacy | Should -BeExactly '10.13'
            (ConvertTo-MacVersion '10.13') -lt (ConvertTo-MacVersion '13.0') | Should -BeTrue
            (ConvertTo-MacVersion '13.0.1') -gt (ConvertTo-MacVersion '13.0') | Should -BeTrue
            ConvertTo-MacVersion '13' | Should -Be (ConvertTo-MacVersion '13.0.0')
        }

        It 'rejects missing, invalid, ambiguous, and non-macOS load commands' {
            $modern = "Load command 1`n cmd LC_BUILD_VERSION`n platform MACOS`n minos 13.0`n"
            foreach ($value in @('', $modern.Replace('MACOS', 'IOS'), $modern.Replace('13.0', 'unknown'),
                                 ($modern + "Load command 2`n cmd LC_VERSION_MIN_MACOSX`n version 13.0`n"))) {
                { Read-MacDeploymentTarget $value } | Should -Throw
            }
        }

        It 'reports Mach-O inspection and metadata failures' {
            Mock Invoke-MacCommand { throw 'malformed load commands' }
            { Get-MacDeploymentTarget '/fixture/jiffshot' 'arm64' } | Should -Throw '*Cannot inspect*arm64*malformed load commands*'
            Mock Invoke-MacCommand { [pscustomobject]@{ Stdout = 'not a Mach-O' } }
            { Get-MacDeploymentTarget '/fixture/jiffshot' 'arm64' } | Should -Throw '*Cannot determine the macOS deployment target*'
        }

        It 'writes a valid property list with privacy and document declarations' {
            $plist = Get-MacInfoPlist '1.2.3'
            $plist.CFBundleShortVersionString | Should -BeExactly '1.2.3'
            $plist.LSMinimumSystemVersion | Should -BeExactly '13.0'
            foreach ($key in 'NSCameraUsageDescription', 'NSMicrophoneUsageDescription', 'NSScreenCaptureUsageDescription', 'NSAppleEventsUsageDescription') {
                $plist[$key] | Should -Not -BeNullOrEmpty
            }
            $plist.CFBundleDocumentTypes[0].LSItemContentTypes[0] | Should -BeExactly 'com.devolutions.jiffshot.document'
            $plist.UTExportedTypeDeclarations[0].UTTypeTagSpecification['public.filename-extension'][0] | Should -BeExactly 'jsd'
            $path = Join-Path $script:Work 'Info.plist'
            Write-MacPlist $path $plist
            $result = Invoke-MacCommand -File '/usr/bin/plutil' -Arguments @('-lint', $path)
            $result.ExitCode | Should -Be 0
        }

        It 'enables hardened runtime and entitlements for Developer ID signing' {
            Mock Invoke-MacRun {}
            Sign-MacPath '/fixture/Jiffshot.app' 'Developer ID Application: Example' $null '/fixture/entitlements.plist'
            Should -Invoke Invoke-MacRun -Times 1 -Exactly -ParameterFilter {
                $File -ceq '/usr/bin/codesign' -and
                ($Arguments -join '|') -ceq '--force|--sign|Developer ID Application: Example|--timestamp|--options|runtime|--entitlements|/fixture/entitlements.plist|/fixture/Jiffshot.app'
            }
        }

        It 'refuses a missing host and ad-hoc notarization' {
            $binary = Join-Path $script:Work 'jiffshot'
            [IO.File]::WriteAllText($binary, 'fixture')
            Mock Invoke-MacRun {}
            Mock Get-MacDeploymentTarget { '13.0' }
            { Invoke-MacPackage -Binary $binary -Output (Join-Path $script:Work 'out') -Arch arm64 } |
                Should -Throw '*Required bundle binary*'
            foreach ($name in @('jiffshot-cli') + $script:HostLibraries) {
                [IO.File]::WriteAllText((Join-Path $script:Work $name), 'fixture')
            }
            { Invoke-MacPackage -Binary $binary -Output (Join-Path $script:Work 'out') -Arch arm64 -NotaryProfile 'profile' } |
                Should -Throw '*Notarization requires*'
            { Invoke-MacPackage -Binary $binary -Output (Join-Path $script:Work 'out') -Arch arm64 `
                -Identity 'Developer ID Application: Example' -RequirePopupGate $false -RequireTermination $false -RequireFileDrag $false } |
                Should -Throw '*Signing or notarization requires all*'
        }

        It 'rejects a component requiring newer macOS before staging' {
            $binary = Join-Path $script:Work 'jiffshot'
            foreach ($name in @('jiffshot', 'jiffshot-cli') + $script:HostLibraries) {
                [IO.File]::WriteAllText((Join-Path $script:Work $name), 'fixture')
            }
            Mock Invoke-MacRun {}
            Mock Get-MacDeploymentTarget {
                if ($Path.EndsWith('libSkiaSharp.dylib')) { '13.1' } else { '13.0' }
            }
            $output = Join-Path $script:Work 'out'
            { Invoke-MacPackage -Binary $binary -Output $output -Arch x86_64 } |
                Should -Throw '*libSkiaSharp.dylib requires macOS 13.1*'
            Test-Path -LiteralPath $output | Should -BeFalse
        }

        It 'requires exact capability markers with a clean environment' {
            $binary = '/fixture/Jiffshot.app/Contents/MacOS/jiffshot'
            $hostPath = '/fixture/Jiffshot.app/Contents/Frameworks/librustolonia_host.dylib'
            $previous = $env:DYLD_INSERT_LIBRARIES
            try {
                $env:DYLD_INSERT_LIBRARIES = '/wrong'
                Mock Invoke-MacCommand {
                    [pscustomobject]@{ ExitCode = 0; Stdout = "diagnostic`njiffshot-native-popup-gate-v1: supported`n"; Stderr = '' }
                }
                Confirm-MacPopupGate $binary $hostPath
                Should -Invoke Invoke-MacCommand -Times 1 -Exactly -ParameterFilter {
                    $File -ceq $binary -and $Arguments[0] -ceq '--internal-check-native-popup-gate' -and
                    $Environment['RUSTOLONIA_HOST_LIB'] -ceq $hostPath -and
                    $Environment['RUSTOLONIA_HOST_DIR'] -ceq '/fixture/Jiffshot.app/Contents/Frameworks' -and
                    -not $Environment.ContainsKey('DYLD_INSERT_LIBRARIES') -and $TimeoutSeconds -eq 30
                }
                foreach ($result in @(
                    [pscustomobject]@{ ExitCode = 1; Stdout = ''; Stderr = 'no popup gate' },
                    [pscustomobject]@{ ExitCode = 0; Stdout = 'usage: jiffshot'; Stderr = '' },
                    [pscustomobject]@{ ExitCode = 0; Stdout = 'prefix jiffshot-native-popup-gate-v1: supported'; Stderr = '' }
                )) {
                    Mock Invoke-MacCommand { $result }
                    { Confirm-MacPopupGate $binary $hostPath } | Should -Throw '*does not pass the native popup-gate check*'
                }
            }
            finally { $env:DYLD_INSERT_LIBRARIES = $previous }
        }

        It 'reports failed and timed-out capability probes' {
            foreach ($message in 'cannot execute', 'timed out') {
                Mock Invoke-MacCommand { throw $message }
                { Confirm-MacPopupGate '/fixture/jiffshot' '/fixture/host.dylib' } |
                    Should -Throw '*Could not check the staged Rustolonia popup-gate*'
            }
        }

        It 'checks termination and file-drag companion and ABI separately' {
            $script:ArgumentsSeen = [Collections.Generic.List[string]]::new()
            Mock Invoke-MacCommand {
                $script:ArgumentsSeen.Add($Arguments[0])
                $marker = switch ($Arguments[0]) {
                    '--internal-check-native-termination' { 'jiffshot-native-termination-v1: supported' }
                    '--internal-check-native-file-drag' { 'jiffshot-native-file-drag-v1: supported' }
                    '--internal-check-file-drag-abi' { 'jiffshot-file-drag-abi-v1: supported' }
                }
                [pscustomobject]@{ ExitCode = 0; Stdout = "$marker`n"; Stderr = '' }
            }
            Confirm-MacTermination '/fixture/jiffshot' '/fixture/host.dylib'
            Confirm-MacFileDrag '/fixture/jiffshot' '/fixture/host.dylib'
            $script:ArgumentsSeen.ToArray() | Should -Be @(
                '--internal-check-native-termination', '--internal-check-native-file-drag', '--internal-check-file-drag-abi'
            )
            Mock Invoke-MacCommand { [pscustomobject]@{ ExitCode = 0; Stdout = 'old host'; Stderr = '' } }
            { Confirm-MacTermination '/fixture/jiffshot' '/fixture/host.dylib' } | Should -Throw '*native termination check*'
            { Confirm-MacFileDrag '/fixture/jiffshot' '/fixture/host.dylib' } | Should -Throw '*native file-drag*'
        }

        It 'accepts only well-formed host-discovery responses' {
            $marker = "jiffshot-host-discovery-v1$([char]0)"
            $path = "/fixture/host 雪`n.dylib"
            Mock Resolve-MacPhysicalPath { $Path }
            Mock Invoke-MacCommand { [pscustomobject]@{ ExitCode = 0; Stdout = "$marker$path"; Stderr = '' } }
            Get-MacDiscoveredHost '/fixture/jiffshot' '/fixture' | Should -BeExactly $path
            Mock Invoke-MacCommand { [pscustomobject]@{ ExitCode = 1; Stdout = ''; Stderr = 'jiffshot-host-discovery-v1: not found' } }
            Get-MacDiscoveredHost '/fixture/jiffshot' '/fixture' | Should -BeNullOrEmpty
            foreach ($result in @(
                [pscustomobject]@{ ExitCode = 0; Stdout = 'unexpected'; Stderr = '' },
                [pscustomobject]@{ ExitCode = 0; Stdout = $marker; Stderr = '' },
                [pscustomobject]@{ ExitCode = 1; Stdout = ''; Stderr = 'unrelated failure' },
                [pscustomobject]@{ ExitCode = 1; Stdout = 'unexpected'; Stderr = 'jiffshot-host-discovery-v1: not found' },
                [pscustomobject]@{ ExitCode = 1; Stdout = ''; Stderr = 'jiffshot-host-discovery-v1: ' },
                [pscustomobject]@{ ExitCode = -11; Stdout = ''; Stderr = '' }
            )) {
                Mock Invoke-MacCommand { $result }
                { Get-MacDiscoveredHost '/fixture/jiffshot' '/fixture' } | Should -Throw '*host discovery probe failed*'
            }
            Mock Invoke-MacCommand { throw 'timed out' }
            { Get-MacDiscoveredHost '/fixture/jiffshot' '/fixture' } | Should -Throw '*Could not check Rustolonia host discovery*'
        }

        It 'checks host discovery in both directories without inherited host overrides' {
            $binary = Join-Path $script:Work 'jiffshot'
            $hostPath = Join-Path $script:Work 'host.dylib'
            [IO.File]::WriteAllText($binary, 'fixture')
            [IO.File]::WriteAllText($hostPath, 'host')
            $isolated = Join-Path $script:Work 'isolated-host-probe'
            $unbundled = Join-Path $isolated 'jiffshot'
            $variables = @(
                'RUSTOLONIA_HOST_LIB', 'RUSTOLONIA_HOST_DIR', 'RUSTOLONIA_HOST_BASE_URL',
                'DYLD_LIBRARY_PATH', 'DYLD_INSERT_LIBRARIES'
            )
            $previous = @{}
            try {
                foreach ($name in $variables) {
                    $previous[$name] = [Environment]::GetEnvironmentVariable($name)
                    [Environment]::SetEnvironmentVariable($name, '/must-not-leak')
                }
                Mock Resolve-MacPhysicalPath { $Path }
                $script:ProbeCount = 0
                Mock Invoke-MacCommand {
                    $script:ProbeCount++
                    if ($script:ProbeCount -eq 1) {
                        [pscustomobject]@{
                            ExitCode = 0
                            Stdout = "jiffshot-host-discovery-v1$([char]0)$hostPath"
                            Stderr = ''
                        }
                    }
                    else {
                        [pscustomobject]@{
                            ExitCode = 1
                            Stdout = ''
                            Stderr = 'jiffshot-host-discovery-v1: not found'
                        }
                    }
                }
                Confirm-MacHostDiscovery $binary $hostPath $script:Work
                Should -Invoke Invoke-MacCommand -Times 1 -Exactly -ParameterFilter {
                    $File -ceq $binary -and $WorkingDirectory -ceq $script:Work -and
                    $Arguments.Count -eq 1 -and $Arguments[0] -ceq '--internal-check-host-discovery' -and
                    $TimeoutSeconds -eq 30 -and
                    @($variables | Where-Object { $Environment.ContainsKey($_) }).Count -eq 0
                }
                Should -Invoke Invoke-MacCommand -Times 1 -Exactly -ParameterFilter {
                    $File -ceq $unbundled -and $WorkingDirectory -ceq $isolated -and
                    $Arguments.Count -eq 1 -and $Arguments[0] -ceq '--internal-check-host-discovery' -and
                    $TimeoutSeconds -eq 30 -and
                    @($variables | Where-Object { $Environment.ContainsKey($_) }).Count -eq 0
                }
                [IO.File]::ReadAllText($unbundled) | Should -BeExactly 'fixture'
            }
            finally {
                foreach ($name in $variables) {
                    [Environment]::SetEnvironmentVariable($name, $previous[$name])
                }
            }
        }

        It 'requires the bundled host and rejects an implicit developer host' {
            $binary = Join-Path $script:Work 'jiffshot'
            $hostPath = Join-Path $script:Work 'host.dylib'
            [IO.File]::WriteAllText($binary, 'fixture')
            [IO.File]::WriteAllText($hostPath, 'host')
            Mock Resolve-MacPhysicalPath { $Path }
            $script:ProbeCount = 0
            Mock Get-MacDiscoveredHost {
                $script:ProbeCount++
                if ($script:ProbeCount -eq 1) { $hostPath }
            }
            Confirm-MacHostDiscovery $binary $hostPath $script:Work
            [IO.File]::ReadAllText((Join-Path $script:Work 'isolated-host-probe/jiffshot')) | Should -BeExactly 'fixture'
            Mock Get-MacDiscoveredHost { '/wrong/host.dylib' }
            { Confirm-MacHostDiscovery $binary $hostPath $script:Work } | Should -Throw '*did not discover its bundled host*'
            [IO.File]::Delete((Join-Path $script:Work 'isolated-host-probe/jiffshot'))
            $script:ProbeCount = 0
            Mock Get-MacDiscoveredHost {
                $script:ProbeCount++
                if ($script:ProbeCount -eq 1) { $hostPath } else { '/developer/host.dylib' }
            }
            { Confirm-MacHostDiscovery $binary $hostPath $script:Work } | Should -Throw '*unstaged Rustolonia host*'
        }

        It 'rejects existing package output' {
            $binary = Join-Path $script:Work 'jiffshot'
            foreach ($name in @('jiffshot', 'jiffshot-cli') + $script:HostLibraries) {
                [IO.File]::WriteAllText((Join-Path $script:Work $name), 'fixture')
            }
            $output = Join-Path $script:Work 'out'
            [void][IO.Directory]::CreateDirectory((Join-Path $output 'Jiffshot.app'))
            Mock Invoke-MacRun {}
            Mock Get-MacDeploymentTarget { '13.0' }
            { Invoke-MacPackage -Binary $binary -Output $output -Arch arm64 } | Should -Throw '*already exists*'
        }

        It 'stages and signs all libraries before every required capability probe' {
            $binary = Join-Path $script:Work 'jiffshot'
            foreach ($name in @('jiffshot', 'jiffshot-cli') + $script:HostLibraries) {
                [IO.File]::WriteAllText((Join-Path $script:Work $name), $name)
            }
            $script:Signed = [Collections.Generic.List[string]]::new()
            Mock Get-MacDeploymentTarget { '13.0' }
            Mock New-MacIcon {}
            Mock Sign-MacPath { $script:Signed.Add($Path) }
            Mock Invoke-MacRun {
                if ($File -ceq '/usr/bin/hdiutil') { [IO.File]::WriteAllText($Arguments[-1], 'disk image') }
                elseif ($File -ceq '/usr/bin/ditto') { Copy-Item -LiteralPath $Arguments[0] -Destination $Arguments[1] -Recurse }
            }
            Mock Confirm-MacHostDiscovery { $script:Signed.Count | Should -Be 6 }
            Mock Confirm-MacPopupGate { $script:Signed.Count | Should -Be 6 }
            Mock Confirm-MacTermination { $script:Signed.Count | Should -Be 6 }
            Mock Confirm-MacFileDrag { $script:Signed.Count | Should -Be 6 }
            $output = Join-Path $script:Work 'out'
            Invoke-MacPackage -Binary $binary -Output $output -Arch arm64
            foreach ($name in $script:HostLibraries) {
                [IO.File]::ReadAllText((Join-Path $output "Jiffshot.app/Contents/Frameworks/$name")) | Should -BeExactly $name
                Test-Path -LiteralPath (Join-Path $output "Jiffshot.app/Contents/MacOS/$name") | Should -BeFalse
                @($script:Signed | Where-Object { $_.EndsWith("/Frameworks/$name") }).Count | Should -Be 1
            }
            [IO.File]::ReadAllText((Join-Path $output 'Jiffshot.app/Contents/MacOS/jiffshot')) | Should -BeExactly 'jiffshot'
            [IO.File]::ReadAllText((Join-Path $output 'Jiffshot.app/Contents/MacOS/jiffshot-cli')) | Should -BeExactly 'jiffshot-cli'
            $script:Signed[-2] | Should -Match '/Contents/MacOS/jiffshot-cli$'
            $script:Signed[-1] | Should -Match '/Jiffshot\.app$'
            Should -Invoke Sign-MacPath -Times 1 -Exactly -ParameterFilter {
                $Path.EndsWith('/Jiffshot.app') -and $Identity -ceq '-' -and
                $Entitlements.EndsWith('/package/macos/entitlements.plist')
            }
            Should -Invoke Confirm-MacHostDiscovery -Times 1 -Exactly
            Should -Invoke Confirm-MacPopupGate -Times 1 -Exactly
            Should -Invoke Confirm-MacTermination -Times 1 -Exactly
            Should -Invoke Confirm-MacFileDrag -Times 1 -Exactly
        }

        It 'does not publish an artifact when a required host probe fails' {
            $binary = Join-Path $script:Work 'jiffshot'
            foreach ($name in @('jiffshot', 'jiffshot-cli') + $script:HostLibraries) {
                [IO.File]::WriteAllText((Join-Path $script:Work $name), $name)
            }
            Mock Get-MacDeploymentTarget { '13.0' }
            Mock New-MacIcon {}
            Mock Sign-MacPath {}
            Mock Invoke-MacRun {}
            Mock Confirm-MacHostDiscovery {}
            Mock Confirm-MacPopupGate {}
            Mock Confirm-MacTermination {}
            Mock Confirm-MacFileDrag { throw 'fixture drag companion rejected' }
            $output = Join-Path $script:Work 'out'
            { Invoke-MacPackage -Binary $binary -Output $output -Arch arm64 } | Should -Throw '*fixture drag companion rejected*'
            Test-Path -LiteralPath (Join-Path $output 'Jiffshot.app') | Should -BeFalse
            @(Get-ChildItem -LiteralPath $output -Filter '*.dmg').Count | Should -Be 0
        }

        It 'rejects <Kind> failure for the independently required <Gate> gate' -TestCases @(
            @{ Kind = 'discovery'; Gate = 'popup' }
            @{ Kind = 'discovery'; Gate = 'termination' }
            @{ Kind = 'discovery'; Gate = 'drag' }
            @{ Kind = 'popup'; Gate = 'popup' }
            @{ Kind = 'termination'; Gate = 'termination' }
            @{ Kind = 'drag'; Gate = 'drag' }
        ) {
            param($Kind, $Gate)
            $binary = Join-Path $script:Work 'jiffshot'
            foreach ($name in @('jiffshot', 'jiffshot-cli') + $script:HostLibraries) {
                [IO.File]::WriteAllText((Join-Path $script:Work $name), $name)
            }
            $script:RejectedProbe = $Kind
            Mock Get-MacDeploymentTarget { '13.0' }
            Mock New-MacIcon {}
            Mock Sign-MacPath {}
            Mock Invoke-MacRun {}
            Mock Confirm-MacHostDiscovery {
                [IO.File]::ReadAllText($Binary) | Should -BeExactly 'jiffshot'
                [IO.File]::ReadAllText($HostPath) | Should -BeExactly 'librustolonia_host.dylib'
                $Binary | Should -Match '/Contents/MacOS/jiffshot$'
                $HostPath | Should -Match '/Contents/Frameworks/librustolonia_host\.dylib$'
                if ($script:RejectedProbe -ceq 'discovery') { throw 'fixture discovery rejected' }
            }
            Mock Confirm-MacPopupGate {
                [IO.File]::ReadAllText($Binary) | Should -BeExactly 'jiffshot'
                [IO.File]::ReadAllText($HostPath) | Should -BeExactly 'librustolonia_host.dylib'
                if ($script:RejectedProbe -ceq 'popup') { throw 'fixture popup rejected' }
            }
            Mock Confirm-MacTermination {
                [IO.File]::ReadAllText((Join-Path ([IO.Path]::GetDirectoryName($HostPath)) 'libAvaloniaNative.dylib')) |
                    Should -BeExactly 'libAvaloniaNative.dylib'
                if ($script:RejectedProbe -ceq 'termination') { throw 'fixture termination rejected' }
            }
            Mock Confirm-MacFileDrag {
                [IO.File]::ReadAllText((Join-Path ([IO.Path]::GetDirectoryName($HostPath)) 'libAvaloniaNative.dylib')) |
                    Should -BeExactly 'libAvaloniaNative.dylib'
                if ($script:RejectedProbe -ceq 'drag') { throw 'fixture drag rejected' }
            }
            $output = Join-Path $script:Work 'out'
            { Invoke-MacPackage -Binary $binary -Output $output -Arch arm64 `
                -RequirePopupGate ($Gate -ceq 'popup') -RequireTermination ($Gate -ceq 'termination') `
                -RequireFileDrag ($Gate -ceq 'drag') } | Should -Throw "*fixture $Kind rejected*"
            Test-Path -LiteralPath (Join-Path $output 'Jiffshot.app') | Should -BeFalse
            @(Get-ChildItem -LiteralPath $output -Filter '*.dmg').Count | Should -Be 0
        }

        It 'rejects a host ABI even when the file-drag companion is supported' {
            Mock Invoke-MacCommand {
                if ($Arguments[0] -ceq '--internal-check-native-file-drag') {
                    [pscustomobject]@{ ExitCode = 0; Stdout = "jiffshot-native-file-drag-v1: supported`n"; Stderr = '' }
                }
                else { [pscustomobject]@{ ExitCode = 0; Stdout = 'old host'; Stderr = '' } }
            }
            { Confirm-MacFileDrag '/fixture/jiffshot' '/fixture/host.dylib' } | Should -Throw '*file-drag host ABI*'
        }
    }
}

Describe 'macOS packaging entrypoint and CI' {
    BeforeAll {
        $script:Repository = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
        $script:PowerShell = (Get-Process -Id $PID).Path
    }

    It 'enforces the requested default capability gates in the entrypoint' -TestCases @(
        @{ Name = 'default'; Extra = @(); Checked = $true }
        @{ Name = 'required'; Extra = @('-RequireNativeFileDrag'); Checked = $true }
        @{ Name = 'local-debug'; Extra = @('-AllowUncheckedHost'); Checked = $false }
    ) {
        param($Name, $Extra, $Checked)
        $directory = Join-Path $TestDrive $Name
        [void][IO.Directory]::CreateDirectory($directory)
        [IO.File]::Copy((Join-Path $PSScriptRoot 'build.ps1'), (Join-Path $directory 'build.ps1'))
        $stub = @'
function Invoke-MacPackage {
    param($Binary, $Output, $Arch, $Identity, $Keychain, $NotaryProfile,
        [bool]$RequirePopupGate, [bool]$RequireTermination, [bool]$RequireFileDrag)
    [IO.File]::WriteAllText($env:BUILD_TEST_RESULT, (ConvertTo-Json -Compress -InputObject @{
        popup = $RequirePopupGate; termination = $RequireTermination; drag = $RequireFileDrag
    }))
}
Export-ModuleMember -Function Invoke-MacPackage
'@
        [IO.File]::WriteAllText((Join-Path $directory 'JiffshotMacPackage.psm1'), $stub)
        $environment = @{}
        foreach ($entry in [Environment]::GetEnvironmentVariables().GetEnumerator()) {
            $environment[[string]$entry.Key] = [string]$entry.Value
        }
        $environment['BUILD_TEST_RESULT'] = Join-Path $directory 'result.json'
        $result = Invoke-MacCommand -File $script:PowerShell -Arguments (
            @('-NoLogo', '-NoProfile', '-File', (Join-Path $directory 'build.ps1'),
                '-Binary', 'unused', '-Output', 'unused', '-Arch', 'arm64') + $Extra
        ) -Environment $environment
        $result.ExitCode | Should -Be 0
        $checks = ConvertFrom-Json -InputObject ([IO.File]::ReadAllText($environment['BUILD_TEST_RESULT'])) -AsHashtable
        $checks.popup | Should -Be $Checked
        $checks.termination | Should -Be $Checked
        $checks.drag | Should -Be $Checked
        $result = Invoke-MacCommand -File $script:PowerShell -Arguments @(
            '-NoLogo', '-NoProfile', '-File', (Join-Path $directory 'build.ps1'),
            '-Binary', 'unused', '-Output', 'unused', '-Arch', 'arm64',
            '-AllowUncheckedHost', '-RequireNativeFileDrag'
        ) -Environment $environment -AllowFailure
        $result.ExitCode | Should -Not -Be 0
        $result.Stderr | Should -Match 'cannot be combined'
    }

    It 'requires all host gates in macOS CI' {
        $workflow = [IO.File]::ReadAllText((Join-Path $script:Repository '.github/workflows/ci.yml'))
        $macos = [regex]::Match($workflow, '(?s)\n  macos:\n(.*?)\n  msi:\n').Groups[1].Value
        $packaging = [regex]::Match($macos, '(?s)- name: Package and verify(.*?)- uses: actions/upload-artifact').Groups[1].Value
        foreach ($flag in '-RequireNativePopupGate', '-RequireNativeTermination', '-RequireNativeFileDrag') {
            $packaging | Should -Match ([regex]::Escape($flag))
        }
        $packaging | Should -Not -Match 'AllowUncheckedHost'
        $packaging | Should -Match 'package/macos/build\.ps1'
    }

    It 'keeps the developer host fallback disabled only on macOS' {
        foreach ($target in @(
            @{ Triple = 'aarch64-apple-darwin'; Enabled = $false }
            @{ Triple = 'x86_64-apple-darwin'; Enabled = $false }
            @{ Triple = 'x86_64-pc-windows-msvc'; Enabled = $true }
        )) {
            $result = Invoke-MacCommand -File 'cargo' -WorkingDirectory $script:Repository -Arguments @(
                'tree', '--locked', '-p', 'jiffshot', '--target', $target.Triple,
                '-e', 'features', '-i', 'rustolonia-sys'
            ) -TimeoutSeconds 60
            $result.Stdout.Contains('feature "dev-host-path"') | Should -Be $target.Enabled
        }
    }

    It 'keeps automation and capture-device entitlements enabled' {
        $path = Join-Path $script:Repository 'package/macos/entitlements.plist'
        $result = Invoke-MacCommand -File '/usr/bin/plutil' -Arguments @('-convert', 'json', '-o', '-', $path)
        $entitlements = ConvertFrom-Json -InputObject $result.Stdout -AsHashtable
        foreach ($key in 'com.apple.security.automation.apple-events', 'com.apple.security.device.audio-input',
                         'com.apple.security.device.camera') {
            $entitlements[$key] | Should -BeTrue
        }
    }
}
