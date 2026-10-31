# Shared helpers for the Linux packaging Pester tests.

$script:PackageModule = Join-Path $PSScriptRoot 'JiffshotLinuxPackage.psm1'
$script:Assets = $PSScriptRoot
$script:Root = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$script:RuntimeFiles = @('jiffshot', 'librustolonia_host.so', 'libSkiaSharp.so', 'libHarfBuzzSharp.so')
$script:ExpectedFiles = @(
    $script:RuntimeFiles | ForEach-Object { "lib/jiffshot/$_" }
    'bin/jiffshot'
    'share/applications/com.devolutions.Jiffshot.desktop'
    'share/icons/hicolor/scalable/apps/com.devolutions.Jiffshot.svg'
    'share/mime/packages/com.devolutions.Jiffshot.xml'
    'share/doc/jiffshot/README-linux.txt'
    'share/doc/jiffshot/README.md'
) | Sort-Object -CaseSensitive

function New-ElfHeader([int]$Machine = 62, [int]$ElfType = 3) {
    # Synthetic ELF64 header, never an executable runtime fixture.
    $header = [byte[]]::new(64)
    [byte[]](0x7F, 0x45, 0x4C, 0x46, 0x02, 0x01, 0x01) | ForEach-Object -Begin { $index = 0 } -Process { $header[$index++] = $_ }
    [System.BitConverter]::GetBytes([uint16]$ElfType).CopyTo($header, 16)
    [System.BitConverter]::GetBytes([uint16]$Machine).CopyTo($header, 18)
    [System.BitConverter]::GetBytes([uint32]1).CopyTo($header, 20)
    [System.BitConverter]::GetBytes([uint16]64).CopyTo($header, 52)
    , $header
}

function Join-Bytes([byte[]]$First, [byte[]]$Second) {
    $result = [byte[]]::new($First.Length + $Second.Length)
    $First.CopyTo($result, 0)
    $Second.CopyTo($result, $First.Length)
    , $result
}

function Get-AsciiBytes([string]$Text) {
    , [System.Text.Encoding]::ASCII.GetBytes($Text)
}

function Get-TestSha256([string]$Path) {
    [System.Convert]::ToHexString([System.Security.Cryptography.SHA256]::HashData([System.IO.File]::ReadAllBytes($Path))).ToLowerInvariant()
}

function Get-BytesSha256([byte[]]$Bytes) {
    [System.Convert]::ToHexString([System.Security.Cryptography.SHA256]::HashData($Bytes)).ToLowerInvariant()
}

function Get-UnixMode([string]$Path) {
    $item = if ([System.IO.Directory]::Exists($Path)) { [System.IO.DirectoryInfo]::new($Path) } else { [System.IO.FileInfo]::new($Path) }
    [int]$item.UnixFileMode
}

function Set-TestUmask([int]$Mask) {
    if (-not ('Jiffshot.Tests.Native' -as [type])) {
        Add-Type -Namespace Jiffshot.Tests -Name Native -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("libc", SetLastError = true)]
public static extern uint umask(uint mask);
'@
    }
    [int][Jiffshot.Tests.Native]::umask([uint32]$Mask)
}

function New-TestDirectory([string]$Prefix = 'jiffshot-test-') {
    [System.IO.Directory]::CreateTempSubdirectory($Prefix).FullName
}

function Get-RelativeFiles([string]$Root) {
    [System.IO.Directory]::GetFiles($Root, '*', [System.IO.SearchOption]::AllDirectories) |
        ForEach-Object { [System.IO.Path]::GetRelativePath($Root, $_).Replace('\', '/') } |
        Sort-Object -CaseSensitive
}

function Read-TarEntries([System.IO.Stream]$Stream, [switch]$Content) {
    # Python-style member records; directory names lose their trailing slash.
    $reader = [System.Formats.Tar.TarReader]::new($Stream, $true)
    try {
        while ($null -ne ($entry = $reader.GetNextEntry())) {
            $data = $null
            if ($Content -and $null -ne $entry.DataStream) {
                $memory = [System.IO.MemoryStream]::new()
                $entry.DataStream.CopyTo($memory)
                $data = $memory.ToArray()
            }
            [pscustomobject]@{
                Name  = $entry.Name.TrimEnd('/')
                Type  = $entry.EntryType
                Mode  = [int]$entry.Mode
                Uid   = $entry.Uid
                Gid   = $entry.Gid
                Mtime = $entry.ModificationTime.ToUnixTimeSeconds()
                Data  = $data
            }
        }
    }
    finally {
        $reader.Dispose()
    }
}

function Read-TarGz([string]$Path, [switch]$Content) {
    $file = [System.IO.File]::OpenRead($Path)
    try {
        $gzip = [System.IO.Compression.GZipStream]::new($file, [System.IO.Compression.CompressionMode]::Decompress)
        try {
            Read-TarEntries $gzip -Content:$Content
        }
        finally {
            $gzip.Dispose()
        }
    }
    finally {
        $file.Dispose()
    }
}

function Expand-TarGz([string]$Path, [string]$Destination) {
    $file = [System.IO.File]::OpenRead($Path)
    try {
        $gzip = [System.IO.Compression.GZipStream]::new($file, [System.IO.Compression.CompressionMode]::Decompress)
        try {
            [System.Formats.Tar.TarFile]::ExtractToDirectory($gzip, $Destination, $false)
        }
        finally {
            $gzip.Dispose()
        }
    }
    finally {
        $file.Dispose()
    }
}

function Read-DesktopEntry([string]$Path) {
    # The subset of Python's configparser used by desktop entries.
    $sections = [System.Collections.Generic.Dictionary[string, hashtable]]::new()
    $section = $null
    foreach ($line in [System.IO.File]::ReadAllLines($Path)) {
        $trimmed = $line.Trim()
        if (-not $trimmed -or $trimmed.StartsWith('#') -or $trimmed.StartsWith(';')) { continue }
        if ($trimmed -match '\A\[(.+)\]\z') {
            $section = @{}
            $sections[$Matches[1]] = $section
            continue
        }
        $separator = $trimmed.IndexOf('=')
        $section[$trimmed.Substring(0, $separator).Trim()] = $trimmed.Substring($separator + 1).Trim()
    }
    $sections
}

function Split-ShellWords([string]$Text, [switch]$Comments) {
    # Python shlex.split(posix=True) with optional comments.
    $tokens = [System.Collections.Generic.List[string]]::new()
    $token = [System.Text.StringBuilder]::new()
    $inToken = $false
    $index = 0
    while ($index -lt $Text.Length) {
        $character = $Text[$index]
        if ($character -eq "'") {
            $inToken = $true
            $end = $Text.IndexOf("'", $index + 1)
            if ($end -lt 0) { throw 'No closing quotation' }
            [void]$token.Append($Text, $index + 1, $end - $index - 1)
            $index = $end + 1
        }
        elseif ($character -eq '"') {
            $inToken = $true
            $index++
            while ($true) {
                if ($index -ge $Text.Length) { throw 'No closing quotation' }
                $current = $Text[$index]
                if ($current -eq '"') { $index++; break }
                if ($current -eq '\' -and $index + 1 -lt $Text.Length -and $Text[$index + 1] -in '"', '\') {
                    [void]$token.Append($Text[$index + 1])
                    $index += 2
                    continue
                }
                [void]$token.Append($current)
                $index++
            }
        }
        elseif ($character -eq '\') {
            $inToken = $true
            if ($index + 1 -ge $Text.Length) { throw 'No escaped character' }
            [void]$token.Append($Text[$index + 1])
            $index += 2
        }
        elseif ($Comments -and $character -eq '#') {
            while ($index -lt $Text.Length -and $Text[$index] -ne "`n") { $index++ }
            if ($inToken) {
                $tokens.Add($token.ToString())
                [void]$token.Clear()
                $inToken = $false
            }
        }
        elseif ([char]::IsWhiteSpace($character)) {
            if ($inToken) {
                $tokens.Add($token.ToString())
                [void]$token.Clear()
                $inToken = $false
            }
            $index++
        }
        else {
            $inToken = $true
            [void]$token.Append($character)
            $index++
        }
    }
    if ($inToken) { $tokens.Add($token.ToString()) }
    , $tokens.ToArray()
}

function Assert-ThrowsMatch([scriptblock]$Script, [string]$Pattern, [type[]]$Type = @([System.IO.InvalidDataException])) {
    # Python assertRaisesRegex: case-sensitive regex search of the message.
    $failure = $null
    try { & $Script } catch { $failure = $_ }
    $failure | Should -Not -BeNullOrEmpty -Because "an error matching '$Pattern' was expected"
    @($Type | Where-Object { $failure.Exception -is $_ }).Count | Should -BeGreaterThan 0 -Because "$($failure.Exception.GetType()) is not one of $($Type -join ', ')"
    $failure.Exception.Message | Should -MatchExactly $Pattern
    $failure
}

function Get-WorkflowText([string]$Name) {
    [System.IO.File]::ReadAllText((Join-Path $script:Root ".github/workflows/$Name")).Replace("`r`n", "`n")
}

function Get-WorkflowJob([string]$Name) {
    $workflow = Get-WorkflowText 'ci.yml'
    $match = [regex]::Match($workflow, "(?ms)^  $([regex]::Escape($Name)):\n(.*?)(?=^  [a-zA-Z0-9_-]+:|\z)")
    if (-not $match.Success) { throw "missing job $Name" }
    $match.Groups[1].Value
}

function Get-WorkflowStepScript([string]$Job, [string]$Name) {
    $match = [regex]::Match($Job, "(?ms)^      - name: $([regex]::Escape($Name))\n        run: (?:\|\n(.*?)(?=^      - |\z)|([^\n]+))")
    if (-not $match.Success) { throw "missing step $Name" }
    if ($match.Groups[1].Success) { $match.Groups[1].Value } else { $match.Groups[2].Value }
}

function Invoke-TestScript([string]$Script, [string[]]$Arguments) {
    $info = [System.Diagnostics.ProcessStartInfo]::new([System.Environment]::ProcessPath)
    foreach ($argument in @('-NoProfile', '-NonInteractive', '-File', $Script) + $Arguments) {
        $info.ArgumentList.Add($argument)
    }
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.WorkingDirectory = (Get-Location -PSProvider FileSystem).ProviderPath
    $process = [System.Diagnostics.Process]::Start($info)
    try {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        [pscustomobject]@{
            ExitCode = $process.ExitCode
            Stdout   = $stdout.GetAwaiter().GetResult()
            Stderr   = $stderr.GetAwaiter().GetResult()
        }
    }
    finally {
        $process.Dispose()
    }
}
