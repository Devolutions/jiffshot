#Requires -Version 7.4

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacPackage.psm1')

if (-not ('JiffshotFixture.LogPump' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Threading.Tasks;

namespace JiffshotFixture {
    public sealed class LogPump : IDisposable {
        private readonly StreamWriter writer;
        private readonly object gate = new object();

        public LogPump(string path, bool append) {
            var file = new FileStream(path, append ? FileMode.Append : FileMode.Create,
                FileAccess.Write, FileShare.ReadWrite);
            writer = new StreamWriter(file) { AutoFlush = true };
        }

        public async Task CopyFrom(StreamReader source) {
            var buffer = new char[4096];
            int count;
            while ((count = await source.ReadAsync(buffer, 0, buffer.Length)) != 0) {
                lock (gate) { writer.Write(buffer, 0, count); }
            }
        }

        public void Dispose() { writer.Dispose(); }
    }

    public static class Native {
        [DllImport("/usr/lib/libSystem.B.dylib", EntryPoint = "kill", SetLastError = true)]
        public static extern int Kill(int pid, int signal);
    }
}
'@
}

function New-FixtureRoot([string]$Output, [string]$Prefix) {
    if ($Output) {
        $root = [IO.Path]::GetFullPath($Output)
        [void](Invoke-MacCommand -File '/bin/mkdir' -Arguments @('--', $root))
    }
    else {
        $root = [IO.Directory]::CreateTempSubdirectory($Prefix).FullName
    }
    $resolved = Invoke-MacCommand -File '/bin/realpath' -Arguments @('--', $root)
    if (-not $resolved.Stdout.EndsWith("`n")) { throw "Could not resolve fixture directory $root" }
    return $resolved.Stdout.Substring(0, $resolved.Stdout.Length - 1)
}

function Resolve-FixtureFile([string]$Path) {
    $path = [IO.Path]::GetFullPath($Path)
    if (-not [IO.File]::Exists($path)) { throw "Fixture file not found: $path" }
    $resolved = Invoke-MacCommand -File '/bin/realpath' -Arguments @('--', $path)
    if (-not $resolved.Stdout.EndsWith("`n")) { throw "Could not resolve fixture file $path" }
    return $resolved.Stdout.Substring(0, $resolved.Stdout.Length - 1)
}

function Initialize-FixtureDirectories([string]$Root) {
    foreach ($name in 'library', 'config', 'local', 'appdata', 'localappdata', 'tmp', 'home') {
        $path = Join-Path $Root $name
        [void](Invoke-MacCommand -File '/bin/mkdir' -Arguments @('--', $path))
    }
}

function New-FixtureEnvironment([string]$Root, [string]$HostPath, [string]$Mode) {
    $environment = Get-CleanHostEnvironment
    foreach ($key in @($environment.Keys)) {
        if ($key.StartsWith('JIFFSHOT_', [StringComparison]::Ordinal)) {
            $environment.Remove($key)
        }
    }
    $environment['HOME'] = Join-Path $Root 'home'
    $environment['TMPDIR'] = Join-Path $Root 'tmp'
    $environment['JIFFSHOT_UITEST'] = $Root
    $environment[$Mode] = '1'
    $environment['JIFFSHOT_LIBRARY'] = Join-Path $Root 'library'
    $environment['JIFFSHOT_CONFIG_DIR'] = Join-Path $Root 'config'
    $environment['JIFFSHOT_LOCAL_DIR'] = Join-Path $Root 'local'
    $environment['APPDATA'] = Join-Path $Root 'appdata'
    $environment['LOCALAPPDATA'] = Join-Path $Root 'localappdata'
    $environment['RUSTOLONIA_HOST_LIB'] = $HostPath
    return $environment
}

function Start-FixtureProcess {
    param(
        [Parameter(Mandatory)][string]$File,
        [string[]]$Arguments = @(),
        [System.Collections.IDictionary]$Environment,
        [string]$LogPath,
        [switch]$AppendLog
    )
    $info = [Diagnostics.ProcessStartInfo]::new($File)
    foreach ($argument in $Arguments) { $info.ArgumentList.Add([string]$argument) }
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    if ($null -ne $Environment) {
        $info.Environment.Clear()
        foreach ($key in $Environment.Keys) { $info.Environment[[string]$key] = [string]$Environment[$key] }
    }
    $pump = if ($LogPath) { [JiffshotFixture.LogPump]::new($LogPath, [bool]$AppendLog) } else { $null }
    try { $process = [Diagnostics.Process]::Start($info) }
    catch {
        if ($pump) { $pump.Dispose() }
        throw
    }
    if ($pump) {
        $stdout = $pump.CopyFrom($process.StandardOutput)
        $stderr = $pump.CopyFrom($process.StandardError)
    }
    else {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
    }
    return [pscustomobject]@{
        Process = $process
        File = $File
        Stdout = $stdout
        Stderr = $stderr
        Pump = $pump
        Completed = $false
        ExitCode = $null
        Stopped = $false
    }
}

function Wait-FixtureProcess($Fixture, [int]$TimeoutSeconds) {
    if (-not $Fixture.Process.WaitForExit($TimeoutSeconds * 1000)) {
        throw "Fixture process $($Fixture.Process.Id) did not exit within $TimeoutSeconds seconds"
    }
    $Fixture.Process.WaitForExit()
    $stdout = $Fixture.Stdout.GetAwaiter().GetResult()
    $stderr = $Fixture.Stderr.GetAwaiter().GetResult()
    if ($Fixture.Pump) {
        $Fixture.Pump.Dispose()
        $Fixture.Pump = $null
        $stdout = ''
        $stderr = ''
    }
    $Fixture.Completed = $true
    $Fixture.ExitCode = $Fixture.Process.ExitCode
    return [pscustomobject]@{ ExitCode = $Fixture.Process.ExitCode; Stdout = $stdout; Stderr = $stderr }
}

function Stop-FixtureProcess($Fixture) {
    if ($null -eq $Fixture) { return }
    if ($Fixture.Stopped) { return }
    if (-not $Fixture.Process.get_HasExited()) {
        if ([JiffshotFixture.Native]::Kill($Fixture.Process.Id, 15) -ne 0 -and -not $Fixture.Process.get_HasExited()) {
            throw "Could not terminate fixture process $($Fixture.Process.Id)"
        }
        if (-not $Fixture.Process.WaitForExit(10000)) {
            $Fixture.Process.Kill()
            $Fixture.Process.WaitForExit()
        }
    }
    if (-not $Fixture.Completed) { [void](Wait-FixtureProcess $Fixture 10) }
    $Fixture.Process.Dispose()
    $Fixture.Stopped = $true
}

function Read-FixtureJson([string]$Path) {
    return [IO.File]::ReadAllText($Path) | ConvertFrom-Json -AsHashtable -Depth 100
}

function Read-FixtureZipJson([string]$Path, [string]$Entry) {
    $archive = [IO.Compression.ZipFile]::OpenRead($Path)
    try {
        $item = $archive.GetEntry($Entry)
        if ($null -eq $item) { throw "Missing $Entry in $Path" }
        $reader = [IO.StreamReader]::new($item.Open())
        try { return $reader.ReadToEnd() | ConvertFrom-Json -AsHashtable -Depth 100 }
        finally { $reader.Dispose() }
    }
    finally { $archive.Dispose() }
}

function Assert-FixtureMarkers([string]$Path, [string[]]$Markers) {
    $log = [IO.File]::ReadAllText($Path)
    if ($log.Contains('FAILED:')) { throw "The UI fixture reported a failure; inspect $Path" }
    foreach ($marker in $Markers) {
        if (-not $log.Contains($marker)) { throw "The UI fixture did not report '$marker'; inspect $Path" }
    }
}

function Remove-FixtureRoot([string]$Root) {
    [IO.Directory]::Delete($Root, $true)
}

Export-ModuleMember -Function New-FixtureRoot, Resolve-FixtureFile, Initialize-FixtureDirectories, New-FixtureEnvironment, `
    Start-FixtureProcess, Wait-FixtureProcess, Stop-FixtureProcess, Read-FixtureJson, Read-FixtureZipJson, `
    Assert-FixtureMarkers, Remove-FixtureRoot
