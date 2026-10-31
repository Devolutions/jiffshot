#Requires -Version 7.4
# Architecture dispatch and release regression tests; no real ARM execution.

BeforeAll {
    . (Join-Path $PSScriptRoot 'TestHelpers.ps1')
    Import-Module $script:PackageModule -Force

    function New-TestInputs([string]$Work, [int]$Machine) {
        $directory = Join-Path $Work "release-$Machine"
        [void][System.IO.Directory]::CreateDirectory($directory)
        foreach ($name in $script:RuntimeFiles) {
            [System.IO.File]::WriteAllBytes((Join-Path $directory $name), (Join-Bytes (New-ElfHeader $Machine) (Get-AsciiBytes 'debug data')))
        }
        $directory
    }

    function Get-Architecture([string]$Name) {
        Get-JiffshotLinuxArchitecture -Name $Name
    }
}

Describe 'ELF architecture' {
    BeforeEach {
        $script:Work = New-TestDirectory
        Mock -ModuleName JiffshotLinuxPackage Test-JiffshotLinuxHost { $true }
    }

    AfterEach {
        Remove-Item -LiteralPath $script:Work -Recurse -Force -ErrorAction Ignore
    }

    It 'supported ELF headers dispatch (<Arch>, ABI <Abi>, type <ElfType>)' -ForEach @(
        foreach ($row in @(@('x64', 62, 'amd64', 'x86_64'), @('arm64', 183, 'arm64', 'aarch64'))) {
            foreach ($abi in 0, 3) {
                foreach ($elfType in 2, 3) {
                    @{ Arch = $row[0]; Machine = $row[1]; Deb = $row[2]; Rpm = $row[3]; Abi = $abi; ElfType = $elfType }
                }
            }
        }
    ) {
        $binary = Join-Path $script:Work 'binary'
        $header = New-ElfHeader $Machine $ElfType
        $header[7] = $Abi
        [System.IO.File]::WriteAllBytes($binary, $header)
        $actual = Confirm-JiffshotElfBinary -Path $binary
        @($actual.Name, $actual.Machine, $actual.Deb, $actual.Rpm) | Should -BeExactly @($Arch, $Machine, $Deb, $Rpm)
    }

    It 'invalid ELF header <Label> is rejected' -ForEach @(
        @{ Label = 'empty'; Length = 0; Offset = -1; Value = 0; ExpectedError = 'ELF' }
        @{ Label = 'truncated_ident'; Length = 6; Offset = -1; Value = 0; ExpectedError = 'ELF' }
        @{ Label = 'truncated_machine'; Length = 19; Offset = -1; Value = 0; ExpectedError = 'ELF' }
        @{ Label = 'truncated_header'; Length = 63; Offset = -1; Value = 0; ExpectedError = 'ELF' }
        @{ Label = 'magic'; Length = 64; Offset = 0; Value = 0; ExpectedError = 'ELF' }
        @{ Label = 'elf32'; Length = 64; Offset = 4; Value = 1; ExpectedError = 'ELF' }
        @{ Label = 'big_endian'; Length = 64; Offset = 5; Value = 2; ExpectedError = 'ELF' }
        @{ Label = 'ident_version'; Length = 64; Offset = 6; Value = 2; ExpectedError = 'version or Linux ABI' }
        @{ Label = 'freebsd'; Length = 64; Offset = 7; Value = 9; ExpectedError = 'version or Linux ABI' }
        @{ Label = 'abi_version'; Length = 64; Offset = 8; Value = 1; ExpectedError = 'version or Linux ABI' }
        @{ Label = 'object'; Length = 64; Offset = 16; Value = 1; ExpectedError = 'executable or shared object' }
        @{ Label = 'core'; Length = 64; Offset = 16; Value = 4; ExpectedError = 'executable or shared object' }
        @{ Label = 'unknown_machine'; Length = 64; Offset = 18; Value = 0; ExpectedError = 'unsupported ELF machine' }
        @{ Label = 'arm32'; Length = 64; Offset = 18; Value = 40; ExpectedError = 'unsupported ELF machine 40' }
        @{ Label = 'x86_32'; Length = 64; Offset = 18; Value = 3; ExpectedError = 'unsupported ELF machine 3' }
        @{ Label = 'header_version'; Length = 64; Offset = 20; Value = 2; ExpectedError = 'version or Linux ABI' }
        @{ Label = 'small_header'; Length = 64; Offset = 52; Value = 63; ExpectedError = 'header size' }
        @{ Label = 'large_header'; Length = 64; Offset = 52; Value = 65; ExpectedError = 'header size' }
    ) {
        $binary = Join-Path $script:Work 'invalid'
        $header = New-ElfHeader
        if ($Offset -ge 0) { $header[$Offset] = $Value }
        $data = [byte[]]::new(0)
        if ($Length -gt 0) { $data = [byte[]]$header[0..($Length - 1)] }
        [System.IO.File]::WriteAllBytes($binary, $data)
        [void](Assert-ThrowsMatch { Confirm-JiffshotElfBinary -Path $binary } "invalid.*$ExpectedError")
    }

    It 'every runtime library must match the <Expected> executable (<Name>)' -ForEach @(
        foreach ($row in @(@(62, 183, 'x64', 'arm64'), @(183, 62, 'arm64', 'x64'))) {
            foreach ($name in 'librustolonia_host.so', 'libSkiaSharp.so', 'libHarfBuzzSharp.so') {
                @{ Machine = $row[0]; Other = $row[1]; Expected = $row[2]; Actual = $row[3]; Name = $name }
            }
        }
    ) {
        $inputs = New-TestInputs $script:Work $Machine
        [System.IO.File]::WriteAllBytes((Join-Path $inputs $Name), (New-ElfHeader $Other))
        [void](Assert-ThrowsMatch { Confirm-JiffshotBuild -BuildDir $inputs } "$([regex]::Escape($Name)).*Linux $Actual, expected Linux $Expected")
        Mock -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand { }
        [void](Assert-ThrowsMatch { New-JiffshotPayload -BuildDir $inputs -Payload (Join-Path $script:Work 'payload') } '')
        Should -Invoke -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand -Times 0 -Exactly
        Test-Path -LiteralPath (Join-Path $script:Work 'payload') | Should -BeFalse
    }

    It 'libraries must be shared objects (machine <Machine>, <Name>)' -ForEach @(
        foreach ($machine in 62, 183) {
            foreach ($name in 'librustolonia_host.so', 'libSkiaSharp.so', 'libHarfBuzzSharp.so') {
                @{ Machine = $machine; Name = $name }
            }
        }
    ) {
        $inputs = New-TestInputs $script:Work $Machine
        [System.IO.File]::WriteAllBytes((Join-Path $inputs $Name), (New-ElfHeader $Machine 2))
        [void](Assert-ThrowsMatch { Confirm-JiffshotBuild -BuildDir $inputs } "$([regex]::Escape($Name)).*shared object")
    }

    It 'explicit <Arch> architecture is an assertion' -ForEach @(
        @{ Arch = 'x64'; Machine = 62; Other = 'arm64' }
        @{ Arch = 'arm64'; Machine = 183; Other = 'x64' }
    ) {
        $inputs = New-TestInputs $script:Work $Machine
        (Confirm-JiffshotBuild -BuildDir $inputs -Arch $Arch).Name | Should -BeExactly $Arch
        [void](Assert-ThrowsMatch { Confirm-JiffshotBuild -BuildDir $inputs -Arch $Other } "Linux $Arch, expected Linux $Other")
        [void](Assert-ThrowsMatch { Confirm-JiffshotBuild -BuildDir $inputs -Arch 'mips' } 'unsupported architecture assertion')
    }

    It 'missing runtime <Name> is rejected' -ForEach @(
        foreach ($name in 'jiffshot', 'librustolonia_host.so', 'libSkiaSharp.so', 'libHarfBuzzSharp.so') { @{ Name = $name } }
    ) {
        $inputs = New-TestInputs $script:Work 62
        Remove-Item -LiteralPath (Join-Path $inputs $Name)
        $failure = Assert-ThrowsMatch { Confirm-JiffshotBuild -BuildDir $inputs } '' ([System.IO.FileNotFoundException])
        $failure.Exception.FileName | Should -BeExactly (Join-Path $inputs $Name)
    }

    It 'CLI rejects <Case> inputs before tools or publication' -ForEach @(
        @{ Case = 'assertion' }, @{ Case = 'mixed' }, @{ Case = 'unsupported' }, @{ Case = 'missing' }
    ) {
        $inputs = New-TestInputs $script:Work 62
        $output = Join-Path $script:Work 'out'
        $asserted = if ($Case -eq 'assertion') { 'arm64' } else { 'x64' }
        switch ($Case) {
            'mixed' { [System.IO.File]::WriteAllBytes((Join-Path $inputs 'libSkiaSharp.so'), (New-ElfHeader 183)) }
            'unsupported' { [System.IO.File]::WriteAllBytes((Join-Path $inputs 'jiffshot'), (New-ElfHeader 40)) }
            'missing' { Remove-Item -LiteralPath (Join-Path $inputs 'libHarfBuzzSharp.so') }
        }
        Mock -ModuleName JiffshotLinuxPackage Confirm-JiffshotPackagingTool { }
        [void](Assert-ThrowsMatch { Invoke-JiffshotLinuxPackageBuild -Arch $asserted -BuildDir $inputs -OutputDir $output -Format tar.gz } '' `
            ([System.IO.InvalidDataException], [System.IO.FileNotFoundException]))
        Should -Invoke -ModuleName JiffshotLinuxPackage Confirm-JiffshotPackagingTool -Times 0 -Exactly
        Test-Path -LiteralPath $output | Should -BeFalse
    }

    It 'CLI failure has a non-zero exit and a file-specific error' {
        $inputs = New-TestInputs $script:Work 62
        $output = Join-Path $script:Work 'out'
        $result = Invoke-TestScript (Join-Path $script:Assets 'build.ps1') @(
            '-Arch', 'arm64', '-BuildDir', $inputs, '-OutputDir', $output, '-Format', 'tar.gz')
        $result.ExitCode | Should -Be 1
        $result.Stderr | Should -BeLikeExactly '*Linux packaging failed:*'
        $result.Stderr | Should -BeLikeExactly '*jiffshot is Linux x64, expected Linux arm64*'
        Test-Path -LiteralPath $output | Should -BeFalse
    }

    It 'CLI rejects unsupported <Option> values' -ForEach @(
        @{ Option = '-Arch'; Value = 'mips' }, @{ Option = '-Format'; Value = 'zip' }
    ) {
        $output = Join-Path $script:Work 'out'
        $result = Invoke-TestScript (Join-Path $script:Assets 'build.ps1') @($Option, $Value, '-OutputDir', $output)
        # PowerShell parameter validation exits 1 where argparse exited 2.
        $result.ExitCode | Should -Be 1
        $result.Stderr | Should -BeLikeExactly '*does not belong to the set*'
        Test-Path -LiteralPath $output | Should -BeFalse
    }

    It 'installed validator checks all runtime libraries' {
        $inputs = New-TestInputs $script:Work 183
        $validator = Join-Path $script:Assets 'validate-installed.ps1'
        $result = Invoke-TestScript $validator @('-Arch', 'arm64', '-RuntimeDir', $inputs)
        $result.ExitCode | Should -Be 0
        $result.Stdout | Should -BeExactly "Installed runtime: Linux arm64`n"
        [System.IO.File]::WriteAllBytes((Join-Path $inputs 'libHarfBuzzSharp.so'), (New-ElfHeader 62))
        $result = Invoke-TestScript $validator @('-Arch', 'arm64', '-RuntimeDir', $inputs)
        $result.ExitCode | Should -Not -Be 0
        $result.Stdout | Should -BeExactly ''
        $result.Stderr | Should -MatchExactly '(?s)libHarfBuzzSharp\.so.*expected Linux arm64'
    }
}

Describe 'Architecture packaging' {
    BeforeEach {
        $script:Work = New-TestDirectory
        Mock -ModuleName JiffshotLinuxPackage Test-JiffshotLinuxHost { $true }
    }

    AfterEach {
        Remove-Item -LiteralPath $script:Work -Recurse -Force -ErrorAction Ignore
    }

    It 'tar publication uses the detected <Arch> architecture and checksums' -ForEach @(
        @{ Arch = 'x64'; Machine = 62 }, @{ Arch = 'arm64'; Machine = 183 }
    ) {
        $inputs = New-TestInputs $script:Work $Machine
        $output = Join-Path $script:Work $Arch
        $content = Join-Bytes (New-ElfHeader $Machine) (Get-AsciiBytes 'debug data')
        Mock -ModuleName JiffshotLinuxPackage Confirm-JiffshotPackagingTool { }
        Mock -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand { }
        $previous = $env:SOURCE_DATE_EPOCH
        try {
            $env:SOURCE_DATE_EPOCH = '123'
            [void](Invoke-JiffshotLinuxPackageBuild -BuildDir $inputs -OutputDir $output -Format tar.gz, tar.gz)
        }
        finally {
            $env:SOURCE_DATE_EPOCH = $previous
        }
        $version = Get-JiffshotPackageVersion
        $name = "Devolutions.Jiffshot-linux-$Arch-$version.tar.gz"
        @([System.IO.Directory]::GetFileSystemEntries($output) | ForEach-Object { [System.IO.Path]::GetFileName($_) } | Sort-Object -CaseSensitive) |
            Should -BeExactly @(@($name, "SHA256SUMS-linux-$Arch.txt") | Sort-Object -CaseSensitive)
        $artifact = Join-Path $output $name
        [System.IO.File]::ReadAllText((Join-Path $output "SHA256SUMS-linux-$Arch.txt")) | Should -BeExactly "$(Get-TestSha256 $artifact)  $name`n"
        $members = @(Read-TarGz $artifact -Content)
        $prefix = "jiffshot-$version-linux-$Arch"
        $members[0].Name | Should -BeExactly $prefix
        foreach ($runtime in $script:RuntimeFiles) {
            ($members | Where-Object Name -CEQ "$prefix/lib/jiffshot/$runtime").Data | Should -Be $content
            [System.IO.File]::ReadAllBytes((Join-Path $inputs $runtime)) | Should -Be $content
        }
    }

    It 'DEB metadata and filename follow the <Arch> ELF architecture' -ForEach @(
        @{ Arch = 'x64'; Machine = 62; Deb = 'amd64' }, @{ Arch = 'arm64'; Machine = 183; Deb = 'arm64' }
    ) {
        $work = Join-Path $script:Work $Arch
        [void][System.IO.Directory]::CreateDirectory($work)
        $payload = Join-Path $work 'payload'
        Mock -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand { }
        New-JiffshotPayload -BuildDir (New-TestInputs $script:Work $Machine) -Payload $payload
        $calls = [System.Collections.Generic.List[object]]::new()
        Mock -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand {
            $calls.Add([string[]]$Command)
            [pscustomobject]@{ Stdout = "shlibs:Depends=libc6 (>= 2.39)`n" }
        }
        $destination = New-JiffshotDeb -Payload $payload -Work $work -Version '1.2.3' -Architecture (Confirm-JiffshotBuild -BuildDir (New-TestInputs $script:Work $Machine))
        [System.IO.Path]::GetFileName($destination) | Should -BeExactly "jiffshot_1.2.3_$Deb.deb"
        $control = [System.IO.File]::ReadAllText((Join-Path $work 'deb/DEBIAN/control'))
        $control | Should -BeLikeExactly "*`nArchitecture: $Deb`n*"
        [System.IO.File]::ReadAllText((Join-Path $work 'metadata/debian/control')) | Should -BeLikeExactly "*`nArchitecture: $Deb`n*"
        $control | Should -BeLikeExactly '*Depends: libc6 (>= 2.39), libx11-6*'
        $calls[0] | Should -BeExactly (@('dpkg-shlibdeps', '-O') + @($script:RuntimeFiles | ForEach-Object { "-e$(Join-Path $work "deb/usr/lib/jiffshot/$_")" }))
        $calls[-1][-1] | Should -BeExactly $destination
    }

    It 'DEB rejects invalid dependency metadata' {
        $payload = Join-Path $script:Work 'payload'
        Mock -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand { }
        New-JiffshotPayload -BuildDir (New-TestInputs $script:Work 62) -Payload $payload
        $calls = [System.Collections.Generic.List[object]]::new()
        Mock -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand { $calls.Add([string[]]$Command); [pscustomobject]@{ Stdout = 'not shlibs metadata' } }
        [void](Assert-ThrowsMatch { New-JiffshotDeb -Payload $payload -Work $script:Work -Version '1.2.3' -Architecture (Get-Architecture x64) } 'unexpected dependency metadata')
        $calls.Count | Should -Be 1
        Test-Path -LiteralPath (Join-Path $script:Work 'deb/DEBIAN') | Should -BeFalse
    }

    It 'stripped runtime is revalidated before publication' {
        $inputs = New-TestInputs $script:Work 183
        $output = Join-Path $script:Work 'out'
        Mock -ModuleName JiffshotLinuxPackage Confirm-JiffshotPackagingTool { }
        Mock -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand {
            if ($Command[0] -ceq 'strip') {
                [System.IO.File]::WriteAllBytes($Command[-1], (New-ElfHeader 62))
            }
        }
        [void](Assert-ThrowsMatch { Invoke-JiffshotLinuxPackageBuild -BuildDir $inputs -OutputDir $output -Format tar.gz } 'expected Linux arm64')
        Test-Path -LiteralPath $output | Should -BeFalse
        [System.IO.File]::ReadAllBytes((Join-Path $inputs 'jiffshot')) | Should -Be (Join-Bytes (New-ElfHeader 183) (Get-AsciiBytes 'debug data'))
    }

    It 'SOURCE_DATE_EPOCH <Label> is <Outcome>' -ForEach @(
        @{ Label = 'zero'; Epoch = '0'; Accepted = $true; Outcome = 'accepted' }
        @{ Label = 'maximum'; Epoch = '4294967295'; Accepted = $true; Outcome = 'accepted' }
        @{ Label = 'negative'; Epoch = '-1'; Accepted = $false; Outcome = 'rejected' }
        @{ Label = 'overflow'; Epoch = '4294967296'; Accepted = $false; Outcome = 'rejected' }
        @{ Label = 'not_integer'; Epoch = 'yesterday'; Accepted = $false; Outcome = 'rejected' }
    ) {
        $inputs = New-TestInputs $script:Work 62
        $output = Join-Path $script:Work $Label
        Mock -ModuleName JiffshotLinuxPackage Confirm-JiffshotPackagingTool { }
        Mock -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand { }
        $previous = $env:SOURCE_DATE_EPOCH
        try {
            $env:SOURCE_DATE_EPOCH = $Epoch
            if ($Accepted) {
                [void](Invoke-JiffshotLinuxPackageBuild -BuildDir $inputs -OutputDir $output -Format tar.gz)
            }
            else {
                [void](Assert-ThrowsMatch { Invoke-JiffshotLinuxPackageBuild -BuildDir $inputs -OutputDir $output -Format tar.gz } '')
            }
        }
        finally {
            $env:SOURCE_DATE_EPOCH = $previous
        }
        if ($Accepted) {
            $artifacts = @([System.IO.Directory]::GetFiles($output, '*.tar.gz'))
            $artifacts.Count | Should -Be 1
            [System.BitConverter]::ToUInt32([System.IO.File]::ReadAllBytes($artifacts[0]), 4) | Should -Be ([uint32]$Epoch)
        }
        else {
            Test-Path -LiteralPath $output | Should -BeFalse
        }
    }

    It 'RPM target, spec, and filename follow the <Arch> ELF architecture' -ForEach @(
        @{ Arch = 'x64'; Machine = 62; Rpm = 'x86_64' }, @{ Arch = 'arm64'; Machine = 183; Rpm = 'aarch64' }
    ) {
        $work = Join-Path $script:Work $Arch
        [void][System.IO.Directory]::CreateDirectory($work)
        $payload = Join-Path $work 'payload'
        Mock -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand { }
        New-JiffshotPayload -BuildDir (New-TestInputs $script:Work $Machine) -Payload $payload
        Mock -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand {
            $Command[0..3] | Should -BeExactly @('rpmbuild', '-bb', '--target', $Rpm)
            $spec = [System.IO.File]::ReadAllText($Command[-1])
            $spec | Should -BeLikeExactly "*BuildArch:      $Rpm`n*"
            $spec | Should -BeLikeExactly "*Version:        1.2.3`n*"
            $spec | Should -Not -BeLikeExactly '*@ARCH@*'
            $destination = Join-Path $work "rpm/RPMS/$Rpm/jiffshot-1.2.3-1.$Rpm.rpm"
            [void][System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($destination))
            [System.IO.File]::WriteAllBytes($destination, (Get-AsciiBytes 'rpm output'))
        }
        $artifact = New-JiffshotRpm -Payload $payload -Work $work -Version '1.2.3' -Epoch 123 -Architecture (Confirm-JiffshotBuild -BuildDir (New-TestInputs $script:Work $Machine))
        [System.IO.Path]::GetRelativePath($work, $artifact).Replace('\', '/') | Should -BeExactly "rpm/RPMS/$Rpm/jiffshot-1.2.3-1.$Rpm.rpm"
        [System.IO.File]::ReadAllBytes($artifact) | Should -Be (Get-AsciiBytes 'rpm output')
        Should -Invoke -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand -Times 1 -Exactly -ParameterFilter { $Command[0] -ceq 'rpmbuild' }
    }

    It 'RPM rejects a missing target output' {
        $payload = Join-Path $script:Work 'payload'
        Mock -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand { }
        New-JiffshotPayload -BuildDir (New-TestInputs $script:Work 183) -Payload $payload
        [void](Assert-ThrowsMatch { New-JiffshotRpm -Payload $payload -Work $script:Work -Version '1.2.3' -Epoch 0 -Architecture (Get-Architecture arm64) } 'did not produce jiffshot-1.2.3-1.aarch64.rpm')
    }

    It 'RPM ABI guard rejects <Label> OCR linkage' -ForEach @(
        @{ Label = 'missing'; Needed = '' }
        @{ Label = 'ubuntu'; Needed = '[liblept.so.5]' }
        @{ Label = 'mixed'; Needed = "[liblept.so.5]`n[libleptonica.so.6]" }
    ) {
        Mock -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand { [pscustomobject]@{ Stdout = $Needed } }
        [void](Assert-ThrowsMatch { Confirm-JiffshotRpmInput -BuildDir '/release' } 'native Fedora build linked to libleptonica.so.6')
    }

    It 'RPM ABI guard accepts the Fedora OCR linkage' {
        Mock -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand { [pscustomobject]@{ Stdout = '[libleptonica.so.6]' } }
        Confirm-JiffshotRpmInput -BuildDir '/release'
        Should -Invoke -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand -Times 1 -Exactly -ParameterFilter {
            ($Command -join "`0") -ceq (@('readelf', '--dynamic', '/release/jiffshot') -join "`0")
        }
    }
}

Describe 'Release assembly' {
    BeforeEach {
        $script:Work = New-TestDirectory
        $script:Version = Get-JiffshotPackageVersion
        $script:Artifacts = Join-Path $script:Work 'artifacts'
        $script:Output = Join-Path $script:Work 'release'
        $script:Packages = [ordered]@{}
        $script:Manifests = [System.Collections.Generic.List[string]]::new()
        foreach ($row in @(@('x64', 'amd64', 'x86_64'), @('arm64', 'arm64', 'aarch64'))) {
            $arch, $deb, $rpm = $row
            foreach ($group in @(
                    @('', @("Devolutions.Jiffshot-linux-$arch-$($script:Version).tar.gz", "jiffshot_$($script:Version)_$deb.deb")),
                    @('-rpm', @("jiffshot-$($script:Version)-1.$rpm.rpm")))) {
                $directory = Join-Path $script:Artifacts "jiffshot-linux-$arch$($group[0])"
                [void][System.IO.Directory]::CreateDirectory($directory)
                $lines = foreach ($name in $group[1]) {
                    $content = Get-AsciiBytes "independent fixture $name"
                    [System.IO.File]::WriteAllBytes((Join-Path $directory $name), $content)
                    $script:Packages[$name] = @((Join-Path $directory $name), $content)
                    "$(Get-BytesSha256 $content)  $name`n"
                }
                $manifest = Join-Path $directory "SHA256SUMS-linux-$arch.txt"
                [System.IO.File]::WriteAllText($manifest, (-join $lines), [System.Text.Encoding]::ASCII)
                $script:Manifests.Add($manifest)
            }
        }
    }

    AfterEach {
        Remove-Item -LiteralPath $script:Work -Recurse -Force -ErrorAction Ignore
    }

    It 'assembly requires six verified packages' {
        [void][System.IO.Directory]::CreateDirectory($script:Output)
        [System.IO.File]::WriteAllBytes((Join-Path $script:Output 'windows.msi'), (Get-AsciiBytes 'leave Windows assets alone'))
        $actual = @(Publish-JiffshotLinuxRelease -ArtifactDir $script:Artifacts -OutputDir $script:Output -Version $script:Version)
        $actual.Count | Should -Be 6
        @($actual | Sort-Object -CaseSensitive) | Should -BeExactly @($script:Packages.Keys | Sort-Object -CaseSensitive)
        foreach ($name in $script:Packages.Keys) {
            [System.IO.File]::ReadAllBytes((Join-Path $script:Output $name)) | Should -Be $script:Packages[$name][1]
        }
        foreach ($arch in 'x64', 'arm64') {
            $manifest = Join-Path $script:Output "SHA256SUMS-linux-$arch.txt"
            $debSuffix = if ($arch -eq 'x64') { '_amd64.deb' } else { '_arm64.deb' }
            $rpmSuffix = if ($arch -eq 'x64') { '.x86_64.rpm' } else { '.aarch64.rpm' }
            $names = @($script:Packages.Keys | Where-Object {
                    $_.Contains("linux-$arch-") -or $_.EndsWith($debSuffix) -or $_.EndsWith($rpmSuffix)
                })
            $records = Read-JiffshotChecksumManifest -Path $manifest -ExpectedName $names
            $records.Count | Should -Be 3
            foreach ($name in $names) {
                $records[$name] | Should -BeExactly (Get-BytesSha256 $script:Packages[$name][1])
            }
        }
        [System.IO.File]::ReadAllBytes((Join-Path $script:Output 'windows.msi')) | Should -Be (Get-AsciiBytes 'leave Windows assets alone')
    }

    It 'each missing package blocks assembly without partial output' {
        foreach ($name in $script:Packages.Keys) {
            $path, $content = $script:Packages[$name]
            Remove-Item -LiteralPath $path
            $failure = Assert-ThrowsMatch { Publish-JiffshotLinuxRelease -ArtifactDir $script:Artifacts -OutputDir $script:Output -Version $script:Version } '' ([System.IO.FileNotFoundException])
            $failure.Exception.FileName | Should -BeExactly $path -Because $name
            Test-Path -LiteralPath $script:Output | Should -BeFalse
            [System.IO.File]::WriteAllBytes($path, $content)
        }
    }

    It 'each missing manifest blocks assembly' {
        foreach ($path in $script:Manifests) {
            $content = [System.IO.File]::ReadAllBytes($path)
            Remove-Item -LiteralPath $path
            [void](Assert-ThrowsMatch { Publish-JiffshotLinuxRelease -ArtifactDir $script:Artifacts -OutputDir $script:Output -Version $script:Version } '' ([System.IO.FileNotFoundException]))
            Test-Path -LiteralPath $script:Output | Should -BeFalse
            [System.IO.File]::WriteAllBytes($path, $content)
        }
    }

    It 'a corrupt package blocks assembly before copying' {
        foreach ($name in $script:Packages.Keys) {
            $path, $content = $script:Packages[$name]
            [System.IO.File]::WriteAllBytes($path, (Join-Bytes $content (Get-AsciiBytes 'corruption')))
            [void](Assert-ThrowsMatch { Publish-JiffshotLinuxRelease -ArtifactDir $script:Artifacts -OutputDir $script:Output -Version $script:Version } 'checksum mismatch')
            Test-Path -LiteralPath $script:Output | Should -BeFalse
            [System.IO.File]::WriteAllBytes($path, $content)
        }
    }

    It 'malformed checksum record <Label> is rejected' -ForEach @(
        @{ Label = 'duplicate'; ExpectedError = 'duplicate checksum' }
        @{ Label = 'missing_record'; ExpectedError = 'must cover exactly' }
        @{ Label = 'wrong_arch'; ExpectedError = 'must cover exactly' }
        @{ Label = 'extra_record'; ExpectedError = 'must cover exactly' }
        @{ Label = 'short_digest'; ExpectedError = 'invalid checksum record' }
        @{ Label = 'path_traversal'; ExpectedError = 'invalid checksum record' }
        @{ Label = 'wrong_separator'; ExpectedError = 'invalid checksum record' }
    ) {
        $manifest = $script:Manifests[-1]
        $original = [System.IO.File]::ReadAllText($manifest)
        $digest, $name = $original.Trim().Split('  ', 2)
        $content = switch ($Label) {
            'duplicate' { $original * 2 }
            'missing_record' { '' }
            'wrong_arch' { $original.Replace('.aarch64.rpm', '.x86_64.rpm') }
            'extra_record' { $original + "$digest  unexpected.deb`n" }
            'short_digest' { "bad  $name`n" }
            'path_traversal' { "$digest  ../$name`n" }
            'wrong_separator' { "$digest $name`n" }
        }
        [System.IO.File]::WriteAllText($manifest, $content)
        [void](Assert-ThrowsMatch { Publish-JiffshotLinuxRelease -ArtifactDir $script:Artifacts -OutputDir $script:Output -Version $script:Version } $ExpectedError)
        Test-Path -LiteralPath $script:Output | Should -BeFalse
    }

    It 'a version mismatch blocks assembly' {
        [void](Assert-ThrowsMatch { Publish-JiffshotLinuxRelease -ArtifactDir $script:Artifacts -OutputDir $script:Output -Version '999.0.0' } 'does not match Cargo.toml')
        Test-Path -LiteralPath $script:Output | Should -BeFalse
    }

    It 'the release CLI publishes all six packages' {
        $result = Invoke-TestScript (Join-Path $script:Assets 'assemble-release.ps1') @(
            '-ArtifactDir', $script:Artifacts, '-OutputDir', $script:Output, '-Version', $script:Version)
        $result.ExitCode | Should -Be 0 -Because $result.Stderr
        @($result.Stdout.Split("`n", [System.StringSplitOptions]::RemoveEmptyEntries) | Sort-Object -CaseSensitive) |
            Should -BeExactly @($script:Packages.Keys | Sort-Object -CaseSensitive)
    }
}

Describe 'Architecture workflows' {
    It 'Fedora installs media, PipeWire, OCR, font, and PowerShell runtime dependencies' {
        $job = Get-WorkflowJob 'linux-rpm'
        $script = Get-WorkflowStepScript $job 'Install native Fedora build and packaging dependencies'
        $tokens = Split-ShellWords $script.Replace("\`n", '') -Comments
        $required = @(
            'gstreamer1-plugins-base', 'gstreamer1-plugins-good', 'gstreamer1-plugins-bad-free',
            'gstreamer1-plugin-libav', 'ffmpeg-free', 'pipewire-gstreamer',
            'tesseract', 'tesseract-langpack-eng', 'fontconfig', 'dejavu-sans-fonts', 'dnf5-plugins', 'libicu'
        )
        @($required | Where-Object { $tokens -cnotcontains $_ }) | Should -BeNullOrEmpty -Because 'missing explicit Fedora runtime dependencies'
        $tokens | Should -Not -Contain 'gstreamer1-libav'
        $script | Should -BeLikeExactly '*dnf config-manager setopt fedora-cisco-openh264.enabled=1*'
        $script.IndexOf('fedora-cisco-openh264.enabled=1') | Should -BeLessThan $script.IndexOf('gstreamer1-plugin-libav')
        $spec = [System.IO.File]::ReadAllText((Join-Path $script:Assets 'jiffshot.spec')).Replace("`r`n", "`n")
        $spec | Should -BeLikeExactly "*Recommends:     gstreamer1-plugin-libav`n*"
        $spec | Should -Not -BeLikeExactly "*Recommends:     gstreamer1-libav`n*"
        $job.IndexOf('Install native Fedora build and packaging dependencies') | Should -BeLessThan $job.IndexOf('Test against native Fedora runtime libraries')
    }

    It 'Fedora enables free codecs and full FFmpeg before workspace tests' {
        $job = Get-WorkflowJob 'linux-rpm'
        $script = Get-WorkflowStepScript $job 'Enable Fedora 43 RPM Fusion media codecs'
        $script | Should -BeLikeExactly '*test "$(rpm -E %fedora)" = 43*'
        $tokens = Split-ShellWords $script.Replace("\`n", '') -Comments
        $tokens | Should -Contain 'https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-43.noarch.rpm'
        $job | Should -BeLikeExactly '*dnf config-manager setopt fedora-cisco-openh264.enabled=1*'
        $script | Should -BeLikeExactly '*dnf swap --setopt=install_weak_deps=False -y ffmpeg-free ffmpeg --allowerasing*'
        $tokens | Should -Contain 'gstreamer1-plugins-ugly'
        $tokens | Should -Contain 'gstreamer1-plugins-bad-freeworld'
        $tokens | Should -Not -Contain '--nogpgcheck'
        $job.IndexOf('Enable Fedora 43 RPM Fusion media codecs') | Should -BeLessThan $job.IndexOf('Test against native Fedora runtime libraries')
        $test = Get-WorkflowStepScript $job 'Test against native Fedora runtime libraries'
        $words = Split-ShellWords $test
        $words | Should -BeExactly @('cargo', 'test', '--release', '--locked', '--workspace')
    }

    It 'Fedora provider checks verify codecs, English, and font without aliasing' {
        $job = Get-WorkflowJob 'linux-rpm'
        $script = Get-WorkflowStepScript $job 'Verify Fedora media and OCR dependency providers'
        $tokens = Split-ShellWords $script.Replace("\`n", '') -Comments
        $start = [array]::IndexOf($tokens, 'in') + 1
        $stop = [array]::IndexOf($tokens, 'do')
        $tokens[$start..($stop - 1)] | Should -BeExactly @(
            'jpegenc', 'pngenc', 'mp4mux', 'x264enc', 'h264parse', 'aacparse',
            'fdkaacenc', 'avdec_h264', 'avdec_aac', 'pipewiresrc', 'pulsesrc', 'audiomixer;')
        $script | Should -BeLikeExactly '*gst-inspect-1.0 --exists "$element"*'
        $script | Should -BeLikeExactly '*languages=$(tesseract --list-langs)*'
        $script.Contains('printf ''%s\n'' "$languages" | grep -Fx eng') | Should -BeTrue
        $script.Contains('family=$(fc-match -f ''%{family}\n'' ''DejaVu Sans'')') | Should -BeTrue
        $script.Contains('printf ''%s\n'' "$family" | grep -Fx ''DejaVu Sans''') | Should -BeTrue
        $tokens | Should -Not -Contain 'voaacenc' -Because 'do not silently alias the unavailable AAC factory'
        $docs = [System.IO.File]::ReadAllText((Join-Path $script:Assets 'README.txt'))
        $docs | Should -BeLikeExactly '*either voaacenc or fdkaacenc*'
        $docs | Should -BeLikeExactly '*voaacenc is preferred*'
        $docs | Should -BeLikeExactly '*128 kbit/s AAC-LC*'
        $docs | Should -Not -BeLikeExactly '*resolve the voaacenc compatibility*'
        $job.IndexOf('Verify Fedora media and OCR dependency providers') | Should -BeLessThan $job.IndexOf('Test against native Fedora runtime libraries')
    }

    It '<JobName> uses both native runners and distinct artifacts' -ForEach @(
        @{ JobName = 'linux'; CachePrefix = 'ubuntu2404'; Suffix = '' }
        @{ JobName = 'linux-rpm'; CachePrefix = 'fedora43'; Suffix = '-rpm' }
    ) {
        $job = Get-WorkflowJob $JobName
        $rows = @([regex]::Matches($job, '- arch: (\w+)\n\s+runner: ([\w.-]+)\n\s+machine: (\w+)') | ForEach-Object {
                "$($_.Groups[1].Value) $($_.Groups[2].Value) $($_.Groups[3].Value)"
            })
        $rows | Should -BeExactly @('x64 ubuntu-24.04 x86_64', 'arm64 ubuntu-24.04-arm aarch64')
        foreach ($expected in @(
                'runs-on: ${{ matrix.runner }}'
                'test "$(uname -m)" = "${{ matrix.machine }}"'
                'JIFFSHOT_TEST_ARCH: ${{ matrix.arch }}'
                'build.ps1 -Arch "${{ matrix.arch }}" -Format'
                'cargo test --release --locked --workspace'
                'cargo build --release --locked -p jiffshot'
                'pwsh -NoProfile -File package/linux/run-tests.ps1'
                "name: jiffshot-linux-`${{ matrix.arch }}$Suffix"
                "key: $CachePrefix-linux-`${{ matrix.arch }}-"
                "restore-keys: $CachePrefix-linux-`${{ matrix.arch }}-"
            )) {
            $job.Contains($expected) | Should -BeTrue -Because "$JobName must contain $expected"
        }
    }

    It 'Fedora runs in a container and Ubuntu smoke-tests the portable package' {
        (Get-WorkflowJob 'linux-rpm').Contains('container: fedora:43') | Should -BeTrue
        (Get-WorkflowJob 'linux').Contains('smoke.ps1 -Launcher') | Should -BeTrue
    }

    It 'install matrix covers both formats and native architectures' {
        $job = Get-WorkflowJob 'linux-package-install'
        $pattern = '- format: (\w+)\n\s+arch: (\w+)\n\s+runner: ([\w.-]+)\n\s+machine: (\w+)\n\s+package_arch: (\w+)\n\s+image: ([\w.:]+)\n\s+artifact: ([\w-]+)'
        $rows = @([regex]::Matches($job, $pattern) | ForEach-Object { (1..7 | ForEach-Object -Begin { $match = $_ } -Process { $match.Groups[$_].Value }) -join ' ' })
        $rows | Should -BeExactly @(
            'deb x64 ubuntu-24.04 x86_64 amd64 ubuntu:24.04 jiffshot-linux-x64'
            'rpm x64 ubuntu-24.04 x86_64 x86_64 fedora:43 jiffshot-linux-x64-rpm'
            'deb arm64 ubuntu-24.04-arm aarch64 arm64 ubuntu:24.04 jiffshot-linux-arm64'
            'rpm arm64 ubuntu-24.04-arm aarch64 aarch64 fedora:43 jiffshot-linux-arm64-rpm'
        )
        foreach ($expected in @(
                'runs-on: ${{ matrix.runner }}'
                'sha256sum --check "SHA256SUMS-linux-$4.txt"'
                'validate-installed.ps1 -Arch "$4"'
                'smoke.ps1 -Launcher /usr/bin/jiffshot'
                'test "$(uname -m)" = "$2"'
                'package-removal-sentinel'
                'apt-get remove -y jiffshot'
                'dnf remove -y jiffshot'
                '--volume "$RUNNER_TEMP/powershell:/opt/microsoft/powershell/7:ro"'
            )) {
            $job.Contains($expected) | Should -BeTrue -Because "install job must contain $expected"
        }
    }

    It '<JobName> installs PowerShell before its first pwsh script' -ForEach @(
        @{ JobName = 'linux' }, @{ JobName = 'linux-rpm' }, @{ JobName = 'linux-package-install' }
    ) {
        $job = Get-WorkflowJob $JobName
        $setup = $job.IndexOf('uses: ./.github/actions/setup-pwsh')
        $setup | Should -BeGreaterOrEqual 0
        $setup | Should -BeLessThan $job.IndexOf('pwsh -NoProfile -File')
        $job.IndexOf('uses: actions/checkout@') | Should -BeLessThan $setup
    }

    It 'the PowerShell setup action pins and verifies its downloads' {
        $action = [System.IO.File]::ReadAllText((Join-Path $script:Root '.github/actions/setup-pwsh/action.yml')).Replace("`r`n", "`n")
        $action | Should -MatchExactly '(?m)^\s+PWSH_VERSION: 7\.\d+\.\d+$'
        $action | Should -MatchExactly '(?m)^\s+PWSH_SHA256_X64: [0-9a-f]{64}$'
        $action | Should -MatchExactly '(?m)^\s+PWSH_SHA256_ARM64: [0-9a-f]{64}$'
        $action | Should -MatchExactly '(?m)^\s+PESTER_VERSION: 5\.\d+\.\d+$'
        $action | Should -MatchExactly '(?m)^\s+PESTER_SHA256: [0-9a-f]{64}$'
        $action.Contains('sha256sum --check --strict') | Should -BeTrue
        $action.Contains('if ($actual -cne $env:PESTER_SHA256) { throw') | Should -BeTrue
    }

    It 'release uses the verified collector and propagates failure' {
        $release = Get-WorkflowText 'release.yml'
        foreach ($expected in @(
                'pwsh -NoProfile -File package/linux/assemble-release.ps1 -ArtifactDir artifacts'
                '-OutputDir release -Version "$env:RELEASE_VERSION"'
                "if (`$LASTEXITCODE -ne 0) { throw 'Linux release assembly"
                'Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256'
                'Set-Content release\SHA256SUMS.txt -Encoding ascii'
            )) {
            $release.Contains($expected) | Should -BeTrue -Because "release.yml must contain $expected"
        }
    }
}
