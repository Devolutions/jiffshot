#Requires -Version 7.4
# Focused regression tests for the Linux package builder and its outputs.

BeforeDiscovery {
    $formats = @(([string]$(if ($env:JIFFSHOT_TEST_FORMATS) { $env:JIFFSHOT_TEST_FORMATS } else { 'tar.gz,deb,rpm' })).Split(','))
    $formatCases = @($formats | ForEach-Object { @{ Format = $_ } })
}

BeforeAll {
    . (Join-Path $PSScriptRoot 'TestHelpers.ps1')
    Import-Module $script:PackageModule -Force
}

Describe 'Package builder' {
    BeforeEach {
        $script:Work = New-TestDirectory
    }

    AfterEach {
        Remove-Item -LiteralPath $script:Work -Recurse -Force -ErrorAction Ignore
    }

    It 'desktop actions launch supported Linux workflows' {
        $desktop = Read-DesktopEntry (Join-Path $script:Assets 'com.devolutions.Jiffshot.desktop')
        $desktop['Desktop Entry']['Actions'] | Should -BeExactly 'Capture;InteractiveCapture;GrabText;Scrolling;HorizontalScrolling;Panoramic;Interval;Record;'
        $desktop['Desktop Action Capture']['Exec'] | Should -BeExactly 'jiffshot --capture'
        $desktop['Desktop Action InteractiveCapture']['Exec'] | Should -BeExactly 'jiffshot --capture-interactive'
        $desktop['Desktop Action GrabText']['Exec'] | Should -BeExactly 'jiffshot --grab-text'
        $desktop['Desktop Action Scrolling']['Exec'] | Should -BeExactly 'jiffshot --scrolling'
        $desktop['Desktop Action HorizontalScrolling']['Exec'] | Should -BeExactly 'jiffshot --scrolling-horizontal'
        $desktop['Desktop Action Panoramic']['Exec'] | Should -BeExactly 'jiffshot --panoramic'
        $desktop['Desktop Action Interval']['Exec'] | Should -BeExactly 'jiffshot --interval'
        $desktop['Desktop Action Record']['Exec'] | Should -BeExactly 'jiffshot --record'
    }

    It 'CI jobs are unique and include Linux package builds' {
        $workflow = Get-WorkflowText 'ci.yml'
        $workflow | Should -Match "`njobs:`n"
        $jobs = $workflow.Split("`njobs:`n", 2)[1]
        $names = @([regex]::Matches($jobs, '(?m)^  ([a-zA-Z0-9_-]+):[ \t]*$') | ForEach-Object { $_.Groups[1].Value })
        @($names | Sort-Object -Unique -CaseSensitive).Count | Should -Be $names.Count -Because "duplicate CI job IDs: $($names -join ', ')"
        foreach ($name in 'linux', 'linux-rpm', 'linux-package-install') {
            $names | Should -Contain $name
        }
    }

    It 'release version matches the workspace and rejects mismatches' {
        $version = Get-JiffshotPackageVersion
        Get-JiffshotPackageVersion -Expected $version | Should -BeExactly $version
        { Get-JiffshotPackageVersion -Expected '999.0.0' } |
            Should -Throw -ExceptionType ([System.IO.InvalidDataException]) -ExpectedMessage "*does not match Cargo.toml*"
    }

    It 'release version rejects prereleases' {
        [System.IO.File]::WriteAllText((Join-Path $script:Work 'Cargo.toml'), "[workspace.package]`nversion = `"1.0.0-beta.1`"`n")
        InModuleScope JiffshotLinuxPackage -Parameters @{ Root = $script:Work } {
            param($Root)
            $previous = $script:Root
            $script:Root = $Root
            try {
                { Get-JiffshotPackageVersion } |
                    Should -Throw -ExceptionType ([System.IO.InvalidDataException]) -ExpectedMessage '*X.Y.Z*'
            }
            finally {
                $script:Root = $previous
            }
        }
    }

    It 'binary validation rejects non-ELF and unsupported inputs' {
        $binary = Join-Path $script:Work 'binary'
        [System.IO.File]::WriteAllBytes($binary, (Get-AsciiBytes 'not an ELF file'))
        { Confirm-JiffshotElfBinary -Path $binary } | Should -Throw -ExceptionType ([System.IO.InvalidDataException]) -ExpectedMessage '*ELF*'
        [System.IO.File]::WriteAllBytes($binary, (New-ElfHeader 40))
        { Confirm-JiffshotElfBinary -Path $binary } | Should -Throw -ExceptionType ([System.IO.InvalidDataException]) -ExpectedMessage '*unsupported ELF machine 40*'
        [System.IO.File]::WriteAllBytes($binary, (New-ElfHeader))
        (Confirm-JiffshotElfBinary -Path $binary).Name | Should -BeExactly 'x64'
    }

    It 'missing tools are reported before building' {
        Mock -ModuleName JiffshotLinuxPackage Test-JiffshotTool { $false }
        { Confirm-JiffshotPackagingTool -Format deb, rpm } |
            Should -Throw -ExceptionType ([System.IO.InvalidDataException]) -ExpectedMessage '*desktop-file-validate, dpkg-deb, dpkg-shlibdeps, readelf, rpmbuild, strip*'
    }

    It 'DEB and RPM require separate native builds' {
        { Invoke-JiffshotLinuxPackageBuild -Format deb, rpm } |
            Should -Throw -ExceptionType ([System.IO.InvalidDataException]) -ExpectedMessage '*separate native builds*'
    }

    It 'RPM input rejects the Ubuntu OCR ABI and accepts the Fedora ABI' {
        Mock -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand { [pscustomobject]@{ Stdout = 'NEEDED Shared library: [liblept.so.5]' } }
        { Confirm-JiffshotRpmInput -BuildDir '/release' } |
            Should -Throw -ExceptionType ([System.IO.InvalidDataException]) -ExpectedMessage '*native Fedora build*'
        Mock -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand { [pscustomobject]@{ Stdout = 'NEEDED Shared library: [libleptonica.so.6]' } }
        { Confirm-JiffshotRpmInput -BuildDir '/release' } | Should -Not -Throw
    }

    It 'payload staging strips copies without modifying build inputs' {
        $inputs = Join-Path $script:Work 'release'
        $payload = Join-Path $script:Work 'payload'
        [void][System.IO.Directory]::CreateDirectory($inputs)
        $content = Join-Bytes (New-ElfHeader) (Get-AsciiBytes 'original debug data')
        foreach ($name in $script:RuntimeFiles) {
            [System.IO.File]::WriteAllBytes((Join-Path $inputs $name), $content)
        }
        $calls = [System.Collections.Generic.List[object]]::new()
        Mock -ModuleName JiffshotLinuxPackage Invoke-JiffshotNativeCommand { $calls.Add([string[]]$Command); [pscustomobject]@{ Stdout = '' } }
        $previous = Set-TestUmask 0x3F
        try {
            New-JiffshotPayload -BuildDir $inputs -Payload $payload
        }
        finally {
            [void](Set-TestUmask $previous)
        }
        for ($index = 0; $index -lt 4; $index++) {
            $calls[$index] | Should -BeExactly @('strip', '--strip-debug', (Join-Path $payload "lib/jiffshot/$($script:RuntimeFiles[$index])"))
        }
        foreach ($name in $script:RuntimeFiles) {
            [System.IO.File]::ReadAllBytes((Join-Path $inputs $name)) | Should -Be $content
        }
        foreach ($directory in @($payload) + [System.IO.Directory]::GetDirectories($payload, '*', [System.IO.SearchOption]::AllDirectories)) {
            (Get-UnixMode $directory) -band 0x1FF | Should -Be 0x1ED -Because $directory
        }
        Get-RelativeFiles $payload | Should -BeExactly $script:ExpectedFiles
    }

    It 'archive is reproducible with safe paths and normalized metadata' {
        $payload = Join-Path $script:Work 'payload with spaces'
        [void][System.IO.Directory]::CreateDirectory((Join-Path $payload 'bin'))
        $executable = Join-Path $payload 'bin/jiffshot'
        [System.IO.File]::WriteAllBytes($executable, (Get-AsciiBytes 'launcher'))
        [System.IO.File]::SetUnixFileMode($executable, [System.IO.UnixFileMode]0x1ED)
        $first = Join-Path $script:Work 'first.tar.gz'
        $second = Join-Path $script:Work 'second.tar.gz'
        foreach ($destination in $first, $second) {
            Write-JiffshotArchive -Payload $payload -Destination $destination -TopDirectory 'jiffshot-1.2.3-linux-x64' -Epoch 123456789
        }
        [System.IO.File]::ReadAllBytes($first) | Should -Be ([System.IO.File]::ReadAllBytes($second))
        $members = @(Read-TarGz $first -Content)
        $members.Name | Should -BeExactly @('jiffshot-1.2.3-linux-x64', 'jiffshot-1.2.3-linux-x64/bin', 'jiffshot-1.2.3-linux-x64/bin/jiffshot')
        foreach ($member in $members) {
            @($member.Uid, $member.Gid, $member.Mtime, $member.Mode) | Should -Be @(0, 0, 123456789, 0x1ED)
        }
        [System.Text.Encoding]::ASCII.GetString($members[-1].Data) | Should -BeExactly 'launcher'
    }

    It 'launcher preserves arguments, cwd, and library path through symlinks' {
        $work = New-TestDirectory 'jiffshot launch '
        try {
            $bindir = Join-Path $work 'package/bin'
            $runtime = Join-Path $work 'package/lib/jiffshot'
            [void][System.IO.Directory]::CreateDirectory($bindir)
            [void][System.IO.Directory]::CreateDirectory($runtime)
            $launcher = Join-Path $bindir 'jiffshot'
            [System.IO.File]::WriteAllText($launcher, [System.IO.File]::ReadAllText((Join-Path $script:Assets 'jiffshot')))
            [System.IO.File]::SetUnixFileMode($launcher, [System.IO.UnixFileMode]0x1ED)
            $executable = Join-Path $runtime 'jiffshot'
            [System.IO.File]::WriteAllText($executable, "#!/bin/sh`nprintf '%s\0' `"`$(pwd -P)`" `"`$LD_LIBRARY_PATH`" `"`$JIFFSHOT_LAUNCHER`" `"`$@`"`n")
            [System.IO.File]::SetUnixFileMode($executable, [System.IO.UnixFileMode]0x1ED)
            $symlink = Join-Path $work 'linked launcher'
            [void][System.IO.File]::CreateSymbolicLink($symlink, $launcher)
            $arguments = @('image with spaces.png', '', '--not-a-shell-command', '$unchanged')
            $previousPath, $previousLibraries = $env:PATH, $env:LD_LIBRARY_PATH
            try {
                # .NET resolves bare command names with this process's PATH.
                $env:PATH = "${bindir}:$previousPath"
                $env:LD_LIBRARY_PATH = '/existing/libs'
                foreach ($command in $symlink, 'jiffshot') {
                    $result = Invoke-JiffshotNativeCommand -Command (@($command) + $arguments) -WorkingDirectory $work
                    $values = $result.Stdout.Split([char]0)
                    $values[-1] | Should -BeExactly ''
                    $cwd, $libraries, $startupLauncher = $values[0..2]
                    $values[3..($values.Count - 2)] | Should -BeExactly $arguments
                    $cwd | Should -BeExactly $work
                    [System.IO.Path]::GetFullPath($libraries.Split(':')[0]) | Should -BeExactly $runtime
                    $libraries | Should -BeLikeExactly '*:/existing/libs'
                    $startupLauncher | Should -BeExactly $launcher
                }
            }
            finally {
                $env:PATH, $env:LD_LIBRARY_PATH = $previousPath, $previousLibraries
            }
        }
        finally {
            Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction Ignore
        }
    }
}

Describe 'Package contents' -Tag PackageContent {
    BeforeAll {
        $script:Version = Get-JiffshotPackageVersion
        $native = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
            'X64' { 'x64' }
            'Arm64' { 'arm64' }
        }
        $script:Arch = if ($env:JIFFSHOT_TEST_ARCH) { $env:JIFFSHOT_TEST_ARCH } else { $native }
        # Independent expectations: do not derive test identity from the builder.
        $script:DebArch, $script:RpmArch, $script:Machine = @{
            x64   = @('amd64', 'x86_64', 62)
            arm64 = @('arm64', 'aarch64', 183)
        }[$script:Arch]
        $directory = if ($env:JIFFSHOT_PACKAGE_DIR) { $env:JIFFSHOT_PACKAGE_DIR } else { Join-Path $script:Root 'package/out/linux' }
        $script:Output = [System.IO.Path]::GetFullPath($directory)
        $script:Formats = @(([string]$(if ($env:JIFFSHOT_TEST_FORMATS) { $env:JIFFSHOT_TEST_FORMATS } else { 'tar.gz,deb,rpm' })).Split(','))
        $script:Names = @{
            'tar.gz' = "Devolutions.Jiffshot-linux-$($script:Arch)-$($script:Version).tar.gz"
            'deb'    = "jiffshot_$($script:Version)_$($script:DebArch).deb"
            'rpm'    = "jiffshot-$($script:Version)-1.$($script:RpmArch).rpm"
        }
        $script:Sums = @{}
        $text = [System.IO.File]::ReadAllText((Join-Path $script:Output "SHA256SUMS-linux-$($script:Arch).txt"))
        foreach ($line in $text.Split("`n", [System.StringSplitOptions]::RemoveEmptyEntries)) {
            $digest, $name = $line.Split('  ', 2)
            $script:Sums[$name] = $digest
        }
        $script:Extracted = New-TestDirectory 'jiffshot extracted '
        $script:Roots = @{}
    }

    AfterAll {
        Remove-Item -LiteralPath $script:Extracted -Recurse -Force -ErrorAction Ignore
    }

    It 'checksum manifest covers exactly the requested formats' {
        $script:Formats.Count | Should -BeGreaterThan 0 -Because 'no package formats requested'
        @($script:Sums.Keys | Sort-Object -CaseSensitive) | Should -BeExactly @($script:Formats | ForEach-Object { $script:Names[$_] } | Sort-Object -CaseSensitive)
    }

    It '<Format> package has a complete payload, metadata, and checksum' -ForEach $formatCases {
        $artifact = Join-Path $script:Output $script:Names[$Format]
        Get-TestSha256 $artifact | Should -BeExactly $script:Sums[[System.IO.Path]::GetFileName($artifact)]
        $destination = Join-Path $script:Extracted $Format
        [void][System.IO.Directory]::CreateDirectory($destination)
        switch ($Format) {
            'tar.gz' {
                $prefix = "jiffshot-$($script:Version)-linux-$($script:Arch)"
                $members = @(Read-TarGz $artifact)
                foreach ($member in $members) {
                    ($member.Name -ceq $prefix -or $member.Name.StartsWith("$prefix/", [System.StringComparison]::Ordinal)) | Should -BeTrue -Because $member.Name
                    $member.Type | Should -Not -BeIn @('SymbolicLink', 'HardLink')
                    @($member.Uid, $member.Gid) | Should -Be @(0, 0)
                    if ($member.Type -eq 'Directory') {
                        $member.Mode | Should -Be 0x1ED -Because $member.Name
                    }
                }
                Expand-TarGz $artifact $destination
                $root = Join-Path $destination $prefix
            }
            'deb' {
                (Invoke-JiffshotNativeCommand -Command @('dpkg-deb', '-f', $artifact, 'Version', 'Architecture')).Stdout |
                    Should -BeExactly "Version: $($script:Version)`nArchitecture: $($script:DebArch)`n"
                $depends = (Invoke-JiffshotNativeCommand -Command @('dpkg-deb', '-f', $artifact, 'Depends')).Stdout
                foreach ($dependency in 'libc6', 'libtesseract5', 'libx11-6', 'libgtk-3-0t64', 'tesseract-ocr-eng', 'gstreamer1.0-pipewire') {
                    $depends | Should -BeLikeExactly "*$dependency*"
                }
                $info = [System.Diagnostics.ProcessStartInfo]::new('dpkg-deb')
                foreach ($argument in '--fsys-tarfile', $artifact) { $info.ArgumentList.Add($argument) }
                $info.UseShellExecute = $false
                $info.RedirectStandardOutput = $true
                $unpacker = [System.Diagnostics.Process]::Start($info)
                try {
                    $directories = @(Read-TarEntries $unpacker.StandardOutput.BaseStream | Where-Object Type -EQ 'Directory')
                    $unpacker.StandardOutput.Close()
                    $unpacker.WaitForExit()
                    $unpacker.ExitCode | Should -Be 0
                }
                finally {
                    $unpacker.Dispose()
                }
                $directories | Where-Object { $_.Name -ceq '.' -and $_.Mode -eq 0x1ED -and $_.Uid -eq 0 -and $_.Gid -eq 0 } | Should -Not -BeNullOrEmpty
                foreach ($directory in $directories) {
                    @($directory.Mode, $directory.Uid, $directory.Gid) | Should -Be @(0x1ED, 0, 0) -Because $directory.Name
                }
                [void](Invoke-JiffshotNativeCommand -Command @('dpkg-deb', '-x', $artifact, $destination))
                $root = Join-Path $destination 'usr'
            }
            'rpm' {
                (Invoke-JiffshotNativeCommand -Command @('rpm', '-qp', '--queryformat', '%{VERSION} %{RELEASE} %{ARCH}', $artifact)).Stdout |
                    Should -BeExactly "$($script:Version) 1 $($script:RpmArch)"
                $requires = (Invoke-JiffshotNativeCommand -Command @('rpm', '-qp', '--requires', $artifact)).Stdout
                $requires | Should -MatchExactly '(?m)^libtesseract\.so\.5(?:\.\d+)*\(\)\(64bit\)$'
                foreach ($dependency in 'libX11.so.6()(64bit)', 'gtk3', 'pipewire-gstreamer') {
                    $requires.Contains($dependency) | Should -BeTrue -Because $dependency
                }
                $requires.Contains('libleptonica.so.6()(64bit)') | Should -BeTrue
                $requires.Contains('liblept.so.5') | Should -BeFalse
                $modes = @{}
                $listing = (Invoke-JiffshotNativeCommand -Command @('rpm', '-qp', '--queryformat', "[%{FILENAMES} %{FILEMODES:perms}`n]", $artifact)).Stdout
                foreach ($line in $listing.Split("`n", [System.StringSplitOptions]::RemoveEmptyEntries)) {
                    $path, $mode = $line.Split(' ', 2)
                    $modes[$path] = $mode
                }
                foreach ($directory in '/usr/lib/jiffshot', '/usr/share/doc/jiffshot') {
                    $modes[$directory] | Should -BeExactly 'drwxr-xr-x'
                }
                [void](Invoke-JiffshotNativeCommand -WorkingDirectory $destination -Command @(
                        'bash', '-o', 'pipefail', '-c', 'rpm2cpio "$1" | cpio -idm --quiet --no-absolute-filenames', 'bash', $artifact))
                $root = Join-Path $destination 'usr'
            }
        }
        Get-RelativeFiles $root | Should -BeExactly $script:ExpectedFiles
        (Get-UnixMode (Join-Path $root 'bin/jiffshot')) -band 0x1FF | Should -Be 0x1ED
        [System.IO.File]::ReadAllBytes((Join-Path $root 'bin/jiffshot')) | Should -Not -Contain ([byte]13)
        foreach ($name in $script:RuntimeFiles) {
            $binary = Join-Path $root "lib/jiffshot/$name"
            $header = [System.IO.File]::ReadAllBytes($binary)[0..63]
            $header[0..6] | Should -Be @(0x7F, 0x45, 0x4C, 0x46, 0x02, 0x01, 0x01) -Because $name
            $header[18] + 256 * $header[19] | Should -Be $script:Machine -Because $name
            (Confirm-JiffshotElfBinary -Path $binary).Name | Should -BeExactly $script:Arch
        }
        $desktop = Join-Path $root 'share/applications/com.devolutions.Jiffshot.desktop'
        [void](Invoke-JiffshotNativeCommand -Command @('desktop-file-validate', $desktop))
        [System.IO.File]::ReadAllText($desktop) |
            Should -BeExactly ([System.IO.File]::ReadAllText((Join-Path $script:Assets 'com.devolutions.Jiffshot.desktop')).Replace("`r`n", "`n"))
        $mime = [xml][System.IO.File]::ReadAllText((Join-Path $root 'share/mime/packages/com.devolutions.Jiffshot.xml'))
        $namespaces = [System.Xml.XmlNamespaceManager]::new($mime.NameTable)
        $namespaces.AddNamespace('m', 'http://www.freedesktop.org/standards/shared-mime-info')
        $mime.DocumentElement.SelectSingleNode('m:mime-type', $namespaces).GetAttribute('type') | Should -BeExactly 'application/x-jiffshot'
        $mime.DocumentElement.SelectSingleNode('m:mime-type/m:glob', $namespaces).GetAttribute('pattern') | Should -BeExactly '*.jsd'
        $script:Roots[$Format] = $root
    }

    It 'tar.gz and DEB payloads are identical' {
        if (-not ($script:Roots.ContainsKey('tar.gz') -and $script:Roots.ContainsKey('deb'))) {
            Set-ItResult -Skipped -Because 'both tar.gz and deb formats are required'
            return
        }
        foreach ($relative in $script:ExpectedFiles) {
            Get-TestSha256 (Join-Path $script:Roots['tar.gz'] $relative) | Should -BeExactly (Get-TestSha256 (Join-Path $script:Roots['deb'] $relative)) -Because $relative
        }
    }
}
