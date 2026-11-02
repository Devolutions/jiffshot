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
            Mock Invoke-MacCommand { [pscustomobject]@{ ExitCode = 137; Stdout = ''; Stderr = '' } }
            { Get-MacDiscoveredHost '/fixture/jiffshot' '/fixture' } |
                Should -Throw "*host discovery probe failed for '/fixture/jiffshot': exit status 137*"
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
                Mock Sign-MacPath {}
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
                Should -Invoke Sign-MacPath -Times 1 -Exactly -ParameterFilter {
                    $Path -ceq $unbundled -and $Identity -ceq '-' -and -not $Keychain -and -not $Entitlements
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
            Mock Sign-MacPath {}
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

        It 'fails before the isolated probe if the copied executable cannot be signed' {
            $binary = Join-Path $script:Work 'jiffshot'
            $hostPath = Join-Path $script:Work 'host.dylib'
            [IO.File]::WriteAllText($binary, 'fixture')
            [IO.File]::WriteAllText($hostPath, 'host')
            Mock Resolve-MacPhysicalPath { $Path }
            Mock Get-MacDiscoveredHost { $hostPath }
            Mock Sign-MacPath { throw 'fixture signing failed' }

            { Confirm-MacHostDiscovery $binary $hostPath $script:Work } | Should -Throw '*fixture signing failed*'
            Should -Invoke Get-MacDiscoveredHost -Times 1 -Exactly
            Should -Invoke Sign-MacPath -Times 1 -Exactly
        }

        It 'keeps the isolated copy of a signed bundle executable valid' {
            $app = Join-Path $script:Work 'Fixture.app'
            $macos = Join-Path $app 'Contents/MacOS'
            $frameworks = Join-Path $app 'Contents/Frameworks'
            [void][IO.Directory]::CreateDirectory($macos)
            [void][IO.Directory]::CreateDirectory($frameworks)
            $script:SignedFixtureBinary = Join-Path $macos 'jiffshot'
            $script:SignedFixtureHost = Join-Path $frameworks 'librustolonia_host.dylib'
            # Avoid inheriting signing metadata from an Apple system executable.
            $source = Join-Path $script:Work 'fixture.c'
            [IO.File]::WriteAllText($source, "int main(void) { return 0; }`n")
            Invoke-MacRun '/usr/bin/clang' @('-x', 'c', $source, '-o', $script:SignedFixtureBinary)
            Invoke-MacRun '/usr/bin/clang' @('-x', 'c', '-dynamiclib', $source, '-o', $script:SignedFixtureHost)
            Invoke-MacRun '/usr/bin/codesign' @('--force', '--sign', '-', $script:SignedFixtureHost)
            Write-MacPlist (Join-Path $app 'Contents/Info.plist') ([ordered]@{
                CFBundleIdentifier = 'com.devolutions.jiffshot.package-test'
                CFBundleExecutable = 'jiffshot'
                CFBundlePackageType = 'APPL'
            })
            Invoke-MacRun '/usr/bin/codesign' @('--force', '--sign', '-', '--options', 'runtime', $app)
            Invoke-MacRun '/usr/bin/codesign' @('--verify', '--deep', '--strict', $app)

            $script:VerifiedIsolatedCopy = $false
            Mock Get-MacDiscoveredHost {
                if ($Binary -ceq $script:SignedFixtureBinary) { return $script:SignedFixtureHost }
                [void](Invoke-MacCommand -File '/usr/bin/codesign' -Arguments @('--verify', '--strict', $Binary))
                [void](Invoke-MacCommand -File $Binary)
                $script:VerifiedIsolatedCopy = $true
            }
            Confirm-MacHostDiscovery $script:SignedFixtureBinary $script:SignedFixtureHost $script:Work

            $script:VerifiedIsolatedCopy | Should -BeTrue
            Invoke-MacRun '/usr/bin/codesign' @('--verify', '--deep', '--strict', $app)
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

        It 'prepares a checked, signed app without creating an image for external notarization' {
            $binary = Join-Path $script:Work 'jiffshot'
            foreach ($name in @('jiffshot', 'jiffshot-cli') + $script:HostLibraries) {
                [IO.File]::WriteAllText((Join-Path $script:Work $name), $name)
            }
            Mock Get-MacDeploymentTarget { '13.0' }
            Mock New-MacIcon {}
            Mock Sign-MacPath {}
            Mock New-MacDiskImage { throw 'unexpected image creation' }
            Mock Invoke-MacRun {
                if ($File -ceq '/usr/bin/ditto') { Copy-Item -LiteralPath $Arguments[0] -Destination $Arguments[1] -Recurse }
            }
            Mock Confirm-MacHostDiscovery {}
            Mock Confirm-MacPopupGate {}
            Mock Confirm-MacTermination {}
            Mock Confirm-MacFileDrag {}
            $output = Join-Path $script:Work 'out'

            { Invoke-MacPackage -Binary $binary -Output $output -Arch arm64 -PrepareForExternalNotarization } |
                Should -Throw '*requires a Developer ID identity*'
            { Invoke-MacPackage -Binary $binary -Output $output -Arch arm64 -Identity 'Developer ID Application: Example' `
                -NotaryProfile 'profile' -PrepareForExternalNotarization } | Should -Throw '*no local notary profile*'
            Invoke-MacPackage -Binary $binary -Output $output -Arch arm64 `
                -Identity 'Developer ID Application: Example' -PrepareForExternalNotarization

            Test-Path -LiteralPath (Join-Path $output 'Jiffshot.app/Contents/MacOS/jiffshot') | Should -BeTrue
            @(Get-ChildItem -LiteralPath $output -Filter '*.dmg').Count | Should -Be 0
            Should -Invoke New-MacDiskImage -Times 0 -Exactly
            Should -Invoke Confirm-MacHostDiscovery -Times 1 -Exactly
            Should -Invoke Confirm-MacPopupGate -Times 1 -Exactly
            Should -Invoke Confirm-MacTermination -Times 1 -Exactly
            Should -Invoke Confirm-MacFileDrag -Times 1 -Exactly
        }

        It 'preserves local notarization and stapling for the app and disk image' {
            $binary = Join-Path $script:Work 'jiffshot'
            foreach ($name in @('jiffshot', 'jiffshot-cli') + $script:HostLibraries) {
                [IO.File]::WriteAllText((Join-Path $script:Work $name), $name)
            }
            $script:NotaryEvents = [Collections.Generic.List[string]]::new()
            Mock Get-MacDeploymentTarget { '13.0' }
            Mock New-MacIcon {}
            Mock Sign-MacPath {}
            Mock Confirm-MacHostDiscovery {}
            Mock Confirm-MacPopupGate {}
            Mock Confirm-MacTermination {}
            Mock Confirm-MacFileDrag {}
            Mock Submit-MacNotarization {
                $Profile | Should -BeExactly 'fixture-profile'
                $Keychain | Should -BeExactly 'fixture-keychain'
                $script:NotaryEvents.Add($(if ($Path.EndsWith('.zip')) { 'submit-app' } else { 'submit-image' }))
            }
            Mock Invoke-MacRun {
                if ($File -ceq '/usr/bin/ditto' -and $Arguments[0] -ceq '-c') {
                    $script:NotaryEvents.Add('zip')
                    [IO.File]::WriteAllText($Arguments[-1], 'archive')
                }
                elseif ($File -ceq '/usr/bin/ditto') {
                    Copy-Item -LiteralPath $Arguments[0] -Destination $Arguments[1] -Recurse
                }
                elseif ($File -ceq '/usr/bin/hdiutil') {
                    $script:NotaryEvents.Add('image')
                    [IO.File]::WriteAllText($Arguments[-1], 'disk image')
                }
                elseif ($File -ceq '/usr/bin/xcrun') {
                    $script:NotaryEvents.Add($(if ($Arguments[-1].EndsWith('.app')) { 'staple-app' } else { 'staple-image' }))
                }
            }
            $output = Join-Path $script:Work 'out'
            Invoke-MacPackage -Binary $binary -Output $output -Arch arm64 -Identity 'Developer ID Application: Example' `
                -Keychain 'fixture-keychain' -NotaryProfile 'fixture-profile'

            $script:NotaryEvents.ToArray() | Should -Be @(
                'zip', 'submit-app', 'staple-app', 'image', 'submit-image', 'staple-image'
            )
            Test-Path -LiteralPath (Join-Path $output 'Jiffshot.app') | Should -BeTrue
            @(Get-ChildItem -LiteralPath $output -Filter '*.dmg').Count | Should -Be 1
        }

        It 'builds a signed disk image only from a validated, stapled app' {
            $app = Join-Path $script:Work 'Jiffshot.app'
            $macos = Join-Path $app 'Contents/MacOS'
            $frameworks = Join-Path $app 'Contents/Frameworks'
            [void][IO.Directory]::CreateDirectory($macos)
            [void][IO.Directory]::CreateDirectory($frameworks)
            foreach ($name in 'jiffshot', 'jiffshot-cli') {
                [IO.File]::WriteAllText((Join-Path $macos $name), $name)
            }
            foreach ($name in $script:HostLibraries) {
                [IO.File]::WriteAllText((Join-Path $frameworks $name), $name)
            }
            Write-MacPlist (Join-Path $app 'Contents/Info.plist') (Get-MacInfoPlist (Get-MacWorkspaceVersion))
            $script:ImageEvents = [Collections.Generic.List[string]]::new()
            Mock Get-MacDeploymentTarget { '13.0' }
            Mock Sign-MacPath { $script:ImageEvents.Add('sign') }
            Mock Invoke-MacRun {
                if ($File -ceq '/usr/bin/xcrun') { $script:ImageEvents.Add('validate') }
                elseif ($File -ceq '/usr/bin/hdiutil') {
                    $script:ImageEvents.Add('image')
                    [IO.File]::WriteAllText($Arguments[-1], 'disk image')
                }
                elseif ($File -ceq '/usr/bin/ditto') {
                    Copy-Item -LiteralPath $Arguments[0] -Destination $Arguments[1] -Recurse
                }
            }
            $output = Join-Path $script:Work 'out'
            Invoke-MacNotarizedDiskImage -App $app -Output $output -Arch x86_64 -Identity 'Developer ID Application: Example'

            $script:ImageEvents.ToArray() | Should -Be @('validate', 'image', 'sign')
            Test-Path -LiteralPath (Join-Path $output "Devolutions.Jiffshot-macos-x86_64-$(Get-MacWorkspaceVersion).dmg") |
                Should -BeTrue
            Should -Invoke Invoke-MacRun -Times 1 -Exactly -ParameterFilter {
                $File -ceq '/usr/bin/xcrun' -and ($Arguments -join '|') -ceq "stapler|validate|$app"
            }
            Should -Invoke Invoke-MacRun -Times 1 -Exactly -ParameterFilter {
                $File -ceq '/usr/bin/hdiutil' -and $Arguments[0] -ceq 'create' -and $Arguments -cnotcontains '-quiet'
            }
            Should -Invoke Sign-MacPath -Times 1 -Exactly -ParameterFilter {
                $Path.EndsWith('.dmg') -and $Identity -ceq 'Developer ID Application: Example' -and -not $Runtime
            }
        }

        It 'rejects an unstapled or mismatched app before creating an image' {
            $app = Join-Path $script:Work 'Jiffshot.app'
            $macos = Join-Path $app 'Contents/MacOS'
            $frameworks = Join-Path $app 'Contents/Frameworks'
            [void][IO.Directory]::CreateDirectory($macos)
            [void][IO.Directory]::CreateDirectory($frameworks)
            foreach ($name in 'jiffshot', 'jiffshot-cli') {
                [IO.File]::WriteAllText((Join-Path $macos $name), $name)
            }
            foreach ($name in $script:HostLibraries) {
                [IO.File]::WriteAllText((Join-Path $frameworks $name), $name)
            }
            $plist = Join-Path $app 'Contents/Info.plist'
            Write-MacPlist $plist (Get-MacInfoPlist '999.0.0')
            Mock Get-MacDeploymentTarget { '13.0' }
            Mock Invoke-MacRun {
                if ($File -ceq '/usr/bin/xcrun') { throw 'fixture app is not stapled' }
            }
            $output = Join-Path $script:Work 'out'
            { Invoke-MacNotarizedDiskImage -App $app -Output $output -Arch arm64 -Identity 'Developer ID Application: Example' } |
                Should -Throw '*Application version does not match*'
            Write-MacPlist $plist (Get-MacInfoPlist (Get-MacWorkspaceVersion))
            { Invoke-MacNotarizedDiskImage -App $app -Output $output -Arch arm64 -Identity 'Developer ID Application: Example' } |
                Should -Throw '*fixture app is not stapled*'
            Test-Path -LiteralPath $output | Should -BeFalse
            Should -Invoke Invoke-MacRun -Times 0 -Exactly -ParameterFilter { $File -ceq '/usr/bin/hdiutil' }
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

Describe 'macOS signing selection' {
    BeforeAll {
        $script:PowerShell = (Get-Process -Id $PID).Path
        $script:SigningSecrets = @(
            'APPLE_APP_DEV_ID_APP_CERTIFICATE', 'APPLE_APP_DEV_ID_APP_CERTIFICATE_PASSWORD',
            'APPLE_BOT_PASSWORD', 'DEVOLUTIONSBOT_TOKEN', 'APPLE_CERTIFICATE_BASE64',
            'APPLE_CERTIFICATE_PASSWORD', 'APPLE_DEVELOPER_ID', 'APPLE_ID', 'APPLE_TEAM_ID',
            'APPLE_APP_PASSWORD'
        )
    }

    It 'handles <Name> without exposing credential values' -TestCases @(
        @{ Name = 'an unsigned dry run'; Present = @(); Required = 'false'; Kind = 'none'; Enabled = 'false'; Notarize = 'false' }
        @{ Name = 'a token without Apple credentials'; Present = @('DEVOLUTIONSBOT_TOKEN'); Required = 'false'; Kind = 'none'; Enabled = 'false'; Notarize = 'false' }
        @{ Name = 'missing credentials for publication'; Present = @(); Required = 'true'; ExpectedError = 'require Apple signing and notarization' }
        @{ Name = 'an incomplete shared certificate'; Present = @('APPLE_APP_DEV_ID_APP_CERTIFICATE'); Required = 'false'; ExpectedError = 'APPLE_APP_DEV_ID_APP_CERTIFICATE_PASSWORD' }
        @{ Name = 'a missing private-actions token'; Present = @('APPLE_APP_DEV_ID_APP_CERTIFICATE', 'APPLE_APP_DEV_ID_APP_CERTIFICATE_PASSWORD'); Required = 'false'; ExpectedError = 'DEVOLUTIONSBOT_TOKEN' }
        @{ Name = 'shared signing without notarization'; Present = @('APPLE_APP_DEV_ID_APP_CERTIFICATE', 'APPLE_APP_DEV_ID_APP_CERTIFICATE_PASSWORD', 'DEVOLUTIONSBOT_TOKEN'); Required = 'false'; Kind = 'unigetui'; Enabled = 'true'; Notarize = 'false' }
        @{ Name = 'publication without a shared notary password'; Present = @('APPLE_APP_DEV_ID_APP_CERTIFICATE', 'APPLE_APP_DEV_ID_APP_CERTIFICATE_PASSWORD', 'DEVOLUTIONSBOT_TOKEN'); Required = 'true'; ExpectedError = 'require Apple signing and notarization' }
        @{ Name = 'shared signing and notarization'; Present = @('APPLE_APP_DEV_ID_APP_CERTIFICATE', 'APPLE_APP_DEV_ID_APP_CERTIFICATE_PASSWORD', 'APPLE_BOT_PASSWORD', 'DEVOLUTIONSBOT_TOKEN'); Required = 'true'; Kind = 'unigetui'; Enabled = 'true'; Notarize = 'true' }
        @{ Name = 'legacy signing without notarization'; Present = @('APPLE_CERTIFICATE_BASE64', 'APPLE_CERTIFICATE_PASSWORD', 'APPLE_DEVELOPER_ID'); Required = 'false'; Kind = 'legacy'; Enabled = 'true'; Notarize = 'false' }
        @{ Name = 'legacy signing and notarization'; Present = @('APPLE_CERTIFICATE_BASE64', 'APPLE_CERTIFICATE_PASSWORD', 'APPLE_DEVELOPER_ID', 'APPLE_ID', 'APPLE_TEAM_ID', 'APPLE_APP_PASSWORD'); Required = 'true'; Kind = 'legacy'; Enabled = 'true'; Notarize = 'true' }
        @{ Name = 'incomplete legacy notarization'; Present = @('APPLE_CERTIFICATE_BASE64', 'APPLE_CERTIFICATE_PASSWORD', 'APPLE_DEVELOPER_ID', 'APPLE_ID'); Required = 'false'; ExpectedError = 'Legacy notarization requires' }
        @{ Name = 'incomplete shared credentials alongside legacy credentials'; Present = @('APPLE_BOT_PASSWORD', 'APPLE_CERTIFICATE_BASE64', 'APPLE_CERTIFICATE_PASSWORD', 'APPLE_DEVELOPER_ID'); Required = 'false'; ExpectedError = 'APPLE_APP_DEV_ID_APP_CERTIFICATE_PASSWORD' }
        @{ Name = 'shared credentials taking precedence over legacy credentials'; Present = @('APPLE_APP_DEV_ID_APP_CERTIFICATE', 'APPLE_APP_DEV_ID_APP_CERTIFICATE_PASSWORD', 'APPLE_BOT_PASSWORD', 'DEVOLUTIONSBOT_TOKEN', 'APPLE_CERTIFICATE_BASE64'); Required = 'true'; Kind = 'unigetui'; Enabled = 'true'; Notarize = 'true' }
    ) {
        param($Name, $Present, $Required, $Kind, $Enabled, $Notarize, $ExpectedError)
        $environment = @{}
        foreach ($entry in [Environment]::GetEnvironmentVariables().GetEnumerator()) {
            $environment[[string]$entry.Key] = [string]$entry.Value
        }
        foreach ($name in $script:SigningSecrets) { $environment.Remove($name) }
        foreach ($name in $Present) { $environment[$name] = "fixture-value-$name" }
        $output = Join-Path $TestDrive ([guid]::NewGuid().ToString('N'))
        $environment['GITHUB_OUTPUT'] = $output
        $environment['REQUIRE_MACOS_NOTARIZATION'] = $Required
        $result = Invoke-MacCommand -File $script:PowerShell -Arguments @(
            '-NoLogo', '-NoProfile', '-File', (Join-Path $PSScriptRoot 'select-signing.ps1')
        ) -Environment $environment -AllowFailure

        if ($ExpectedError) {
            $result.ExitCode | Should -Not -Be 0
            $result.Stderr | Should -Match ([regex]::Escape($ExpectedError))
            Test-Path -LiteralPath $output | Should -BeFalse
        }
        else {
            $result.ExitCode | Should -Be 0
            [IO.File]::ReadAllLines($output) | Should -Be @("kind=$Kind", "enabled=$Enabled", "notarize=$Notarize")
        }
        foreach ($name in $Present) {
            ($result.Stdout + $result.Stderr) | Should -Not -Match ([regex]::Escape($environment[$name]))
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
        [bool]$RequirePopupGate, [bool]$RequireTermination, [bool]$RequireFileDrag,
        [switch]$PrepareForExternalNotarization)
    [IO.File]::WriteAllText($env:BUILD_TEST_RESULT, (ConvertTo-Json -Compress -InputObject @{
        popup = $RequirePopupGate; termination = $RequireTermination; drag = $RequireFileDrag
        external = $PrepareForExternalNotarization.IsPresent
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
        $checks.external | Should -BeFalse
        $result = Invoke-MacCommand -File $script:PowerShell -Arguments @(
            '-NoLogo', '-NoProfile', '-File', (Join-Path $directory 'build.ps1'),
            '-Binary', 'unused', '-Output', 'unused', '-Arch', 'arm64',
            '-AllowUncheckedHost', '-RequireNativeFileDrag'
        ) -Environment $environment -AllowFailure
        $result.ExitCode | Should -Not -Be 0
        $result.Stderr | Should -Match 'cannot be combined'
    }

    It 'passes external notarization preparation through the entrypoint' {
        $directory = Join-Path $TestDrive 'external-entrypoint'
        [void][IO.Directory]::CreateDirectory($directory)
        [IO.File]::Copy((Join-Path $PSScriptRoot 'build.ps1'), (Join-Path $directory 'build.ps1'))
        $stub = @'
function Invoke-MacPackage {
    param($Binary, $Output, $Arch, $Identity, $Keychain, $NotaryProfile,
        [bool]$RequirePopupGate, [bool]$RequireTermination, [bool]$RequireFileDrag,
        [switch]$PrepareForExternalNotarization)
    if (-not $PrepareForExternalNotarization) { throw 'external preparation was not passed' }
}
Export-ModuleMember -Function Invoke-MacPackage
'@
        [IO.File]::WriteAllText((Join-Path $directory 'JiffshotMacPackage.psm1'), $stub)
        $result = Invoke-MacCommand -File $script:PowerShell -Arguments @(
            '-NoLogo', '-NoProfile', '-File', (Join-Path $directory 'build.ps1'),
            '-Binary', 'unused', '-Output', 'unused', '-Arch', 'arm64', '-PrepareForExternalNotarization'
        )
        $result.ExitCode | Should -Be 0
    }

    It 'requires all host gates in macOS CI' {
        $workflow = [IO.File]::ReadAllText((Join-Path $script:Repository '.github/workflows/ci.yml'))
        $macos = [regex]::Match($workflow, '(?s)\r?\n  macos:\r?\n(.*?)\r?\n  msi:\r?\n').Groups[1].Value
        $packaging = [regex]::Match($macos, '(?s)- name: Package and verify(.*?)- uses: actions/upload-artifact').Groups[1].Value
        foreach ($flag in '-RequireNativePopupGate', '-RequireNativeTermination', '-RequireNativeFileDrag') {
            $packaging | Should -Match ([regex]::Escape($flag))
        }
        $packaging | Should -Not -Match 'AllowUncheckedHost'
        $packaging | Should -Match 'package/macos/build\.ps1'
    }

    It 'builds both macOS architectures and tests arm64 with the release host before recording its checksums' {
        $workflow = [IO.File]::ReadAllText((Join-Path $script:Repository '.github/workflows/ci.yml'))
        $macos = [regex]::Match($workflow, '(?s)\r?\n  macos:\r?\n(.*?)\r?\n  msi:\r?\n').Groups[1].Value
        $macos | Should -Match '(?m)^[ \t]*- arch: arm64\r?\n[ \t]+target: aarch64-apple-darwin\r?$'
        $macos | Should -Match '(?m)^[ \t]*- arch: x86_64\r?\n[ \t]+target: x86_64-apple-darwin\r?$'
        $build = [regex]::Match($macos, '- name: Build\r?\n\s+run: cargo build --release --locked -p jiffshot -p jiffshot-cli --target')
        $test = [regex]::Match($macos, "- name: Test\r?\n\s+if: matrix\.arch == 'arm64'\r?\n\s+run: \|\r?\n\s+cargo test --release --locked --workspace --target")
        $scriptTests = [regex]::Match($macos, "- name: Test macOS PowerShell scripts\r?\n\s+if: matrix\.arch == 'arm64'\r?\n\s+shell: pwsh")
        $build.Success | Should -BeTrue
        $test.Success | Should -BeTrue
        $scriptTests.Success | Should -BeTrue
        $build.Index | Should -BeLessThan $test.Index
        $test.Index | Should -BeLessThan $scriptTests.Index
        $macos | Should -Match 'RUSTOLONIA_HOST_LIB: \$\{\{ github\.workspace \}\}/target/\$\{\{ matrix\.target \}\}/release/librustolonia_host\.dylib'
        $macos | Should -Not -Match 'build-host\.ps1|RUSTOLONIA_HOST_DIR|\.ci/rustolonia'
        $macos | Should -Match 'shasum -a 256'
        foreach ($library in 'librustolonia_host.dylib', 'libAvaloniaNative.dylib', 'libSkiaSharp.dylib', 'libHarfBuzzSharp.dylib') {
            $macos | Should -Match ([regex]::Escape($library))
        }
        $macos | Should -Match 'macos-package/host-SHA256SUMS\.txt'
    }

    It 'uses the shared Apple secrets only for release signing and notarizes both artifacts' {
        $workflow = [IO.File]::ReadAllText((Join-Path $script:Repository '.github/workflows/ci.yml'))
        $macos = [regex]::Match($workflow, '(?s)\r?\n  macos:\r?\n(.*?)\r?\n  msi:\r?\n').Groups[1].Value
        $selection = [regex]::Match($macos, '(?s)- name: Select macOS signing(.*?)(?=\r?\n      - )').Groups[1].Value
        $selection | Should -Match "github\.event_name != 'pull_request' && inputs\.signing_environment != ''"
        foreach ($secret in 'APPLE_APP_DEV_ID_APP_CERTIFICATE', 'APPLE_APP_DEV_ID_APP_CERTIFICATE_PASSWORD',
                            'APPLE_BOT_PASSWORD', 'DEVOLUTIONSBOT_TOKEN') {
            $selection | Should -Match ([regex]::Escape("secrets.$secret"))
        }
        $macos | Should -Match 'repository: Devolutions/actions'
        $macos | Should -Match 'persist-credentials: false'
        $macos | Should -Match 'macos-code-sign-setup'
        $macos | Should -Match 'security set-keychain-settings -lut 21600 build\.keychain'
        $macos | Should -Match 'rm -f -- certificate\.p12'
        $macos | Should -Match 'steps\.signing\.outputs\.kind == ''legacy'''
        $macos.IndexOf('- name: Notarize the macOS app') | Should -BeLessThan $macos.IndexOf('- name: Build disk image from notarized app')
        $macos.IndexOf('- name: Build disk image from notarized app') | Should -BeLessThan $macos.IndexOf('- name: Notarize the macOS disk image')
        $macos.IndexOf('- name: Notarize the macOS disk image') | Should -BeLessThan $macos.IndexOf('- name: Verify macOS package')
        $macos | Should -Match 'skip_checksum_validation: ''true'''
        $macos | Should -Match 'skip_stapling: ''true'''
        $macos | Should -Match 'stapler staple macos-package/Jiffshot\.app'
        $macos | Should -Match 'stapler validate macos-package/Jiffshot\.app'
        $macos | Should -Match 'stapler validate "\$\{images\[0\]\}"'
        $macos | Should -Match 'codesign --verify --deep --strict "\$mountpoint/Jiffshot\.app"'
        $macos | Should -Match 'stapler validate "\$mountpoint/Jiffshot\.app"'
        $release = [IO.File]::ReadAllText((Join-Path $script:Repository '.github/workflows/release.yml'))
        $release | Should -Match 'require_macos_notarization: \$\{\{ !inputs\.dry_run \}\}'
        $release | Should -Match '\$env:DRY_RUN -ne ''true'' -and \$status -ne ''notarized'''
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
