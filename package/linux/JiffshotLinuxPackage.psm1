#Requires -Version 7.4
# Shared Linux packaging logic for build.ps1, assemble-release.ps1,
# validate-installed.ps1, smoke.ps1, and the Pester regression tests.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:Assets = $PSScriptRoot
$script:Root = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$script:RuntimeFiles = @('jiffshot', 'librustolonia_host.so', 'libSkiaSharp.so', 'libHarfBuzzSharp.so')
$script:Maintainer = 'Devolutions <service@devolutions.net>'
$script:DebRuntime = @(
    'libx11-6', 'libice6', 'libsm6', 'libxcursor1', 'libxi6', 'libxrandr2',
    'libgtk-3-0t64 | libgtk-3-0',
    'libicu74 | libicu76', 'libssl3t64 | libssl3',
    'fontconfig', 'fonts-dejavu-core', 'tesseract-ocr-eng',
    'gstreamer1.0-plugins-base (>= 1.24)', 'gstreamer1.0-plugins-good',
    'gstreamer1.0-plugins-bad', 'gstreamer1.0-plugins-ugly',
    'gstreamer1.0-libav', 'gstreamer1.0-pipewire', 'gstreamer1.0-pulseaudio',
    'desktop-file-utils', 'shared-mime-info', 'hicolor-icon-theme'
)
$script:Architectures = [ordered]@{
    x64   = [pscustomobject]@{ PSTypeName = 'Jiffshot.LinuxArchitecture'; Name = 'x64'; Machine = 62; Deb = 'amd64'; Rpm = 'x86_64' }
    arm64 = [pscustomobject]@{ PSTypeName = 'Jiffshot.LinuxArchitecture'; Name = 'arm64'; Machine = 183; Deb = 'arm64'; Rpm = 'aarch64' }
}
$script:Mode755 = 0x1ED
$script:Mode644 = 0x1A4
$script:Mode700 = 0x1C0
$script:Utf8 = [System.Text.UTF8Encoding]::new($false, $true)

function New-PackagingError([string]$Message) {
    [System.IO.InvalidDataException]::new($Message)
}

function Resolve-FullPath([string]$Path) {
    $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path)
}

function Assert-FileExists([string]$Path) {
    if (-not [System.IO.File]::Exists($Path)) {
        throw [System.IO.FileNotFoundException]::new("No such file or directory: '$Path'", $Path)
    }
}

function New-Directory([string]$Path, [switch]$MustNotExist) {
    if ($MustNotExist -and (Test-Path -LiteralPath $Path)) {
        throw [System.IO.IOException]::new("File exists: '$Path'")
    }
    [void][System.IO.Directory]::CreateDirectory($Path)
}

function Set-UnixMode([string]$Path, [int]$Mode) {
    $item = if ([System.IO.Directory]::Exists($Path)) {
        [System.IO.DirectoryInfo]::new($Path)
    }
    else {
        [System.IO.FileInfo]::new($Path)
    }
    $item.UnixFileMode = [System.IO.UnixFileMode]$Mode
}

function Read-NormalizedText([string]$Path) {
    # Match Python's read_text(): strict UTF-8 and universal newlines.
    $text = $script:Utf8.GetString([System.IO.File]::ReadAllBytes($Path))
    $text -replace "`r`n?", "`n"
}

function Write-Utf8Text([string]$Path, [string]$Text) {
    [System.IO.File]::WriteAllBytes($Path, $script:Utf8.GetBytes($Text))
}

function Get-JiffshotPackageInfo {
    <#
    .SYNOPSIS
    Returns the repository root, packaging asset directory, and bundled runtime file names.
    #>
    [CmdletBinding()]
    param()
    [pscustomobject]@{
        Root         = $script:Root
        Assets       = $script:Assets
        RuntimeFiles = [string[]]$script:RuntimeFiles
        Maintainer   = $script:Maintainer
    }
}

function Get-JiffshotLinuxArchitecture {
    <#
    .SYNOPSIS
    Returns the supported Linux architectures (x64 and arm64) or one of them by name.
    #>
    [CmdletBinding()]
    param([string]$Name)
    if ($Name) {
        if ($Name -cnotin $script:Architectures.Keys) {
            throw (New-PackagingError "unsupported architecture: '$Name'; expected x64 or arm64")
        }
        return $script:Architectures[$Name]
    }
    $script:Architectures.Values
}

function Invoke-JiffshotNativeCommand {
    <#
    .SYNOPSIS
    Runs a native command with exact arguments, captures stdout/stderr, and throws on a non-zero exit.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][AllowEmptyString()][string[]]$Command,
        [string]$WorkingDirectory,
        [System.Collections.IDictionary]$Environment
    )
    $info = [System.Diagnostics.ProcessStartInfo]::new($Command[0])
    for ($index = 1; $index -lt $Command.Count; $index++) {
        $info.ArgumentList.Add($Command[$index])
    }
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = [System.Text.UTF8Encoding]::new($false)
    $info.StandardErrorEncoding = [System.Text.UTF8Encoding]::new($false)
    $info.WorkingDirectory = if ($WorkingDirectory) {
        Resolve-FullPath $WorkingDirectory
    }
    else {
        (Get-Location -PSProvider FileSystem).ProviderPath
    }
    if ($Environment) {
        foreach ($key in $Environment.Keys) {
            if ($null -eq $Environment[$key]) {
                [void]$info.Environment.Remove($key)
            }
            else {
                $info.Environment[$key] = [string]$Environment[$key]
            }
        }
    }
    $process = [System.Diagnostics.Process]::Start($info)
    try {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $result = [pscustomobject]@{
            Command  = $Command
            ExitCode = $process.ExitCode
            Stdout   = $stdout.GetAwaiter().GetResult()
            Stderr   = $stderr.GetAwaiter().GetResult()
        }
    }
    finally {
        $process.Dispose()
    }
    if ($result.ExitCode -ne 0) {
        $quoted = ($Command | ForEach-Object { "'$_'" }) -join ', '
        $exception = [System.Management.Automation.RuntimeException]::new(
            "Command '[$quoted]' returned non-zero exit status $($result.ExitCode)."
        )
        $exception.Data['Stdout'] = $result.Stdout
        $exception.Data['Stderr'] = $result.Stderr
        throw $exception
    }
    $result
}

function Test-JiffshotTool([string]$Name) {
    [bool](Get-Command -Name $Name -CommandType Application -ErrorAction Ignore)
}

function Test-JiffshotLinuxHost {
    $IsLinux
}

function Skip-TomlSpace([string]$Text, [ref]$Position, [switch]$Newlines) {
    while ($Position.Value -lt $Text.Length) {
        $character = $Text[$Position.Value]
        if ($character -eq ' ' -or $character -eq "`t") {
            $Position.Value++
        }
        elseif ($Newlines -and ($character -eq "`n" -or $character -eq "`r")) {
            $Position.Value++
        }
        elseif ($Newlines -and $character -eq '#') {
            while ($Position.Value -lt $Text.Length -and $Text[$Position.Value] -ne "`n") { $Position.Value++ }
        }
        else {
            break
        }
    }
}

function Read-TomlString([string]$Text, [ref]$Position) {
    $quote = $Text[$Position.Value]
    $multiline = $Position.Value + 2 -lt $Text.Length -and $Text.Substring($Position.Value, 3) -eq [string]::new($quote, 3)
    $delimiter = if ($multiline) { [string]::new($quote, 3) } else { [string]$quote }
    $Position.Value += $delimiter.Length
    if ($multiline -and $Position.Value -lt $Text.Length -and $Text[$Position.Value] -eq "`n") {
        $Position.Value++
    }
    $builder = [System.Text.StringBuilder]::new()
    while ($true) {
        if ($Position.Value -ge $Text.Length) { throw (New-PackagingError 'unterminated TOML string') }
        if ($Text.Substring($Position.Value).StartsWith($delimiter, [System.StringComparison]::Ordinal)) {
            $Position.Value += $delimiter.Length
            # Up to two adjacent quotes may close a multi-line string.
            while ($multiline -and $Position.Value -lt $Text.Length -and $Text[$Position.Value] -eq $quote) {
                [void]$builder.Append($quote)
                $Position.Value++
            }
            return $builder.ToString()
        }
        $character = $Text[$Position.Value]
        if (-not $multiline -and $character -eq "`n") { throw (New-PackagingError 'unterminated TOML string') }
        if ($quote -eq '"' -and $character -eq '\') {
            $Position.Value++
            $escape = $Text[$Position.Value]
            $Position.Value++
            switch -CaseSensitive ($escape) {
                'b' { [void]$builder.Append("`b") }
                't' { [void]$builder.Append("`t") }
                'n' { [void]$builder.Append("`n") }
                'f' { [void]$builder.Append("`f") }
                'r' { [void]$builder.Append("`r") }
                'e' { [void]$builder.Append([char]27) }
                '"' { [void]$builder.Append('"') }
                '\' { [void]$builder.Append('\') }
                { $_ -ceq 'u' -or $_ -ceq 'U' } {
                    $length = if ($escape -ceq 'u') { 4 } else { 8 }
                    $code = [Convert]::ToInt32($Text.Substring($Position.Value, $length), 16)
                    [void]$builder.Append([char]::ConvertFromUtf32($code))
                    $Position.Value += $length
                }
                default {
                    if ($multiline -and ($escape -eq "`n" -or $escape -eq ' ' -or $escape -eq "`t" -or $escape -eq "`r")) {
                        # Line-ending backslash trims following whitespace.
                        $Position.Value--
                        Skip-TomlSpace $Text $Position -Newlines
                    }
                    else {
                        throw (New-PackagingError "invalid TOML escape: \$escape")
                    }
                }
            }
            continue
        }
        [void]$builder.Append($character)
        $Position.Value++
    }
}

function Read-TomlKey([string]$Text, [ref]$Position) {
    $parts = [System.Collections.Generic.List[string]]::new()
    while ($true) {
        Skip-TomlSpace $Text $Position
        if ($Position.Value -ge $Text.Length) { throw (New-PackagingError 'invalid TOML key') }
        $character = $Text[$Position.Value]
        if ($character -eq '"' -or $character -eq "'") {
            $parts.Add((Read-TomlString $Text $Position))
        }
        else {
            $start = $Position.Value
            while ($Position.Value -lt $Text.Length -and $Text[$Position.Value] -match '[A-Za-z0-9_-]') { $Position.Value++ }
            if ($Position.Value -eq $start) { throw (New-PackagingError 'invalid TOML key') }
            $parts.Add($Text.Substring($start, $Position.Value - $start))
        }
        Skip-TomlSpace $Text $Position
        if ($Position.Value -lt $Text.Length -and $Text[$Position.Value] -eq '.') {
            $Position.Value++
            continue
        }
        return , $parts.ToArray()
    }
}

function Read-TomlValue([string]$Text, [ref]$Position, [string[]]$Path, [hashtable]$Values) {
    $character = $Text[$Position.Value]
    if ($character -eq '"' -or $character -eq "'") {
        $Values[$Path -join "`0"] = Read-TomlString $Text $Position
        return
    }
    if ($character -eq '[') {
        $Position.Value++
        while ($true) {
            Skip-TomlSpace $Text $Position -Newlines
            if ($Text[$Position.Value] -eq ']') { $Position.Value++; return }
            Read-TomlValue $Text $Position @() @{}
            Skip-TomlSpace $Text $Position -Newlines
            if ($Text[$Position.Value] -eq ',') { $Position.Value++ }
        }
    }
    if ($character -eq '{') {
        $Position.Value++
        while ($true) {
            Skip-TomlSpace $Text $Position
            if ($Text[$Position.Value] -eq '}') { $Position.Value++; return }
            $key = Read-TomlKey $Text $Position
            if ($Text[$Position.Value] -ne '=') { throw (New-PackagingError 'invalid TOML inline table') }
            $Position.Value++
            Skip-TomlSpace $Text $Position
            Read-TomlValue $Text $Position ($Path + $key) $Values
            Skip-TomlSpace $Text $Position
            if ($Text[$Position.Value] -eq ',') { $Position.Value++ }
        }
    }
    $start = $Position.Value
    while ($Position.Value -lt $Text.Length -and $Text[$Position.Value] -notmatch '[\s,\]}#]') { $Position.Value++ }
    if ($Position.Value -eq $start) { throw (New-PackagingError 'invalid TOML value') }
    $Values[$Path -join "`0"] = [pscustomobject]@{ Scalar = $Text.Substring($start, $Position.Value - $start) }
}

function Read-TomlValues([string]$Path) {
    # A small TOML reader: tables, dotted/quoted keys, strings, arrays, and inline tables.
    $text = Read-NormalizedText $Path
    $position = 0
    $table = @()
    $values = @{}
    while ($true) {
        Skip-TomlSpace $text ([ref]$position) -Newlines
        if ($position -ge $text.Length) { return $values }
        if ($text[$position] -eq '[') {
            $array = $position + 1 -lt $text.Length -and $text[$position + 1] -eq '['
            $position += if ($array) { 2 } else { 1 }
            $table = Read-TomlKey $text ([ref]$position)
            $closing = if ($array) { ']]' } else { ']' }
            if (-not $text.Substring($position).StartsWith($closing, [System.StringComparison]::Ordinal)) {
                throw (New-PackagingError "invalid TOML table header in $Path")
            }
            $position += $closing.Length
        }
        else {
            $key = Read-TomlKey $text ([ref]$position)
            if ($position -ge $text.Length -or $text[$position] -ne '=') {
                throw (New-PackagingError "invalid TOML key/value pair in $Path")
            }
            $position++
            Skip-TomlSpace $text ([ref]$position)
            Read-TomlValue $text ([ref]$position) ($table + $key) $values
        }
        Skip-TomlSpace $text ([ref]$position)
        if ($position -lt $text.Length -and $text[$position] -eq '#') {
            while ($position -lt $text.Length -and $text[$position] -ne "`n") { $position++ }
        }
        if ($position -lt $text.Length -and $text[$position] -ne "`n") {
            throw (New-PackagingError "invalid TOML line ending in $Path")
        }
    }
}

function Get-JiffshotPackageVersion {
    <#
    .SYNOPSIS
    Reads [workspace.package] version from Cargo.toml and optionally asserts the expected version.
    #>
    [CmdletBinding()]
    param([string]$Expected)
    $manifest = Join-Path $script:Root 'Cargo.toml'
    Assert-FileExists $manifest
    $values = Read-TomlValues $manifest
    $key = @('workspace', 'package', 'version') -join "`0"
    if (-not $values.ContainsKey($key)) {
        throw (New-PackagingError "$manifest does not define workspace.package.version")
    }
    $version = $values[$key]
    if ($version -isnot [string] -or $version -cnotmatch '\A(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\z') {
        throw (New-PackagingError 'Linux packages require an X.Y.Z workspace release version')
    }
    if ($Expected -and $Expected -cne $version) {
        throw (New-PackagingError "requested version '$Expected' does not match Cargo.toml ($version)")
    }
    $version
}

function Confirm-JiffshotElfBinary {
    <#
    .SYNOPSIS
    Validates a little-endian ELF64 Linux executable or shared object and returns its architecture.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$Path,
        [object]$Expected,
        [switch]$Shared
    )
    $full = Resolve-FullPath $Path
    Assert-FileExists $full
    $header = [byte[]]::new(64)
    $stream = [System.IO.File]::OpenRead($full)
    try {
        $count = 0
        while ($count -lt 64) {
            $read = $stream.Read($header, $count, 64 - $count)
            if ($read -eq 0) { break }
            $count += $read
        }
    }
    finally {
        $stream.Dispose()
    }
    $magic = [byte[]](0x7F, 0x45, 0x4C, 0x46, 0x02, 0x01)
    if ($count -ne 64 -or -not [System.Linq.Enumerable]::SequenceEqual([byte[]]$header[0..5], $magic)) {
        throw (New-PackagingError "$Path is not a little-endian 64-bit Linux ELF binary")
    }
    $elfType = $header[16] + 256 * $header[17]
    $machine = $header[18] + 256 * $header[19]
    $version = [uint32]$header[20] + 256 * [uint32]$header[21] + 65536 * [uint32]$header[22] + 16777216 * [uint32]$header[23]
    # System V (0) and GNU/Linux (3) are used by Linux executable/shared objects.
    if ($header[6] -ne 1 -or $version -ne 1 -or $header[7] -notin 0, 3 -or $header[8] -ne 0) {
        throw (New-PackagingError "$Path has an unsupported ELF version or Linux ABI")
    }
    if ($header[52] + 256 * $header[53] -ne 64) {
        throw (New-PackagingError "$Path has an invalid ELF64 header size")
    }
    $allowed = if ($Shared) { @(3) } else { @(2, 3) }
    if ($elfType -notin $allowed) {
        $kind = if ($Shared) { 'shared object' } else { 'executable or shared object' }
        throw (New-PackagingError "$Path is not an ELF $kind")
    }
    $architecture = $script:Architectures.Values | Where-Object Machine -EQ $machine | Select-Object -First 1
    if ($null -eq $architecture) {
        throw (New-PackagingError "$Path has unsupported ELF machine $machine; expected Linux x64 or arm64")
    }
    if ($null -ne $Expected -and $architecture.Name -cne $Expected.Name) {
        throw (New-PackagingError "$Path is Linux $($architecture.Name), expected Linux $($Expected.Name)")
    }
    $architecture
}

function Confirm-JiffshotBuild {
    <#
    .SYNOPSIS
    Validates the executable and every private runtime library, returning their shared architecture.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$BuildDir,
        [string]$Arch
    )
    if ($Arch -and $Arch -cnotin $script:Architectures.Keys) {
        throw (New-PackagingError "unsupported architecture assertion: '$Arch'; expected x64 or arm64")
    }
    $expected = if ($Arch) { $script:Architectures[$Arch] } else { $null }
    $architecture = Confirm-JiffshotElfBinary -Path (Join-Path $BuildDir 'jiffshot') -Expected $expected
    foreach ($name in $script:RuntimeFiles | Select-Object -Skip 1) {
        [void](Confirm-JiffshotElfBinary -Path (Join-Path $BuildDir $name) -Expected $architecture -Shared)
    }
    $architecture
}

function Confirm-JiffshotPackagingTool {
    <#
    .SYNOPSIS
    Fails before building when a packaging tool required by the selected formats is missing.
    #>
    [CmdletBinding()]
    param([string[]]$Format)
    $tools = [System.Collections.Generic.SortedSet[string]]::new([System.StringComparer]::Ordinal)
    [void]$tools.Add('strip')
    [void]$tools.Add('desktop-file-validate')
    if ($Format -ccontains 'deb') {
        [void]$tools.Add('dpkg-deb')
        [void]$tools.Add('dpkg-shlibdeps')
    }
    if ($Format -ccontains 'rpm') {
        [void]$tools.Add('rpmbuild')
        [void]$tools.Add('readelf')
    }
    $missing = @($tools | Where-Object { -not (Test-JiffshotTool $_) })
    if ($missing.Count) {
        throw (New-PackagingError "missing Linux packaging tools: $($missing -join ', '); see package/linux/README.txt")
    }
}

function Confirm-JiffshotRpmInput {
    <#
    .SYNOPSIS
    Rejects Ubuntu OCR ABI inputs for Fedora RPM packaging.
    #>
    [CmdletBinding()]
    param([Parameter(Mandatory)][string]$BuildDir)
    $dependencies = (Invoke-JiffshotNativeCommand -Command @('readelf', '--dynamic', (Join-Path $BuildDir 'jiffshot'))).Stdout
    if (-not $dependencies.Contains('[libleptonica.so.6]') -or $dependencies.Contains('[liblept.so.5]')) {
        throw (New-PackagingError (
                'RPM requires a native Fedora build linked to libleptonica.so.6; ' +
                'an Ubuntu 24.04 build linked to liblept.so.5 cannot be repackaged for Fedora'))
    }
}

function New-JiffshotPayload {
    <#
    .SYNOPSIS
    Stages the stripped runtime, launcher, desktop integration, and documentation under a /usr-style payload.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$BuildDir,
        [Parameter(Mandatory)][string]$Payload,
        [object]$Architecture
    )
    $detected = Confirm-JiffshotBuild -BuildDir $BuildDir -Arch $(if ($Architecture) { $Architecture.Name })
    $Payload = Resolve-FullPath $Payload
    $runtime = Join-Path $Payload 'lib/jiffshot'
    New-Directory $runtime -MustNotExist
    foreach ($name in $script:RuntimeFiles) {
        $destination = Join-Path $runtime $name
        [System.IO.File]::Copy((Join-Path $BuildDir $name), $destination, $true)
        Set-UnixMode $destination $script:Mode755
        [void](Invoke-JiffshotNativeCommand -Command @('strip', '--strip-debug', $destination))
        [void](Confirm-JiffshotElfBinary -Path $destination -Expected $detected -Shared:($name -cne 'jiffshot'))
    }
    $files = @(
        @((Join-Path $script:Assets 'jiffshot'), 'bin/jiffshot', $script:Mode755),
        @((Join-Path $script:Assets 'com.devolutions.Jiffshot.desktop'), 'share/applications/com.devolutions.Jiffshot.desktop', $script:Mode644),
        @((Join-Path $script:Root 'assets/jiffshot.svg'), 'share/icons/hicolor/scalable/apps/com.devolutions.Jiffshot.svg', $script:Mode644),
        @((Join-Path $script:Assets 'com.devolutions.Jiffshot.xml'), 'share/mime/packages/com.devolutions.Jiffshot.xml', $script:Mode644),
        @((Join-Path $script:Assets 'README.txt'), 'share/doc/jiffshot/README-linux.txt', $script:Mode644),
        @((Join-Path $script:Root 'README.md'), 'share/doc/jiffshot/README.md', $script:Mode644)
    )
    foreach ($file in $files) {
        $destination = Join-Path $Payload $file[1]
        New-Directory ([System.IO.Path]::GetDirectoryName($destination))
        Write-Utf8Text $destination (Read-NormalizedText $file[0])
        Set-UnixMode $destination $file[2]
    }
    foreach ($directory in @($Payload) + @([System.IO.Directory]::GetDirectories($Payload, '*', [System.IO.SearchOption]::AllDirectories))) {
        Set-UnixMode $directory $script:Mode755
    }
    [void](Invoke-JiffshotNativeCommand -Command @('desktop-file-validate', (Join-Path $Payload 'share/applications/com.devolutions.Jiffshot.desktop')))
}

function Write-TarField([byte[]]$Buffer, [int]$Offset, [int]$Length, [string]$Value) {
    # Python tarfile stn(): ASCII text truncated or NUL padded to the field length.
    $bytes = [System.Text.Encoding]::ASCII.GetBytes($Value)
    [System.Array]::Copy($bytes, 0, $Buffer, $Offset, [Math]::Min($bytes.Length, $Length))
}

function Write-TarNumber([byte[]]$Buffer, [int]$Offset, [int]$Length, [long]$Value) {
    # Python tarfile itn() for POSIX formats: zero-padded octal followed by NUL.
    if ($Value -lt 0 -or $Value -ge [Math]::Pow(8, $Length - 1)) {
        throw (New-PackagingError 'overflow in tar number field')
    }
    Write-TarField $Buffer $Offset $Length ([Convert]::ToString($Value, 8).PadLeft($Length - 1, '0'))
}

function New-TarHeader([string]$Name, [int]$Mode, [long]$Size, [long]$Mtime, [char]$Type, [string]$User) {
    $header = [byte[]]::new(512)
    Write-TarField $header 0 100 $Name
    Write-TarNumber $header 100 8 $Mode
    Write-TarNumber $header 108 8 0
    Write-TarNumber $header 116 8 0
    Write-TarNumber $header 124 12 $Size
    Write-TarNumber $header 136 12 $Mtime
    Write-TarField $header 148 8 '        '
    $header[156] = [byte]$Type
    Write-TarField $header 257 8 "ustar`000"
    Write-TarField $header 265 32 $User
    Write-TarField $header 297 32 $User
    $checksum = 0
    foreach ($byte in $header) { $checksum += $byte }
    Write-TarField $header 148 7 ([Convert]::ToString($checksum, 8).PadLeft(6, '0') + "`0")
    $header
}

function Write-TarPadding([System.IO.Stream]$Stream, [long]$Length, [int]$Block) {
    $remainder = $Length % $Block
    if ($remainder) {
        $Stream.Write([byte[]]::new($Block - $remainder), 0, $Block - $remainder)
    }
}

function Write-TarEntry([System.IO.Stream]$Stream, [string]$Name, [int]$Mode, [long]$Size, [long]$Mtime, [char]$Type) {
    # Returns the number of header bytes written so the archive can be padded like Python's tarfile.
    $written = 0L
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($Name)
    $ascii = -not ($Name.ToCharArray() | Where-Object { [int]$_ -gt 127 })
    if (-not $ascii -or $Name.Length -gt 100) {
        # Same PAX "path" extended header as Python's default tarfile format.
        $length = 'path'.Length + $bytes.Length + 3
        $total = 0
        $previous = 0
        while ($true) {
            $total = $length + "$previous".Length
            if ($total -eq $previous) { break }
            $previous = $total
        }
        $record = [System.Text.Encoding]::UTF8.GetBytes("$total path=") + $bytes + [byte[]](10)
        $Stream.Write((New-TarHeader '././@PaxHeader' 0 $record.Length 0 'x' ''), 0, 512)
        $Stream.Write($record, 0, $record.Length)
        Write-TarPadding $Stream $record.Length 512
        $written += 512 + $record.Length + ((512 - $record.Length % 512) % 512)
        $Name = -join ($Name.ToCharArray() | ForEach-Object { if ([int]$_ -gt 127) { '?' } else { $_ } })
    }
    $Stream.Write((New-TarHeader $Name $Mode $Size $Mtime $Type 'root'), 0, 512)
    $written + 512
}

function Write-JiffshotArchive {
    <#
    .SYNOPSIS
    Writes a reproducible gzip-compressed ustar archive with root ownership and normalized modes.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$Payload,
        [Parameter(Mandatory)][string]$Destination,
        [Parameter(Mandatory)][string]$TopDirectory,
        [Parameter(Mandatory)][long]$Epoch
    )
    $Payload = Resolve-FullPath $Payload
    $Destination = Resolve-FullPath $Destination
    $relatives = [string[]]@(
        [System.IO.Directory]::GetFileSystemEntries($Payload, '*', [System.IO.SearchOption]::AllDirectories) |
            ForEach-Object { [System.IO.Path]::GetRelativePath($Payload, $_).Replace('\', '/') }
    )
    # Python sorts Path objects component by component, so sort with a separator below every character.
    $keys = [string[]]@($relatives | ForEach-Object { $_.Replace('/', "`0") })
    [System.Array]::Sort($keys, $relatives, [System.StringComparer]::Ordinal)
    $output = [System.IO.File]::Create($Destination)
    try {
        $compressed = [System.IO.Compression.GZipStream]::new($output, [System.IO.Compression.CompressionLevel]::SmallestSize, $true)
        try {
            $offset = 0L
            foreach ($relative in @('.') + $relatives) {
                $path = if ($relative -eq '.') { $Payload } else { Join-Path $Payload $relative }
                $name = if ($relative -eq '.') { $TopDirectory } else { "$TopDirectory/$relative" }
                $name = $name.TrimStart('/')
                $item = Get-Item -LiteralPath $path -Force
                if ($item.LinkTarget) {
                    throw (New-PackagingError "$path is a symbolic link; payloads contain only files and directories")
                }
                if ($item -is [System.IO.DirectoryInfo]) {
                    $offset += Write-TarEntry $compressed "$name/" $script:Mode755 0 $Epoch '5'
                    continue
                }
                $mode = if ([int]$item.UnixFileMode -band 0x49) { $script:Mode755 } else { $script:Mode644 }
                $size = $item.Length
                $offset += Write-TarEntry $compressed $name $mode $size $Epoch '0'
                $content = [System.IO.File]::OpenRead($path)
                try {
                    $content.CopyTo($compressed)
                }
                finally {
                    $content.Dispose()
                }
                Write-TarPadding $compressed $size 512
                $offset += $size + ((512 - $size % 512) % 512)
            }
            $compressed.Write([byte[]]::new(1024), 0, 1024)
            Write-TarPadding $compressed ($offset + 1024) 10240
        }
        finally {
            $compressed.Dispose()
        }
        # GZipStream always writes MTIME 0; record the requested epoch, best-compression XFL,
        # and an unknown OS like Python's gzip so output is identical on every host.
        $output.Position = 0
        $prefix = [byte[]]::new(4)
        [void]$output.Read($prefix, 0, 4)
        if ($prefix[0] -ne 0x1F -or $prefix[1] -ne 0x8B -or $prefix[2] -ne 8 -or $prefix[3] -ne 0) {
            throw (New-PackagingError 'unexpected gzip header from the compression library')
        }
        $output.Position = 4
        $output.Write([System.BitConverter]::GetBytes([uint32]$Epoch), 0, 4)
        $output.Write([byte[]](2, 255), 0, 2)
    }
    finally {
        $output.Dispose()
    }
}

function Copy-DirectoryTree([string]$Source, [string]$Destination) {
    # Equivalent to shutil.copytree(): contents, permissions, and modification times.
    New-Directory $Destination -MustNotExist
    foreach ($entry in [System.IO.Directory]::GetFileSystemEntries($Source)) {
        $target = Join-Path $Destination ([System.IO.Path]::GetFileName($entry))
        if ([System.IO.Directory]::Exists($entry)) {
            Copy-DirectoryTree $entry $target
        }
        else {
            [System.IO.File]::Copy($entry, $target)
            [System.IO.File]::SetUnixFileMode($target, [System.IO.File]::GetUnixFileMode($entry))
            [System.IO.File]::SetLastWriteTimeUtc($target, [System.IO.File]::GetLastWriteTimeUtc($entry))
        }
    }
    ([System.IO.DirectoryInfo]::new($Destination)).UnixFileMode = ([System.IO.DirectoryInfo]::new($Source)).UnixFileMode
    [System.IO.Directory]::SetLastWriteTimeUtc($Destination, [System.IO.Directory]::GetLastWriteTimeUtc($Source))
}

function New-JiffshotDeb {
    <#
    .SYNOPSIS
    Builds the Debian package from a staged payload and returns its path.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$Payload,
        [Parameter(Mandatory)][string]$Work,
        [Parameter(Mandatory)][string]$Version,
        [Parameter(Mandatory)][object]$Architecture
    )
    $root = Join-Path $Work 'deb'
    Copy-DirectoryTree $Payload (Join-Path $root 'usr')
    Set-UnixMode $root $script:Mode755
    $metadata = Join-Path $Work 'metadata/debian'
    New-Directory $metadata -MustNotExist
    Write-Utf8Text (Join-Path $metadata 'control') (
        "Source: jiffshot`nSection: graphics`nPriority: optional`nMaintainer: $script:Maintainer`n`n" +
        "Package: jiffshot`nArchitecture: $($Architecture.Deb)`nDescription: Jiffshot screen capture and editor`n")
    # Scan every ELF, including the runtime loaded with dlopen. System package
    # names and minimum symbol versions come from the build distro's shlibs data.
    $scan = @('dpkg-shlibdeps', '-O') + @($script:RuntimeFiles | ForEach-Object { "-e$(Join-Path $root "usr/lib/jiffshot/$_")" })
    $dependencies = ([string](Invoke-JiffshotNativeCommand -Command $scan -WorkingDirectory (Split-Path $metadata -Parent)).Stdout).Trim()
    $prefix = 'shlibs:Depends='
    if (-not $dependencies.StartsWith($prefix, [System.StringComparison]::Ordinal)) {
        throw (New-PackagingError "dpkg-shlibdeps returned unexpected dependency metadata: $dependencies")
    }
    $dependencies = (@($dependencies.Substring($prefix.Length)) + $script:DebRuntime) -join ', '
    $size = 0L
    foreach ($file in [System.IO.Directory]::GetFiles($Payload, '*', [System.IO.SearchOption]::AllDirectories)) {
        $size += ([System.IO.FileInfo]::new($file)).Length
    }
    $installedSize = [long][Math]::Floor(($size + 1023) / 1024)
    $control = Join-Path $root 'DEBIAN/control'
    New-Directory (Split-Path $control -Parent) -MustNotExist
    Set-UnixMode (Split-Path $control -Parent) $script:Mode755
    Write-Utf8Text $control (
        "Package: jiffshot`nVersion: $Version`nArchitecture: $($Architecture.Deb)`n" +
        "Maintainer: $script:Maintainer`nSection: graphics`nPriority: optional`n" +
        "Installed-Size: $installedSize`nDepends: $dependencies`n" +
        "Homepage: https://github.com/Devolutions/jiffshot`n" +
        "Description: Screen capture, recording, and non-destructive image editor`n" +
        " Linux image/document editing, OCR, portal capture, and video tools.`n" +
        " Screen capture requires a running XDG desktop portal and PipeWire.`n")
    Set-UnixMode $control $script:Mode644
    $destination = Join-Path $Work "jiffshot_${Version}_$($Architecture.Deb).deb"
    [void](Invoke-JiffshotNativeCommand -Command @('dpkg-deb', '--build', '--root-owner-group', '-Zxz', $root, $destination))
    $destination
}

function New-JiffshotRpm {
    <#
    .SYNOPSIS
    Builds the Fedora RPM from a staged payload and returns its path.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$Payload,
        [Parameter(Mandatory)][string]$Work,
        [Parameter(Mandatory)][string]$Version,
        [Parameter(Mandatory)][long]$Epoch,
        [Parameter(Mandatory)][object]$Architecture
    )
    $topdir = Join-Path $Work 'rpm'
    foreach ($name in 'BUILD', 'BUILDROOT', 'RPMS', 'SOURCES', 'SPECS', 'SRPMS') {
        New-Directory (Join-Path $topdir $name) -MustNotExist
    }
    Write-JiffshotArchive -Payload $Payload -Destination (Join-Path $topdir 'SOURCES/payload.tar.gz') -TopDirectory "jiffshot-$Version" -Epoch $Epoch
    $spec = (Read-NormalizedText (Join-Path $script:Assets 'jiffshot.spec')).Replace('@VERSION@', $Version).Replace('@ARCH@', $Architecture.Rpm)
    $specPath = Join-Path $topdir 'SPECS/jiffshot.spec'
    Write-Utf8Text $specPath $spec
    [void](Invoke-JiffshotNativeCommand -Command @(
            'rpmbuild', '-bb', '--target', $Architecture.Rpm, '--define', "_topdir $topdir",
            '--define', 'use_source_date_epoch_as_buildtime 1',
            '--define', 'clamp_mtime_to_source_date_epoch 1', $specPath))
    $name = "jiffshot-$Version-1.$($Architecture.Rpm).rpm"
    $destination = Join-Path $topdir "RPMS/$($Architecture.Rpm)/$name"
    if (-not [System.IO.File]::Exists($destination)) {
        throw (New-PackagingError "rpmbuild did not produce $name")
    }
    $destination
}

function Get-JiffshotSha256 {
    <#
    .SYNOPSIS
    Returns the lowercase hexadecimal SHA-256 digest of a file.
    #>
    [CmdletBinding()]
    param([Parameter(Mandatory)][string]$Path)
    $full = Resolve-FullPath $Path
    Assert-FileExists $full
    $stream = [System.IO.File]::OpenRead($full)
    try {
        [System.Convert]::ToHexString([System.Security.Cryptography.SHA256]::HashData($stream)).ToLowerInvariant()
    }
    finally {
        $stream.Dispose()
    }
}

function Get-SourceDateEpoch {
    $value = [System.Environment]::GetEnvironmentVariable('SOURCE_DATE_EPOCH')
    if ($null -eq $value) { $value = '0' }
    # Same accepted syntax as Python's int(): optional sign, digits, and single underscores.
    if ($value -cnotmatch '\A\s*[+-]?[0-9]+(_[0-9]+)*\s*\z') {
        throw (New-PackagingError "invalid literal for int() with base 10: '$value'")
    }
    $epoch = [System.Numerics.BigInteger]::Parse($value.Trim().Replace('_', ''), [System.Globalization.CultureInfo]::InvariantCulture)
    if ($epoch -lt 0 -or $epoch -gt [uint32]::MaxValue) {
        throw (New-PackagingError 'SOURCE_DATE_EPOCH must fit an unsigned 32-bit gzip timestamp')
    }
    [long]$epoch
}

function Invoke-JiffshotLinuxPackageBuild {
    <#
    .SYNOPSIS
    Package a Linux release build with its private Rustolonia runtime.
    #>
    [CmdletBinding()]
    param(
        [string]$BuildDir = (Join-Path $script:Root 'target/release'),
        [string]$OutputDir = (Join-Path $script:Root 'package/out/linux'),
        [string]$Version = $env:JIFFSHOT_VERSION,
        [ValidateSet('tar.gz', 'deb', 'rpm', IgnoreCase = $false)]
        [string[]]$Format = @('tar.gz', 'deb'),
        [ValidateSet('x64', 'arm64', IgnoreCase = $false)]
        [string]$Arch
    )
    if (-not (Test-JiffshotLinuxHost)) {
        throw (New-PackagingError 'run this packaging script on Linux (or in WSL)')
    }
    $Version = Get-JiffshotPackageVersion -Expected $Version
    $formats = [System.Collections.Generic.List[string]]::new()
    foreach ($item in $Format) {
        if (-not $formats.Contains($item)) { $formats.Add($item) }
    }
    if ($formats.Contains('deb') -and $formats.Contains('rpm')) {
        throw (New-PackagingError 'DEB and Fedora RPM require separate native builds; generate them in separate invocations')
    }
    $BuildDir = Resolve-FullPath $BuildDir
    $architecture = Confirm-JiffshotBuild -BuildDir $BuildDir -Arch $Arch
    Confirm-JiffshotPackagingTool -Format $formats
    if ($formats.Contains('rpm')) {
        Confirm-JiffshotRpmInput -BuildDir $BuildDir
    }
    $epoch = Get-SourceDateEpoch
    $previousEpoch = [System.Environment]::GetEnvironmentVariable('SOURCE_DATE_EPOCH')
    $work = [System.IO.Directory]::CreateTempSubdirectory('jiffshot-package-').FullName
    try {
        [System.Environment]::SetEnvironmentVariable('SOURCE_DATE_EPOCH', "$epoch")
        $payload = Join-Path $work 'payload'
        New-JiffshotPayload -BuildDir $BuildDir -Payload $payload -Architecture $architecture
        $artifacts = [System.Collections.Generic.List[string]]::new()
        if ($formats.Contains('tar.gz')) {
            $destination = Join-Path $work "Devolutions.Jiffshot-linux-$($architecture.Name)-$Version.tar.gz"
            Write-JiffshotArchive -Payload $payload -Destination $destination -TopDirectory "jiffshot-$Version-linux-$($architecture.Name)" -Epoch $epoch
            $artifacts.Add($destination)
        }
        if ($formats.Contains('deb')) {
            $artifacts.Add((New-JiffshotDeb -Payload $payload -Work $work -Version $Version -Architecture $architecture))
        }
        if ($formats.Contains('rpm')) {
            $artifacts.Add((New-JiffshotRpm -Payload $payload -Work $work -Version $Version -Epoch $epoch -Architecture $architecture))
        }
        $OutputDir = Resolve-FullPath $OutputDir
        New-Directory $OutputDir
        $checksums = foreach ($path in $artifacts) {
            "$(Get-JiffshotSha256 $path)  $([System.IO.Path]::GetFileName($path))`n"
        }
        $sums = Join-Path $work "SHA256SUMS-linux-$($architecture.Name).txt"
        [System.IO.File]::WriteAllText($sums, (-join $checksums), [System.Text.Encoding]::ASCII)
        $publishing = Join-Path $OutputDir ('.jiffshot-' + [System.IO.Path]::GetRandomFileName().Replace('.', ''))
        [void][System.IO.Directory]::CreateDirectory($publishing, [System.IO.UnixFileMode]$script:Mode700)
        try {
            foreach ($artifact in @($artifacts) + $sums) {
                $name = [System.IO.Path]::GetFileName($artifact)
                $staged = Join-Path $publishing $name
                [System.IO.File]::Copy($artifact, $staged, $true)
                Set-UnixMode $staged $script:Mode644
                $published = Join-Path $OutputDir $name
                [System.IO.File]::Move($staged, $published, $true)
                $size = ([System.IO.FileInfo]::new($artifact)).Length
                [string]::Format([System.Globalization.CultureInfo]::InvariantCulture, '{0} ({1:N0} bytes)', $published, $size)
            }
        }
        finally {
            Remove-Item -LiteralPath $publishing -Recurse -Force -ErrorAction Ignore
        }
    }
    finally {
        [System.Environment]::SetEnvironmentVariable('SOURCE_DATE_EPOCH', $previousEpoch)
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction Ignore
    }
}

function Split-PythonLines([string]$Text) {
    # Python str.splitlines() for ASCII text.
    if ($Text.Length -eq 0) { return , @() }
    $lines = [System.Collections.Generic.List[string]]::new([string[]]($Text -split "\r\n|[\n\r\x0b\x0c\x1c\x1d\x1e]"))
    if ($Text -match '[\n\r\x0b\x0c\x1c\x1d\x1e]\z') {
        $lines.RemoveAt($lines.Count - 1)
    }
    , $lines.ToArray()
}

function Read-JiffshotChecksumManifest {
    <#
    .SYNOPSIS
    Parses a "<sha256>  <name>" manifest that must cover exactly the expected file names.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string[]]$ExpectedName
    )
    $full = Resolve-FullPath $Path
    Assert-FileExists $full
    $bytes = [System.IO.File]::ReadAllBytes($full)
    if ($bytes | Where-Object { $_ -gt 127 }) {
        throw (New-PackagingError "$Path is not an ASCII checksum manifest")
    }
    $records = [System.Collections.Generic.Dictionary[string, string]]::new([System.StringComparer]::Ordinal)
    foreach ($line in (Split-PythonLines ([System.Text.Encoding]::ASCII.GetString($bytes)))) {
        $match = [regex]::Match($line, '\A([0-9a-f]{64})  ([^/\\]+)\z')
        if (-not $match.Success) {
            throw (New-PackagingError "invalid checksum record in $Path")
        }
        $digest, $name = $match.Groups[1].Value, $match.Groups[2].Value
        if ($records.ContainsKey($name)) {
            throw (New-PackagingError "duplicate checksum for $name in $Path")
        }
        $records[$name] = $digest
    }
    $expected = [System.Collections.Generic.HashSet[string]]::new([string[]]$ExpectedName, [System.StringComparer]::Ordinal)
    if (-not $expected.SetEquals([string[]]@($records.Keys))) {
        throw (New-PackagingError "$Path must cover exactly $($ExpectedName -join ', ')")
    }
    , $records
}

function Publish-JiffshotLinuxRelease {
    <#
    .SYNOPSIS
    Require and verify all six Linux packages before adding them to a release.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$ArtifactDir,
        [Parameter(Mandatory)][string]$OutputDir,
        [Parameter(Mandatory)][string]$Version
    )
    $Version = Get-JiffshotPackageVersion -Expected $Version
    $ArtifactDir = Resolve-FullPath $ArtifactDir
    $OutputDir = Resolve-FullPath $OutputDir
    $verified = [System.Collections.Generic.List[string]]::new()
    $merged = [ordered]@{}
    foreach ($architecture in $script:Architectures.Values) {
        $arch = $architecture.Name
        $records = [System.Collections.Generic.Dictionary[string, string]]::new([System.StringComparer]::Ordinal)
        foreach ($group in @(
                @('', @("Devolutions.Jiffshot-linux-$arch-$Version.tar.gz", "jiffshot_${Version}_$($architecture.Deb).deb")),
                @('-rpm', @("jiffshot-$Version-1.$($architecture.Rpm).rpm")))) {
            $directory = Join-Path $ArtifactDir "jiffshot-linux-$arch$($group[0])"
            $names = [string[]]$group[1]
            $checksums = Read-JiffshotChecksumManifest -Path (Join-Path $directory "SHA256SUMS-linux-$arch.txt") -ExpectedName $names
            foreach ($name in $names) {
                $source = Join-Path $directory $name
                $actual = Get-JiffshotSha256 $source
                if ($actual -cne $checksums[$name]) {
                    throw (New-PackagingError "checksum mismatch for $source")
                }
                $verified.Add($source)
                $records[$name] = $actual
            }
        }
        $merged[$arch] = $records
    }
    # No output is touched until all architectures, formats, and hashes pass.
    New-Directory $OutputDir
    foreach ($source in $verified) {
        [System.IO.File]::Copy($source, (Join-Path $OutputDir ([System.IO.Path]::GetFileName($source))), $true)
    }
    foreach ($arch in $merged.Keys) {
        $names = [string[]]@($merged[$arch].Keys)
        [System.Array]::Sort($names, [System.StringComparer]::Ordinal)
        $content = -join ($names | ForEach-Object { "$($merged[$arch][$_])  $_`n" })
        [System.IO.File]::WriteAllText((Join-Path $OutputDir "SHA256SUMS-linux-$arch.txt"), $content, [System.Text.Encoding]::ASCII)
    }
    $verified | ForEach-Object { [System.IO.Path]::GetFileName($_) }
}

Export-ModuleMember -Function @(
    'Get-JiffshotPackageInfo'
    'Get-JiffshotLinuxArchitecture'
    'Invoke-JiffshotNativeCommand'
    'Get-JiffshotPackageVersion'
    'Confirm-JiffshotElfBinary'
    'Confirm-JiffshotBuild'
    'Confirm-JiffshotPackagingTool'
    'Confirm-JiffshotRpmInput'
    'New-JiffshotPayload'
    'Write-JiffshotArchive'
    'New-JiffshotDeb'
    'New-JiffshotRpm'
    'Get-JiffshotSha256'
    'Invoke-JiffshotLinuxPackageBuild'
    'Read-JiffshotChecksumManifest'
    'Publish-JiffshotLinuxRelease'
)
