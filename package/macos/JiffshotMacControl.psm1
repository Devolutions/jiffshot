#Requires -Version 7.4

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacPackage.psm1')
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacFixture.psm1')

if (-not ('JiffshotFixture.Png' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Buffers.Binary;
using System.IO;
using System.IO.Compression;

namespace JiffshotFixture {
    public static class Png {
        private static readonly byte[] Signature = { 137, 80, 78, 71, 13, 10, 26, 10 };

        private static int Paeth(int left, int above, int upperLeft) {
            int estimate = left + above - upperLeft;
            int a = Math.Abs(estimate - left);
            int b = Math.Abs(estimate - above);
            int c = Math.Abs(estimate - upperLeft);
            return a <= b && a <= c ? left : b <= c ? above : upperLeft;
        }

        public static int[] Center(string path) {
            byte[] data = File.ReadAllBytes(path);
            if (data.Length < Signature.Length || !data.AsSpan(0, Signature.Length).SequenceEqual(Signature))
                throw new InvalidDataException("not a PNG: " + path);
            int width = 0, height = 0, channels = 0, offset = 8;
            bool ended = false;
            using var compressed = new MemoryStream();
            while (offset < data.Length) {
                if (data.Length - offset < 12) throw new InvalidDataException("truncated PNG chunk: " + path);
                uint size = BinaryPrimitives.ReadUInt32BigEndian(data.AsSpan(offset, 4));
                if (size > int.MaxValue || size > data.Length - offset - 12)
                    throw new InvalidDataException("invalid PNG chunk length: " + path);
                int length = (int)size;
                string kind = System.Text.Encoding.ASCII.GetString(data, offset + 4, 4);
                int payload = offset + 8;
                offset += length + 12;
                if (kind == "IHDR") {
                    if (length != 13) throw new InvalidDataException("invalid PNG header: " + path);
                    width = checked((int)BinaryPrimitives.ReadUInt32BigEndian(data.AsSpan(payload, 4)));
                    height = checked((int)BinaryPrimitives.ReadUInt32BigEndian(data.AsSpan(payload + 4, 4)));
                    byte depth = data[payload + 8], color = data[payload + 9];
                    if (depth != 8 || (color != 2 && color != 6) ||
                        data[payload + 10] != 0 || data[payload + 11] != 0 || data[payload + 12] != 0)
                        throw new InvalidDataException("unsupported fixture PNG format: " + path);
                    channels = color == 2 ? 3 : 4;
                } else if (kind == "IDAT") {
                    compressed.Write(data, payload, length);
                } else if (kind == "IEND") {
                    ended = true;
                    break;
                }
            }
            if (width <= 0 || height <= 0 || channels == 0 || !ended)
                throw new InvalidDataException("missing PNG dimensions or end: " + path);
            int stride = checked(width * channels);
            int expected = checked((stride + 1) * height);
            compressed.Position = 0;
            using var inflater = new ZLibStream(compressed, CompressionMode.Decompress);
            using var decoded = new MemoryStream();
            inflater.CopyTo(decoded);
            byte[] rows = decoded.ToArray();
            if (rows.Length != expected) throw new InvalidDataException("unexpected PNG row data: " + path);
            var previous = new byte[stride];
            for (int y = 0; y <= height / 2; y++) {
                int start = y * (stride + 1);
                int filter = rows[start];
                var row = new byte[stride];
                Array.Copy(rows, start + 1, row, 0, stride);
                for (int i = 0; i < stride; i++) {
                    int left = i >= channels ? row[i - channels] : 0;
                    int above = previous[i];
                    int upperLeft = i >= channels ? previous[i - channels] : 0;
                    int predictor = filter switch {
                        0 => 0,
                        1 => left,
                        2 => above,
                        3 => (left + above) / 2,
                        4 => Paeth(left, above, upperLeft),
                        _ => throw new InvalidDataException("unsupported PNG filter: " + filter),
                    };
                    row[i] = unchecked((byte)(row[i] + predictor));
                }
                previous = row;
            }
            int center = (width / 2) * channels;
            return new[] { width, height, (int)previous[center], previous[center + 1], previous[center + 2] };
        }
    }
}
'@
}

function Invoke-ControlCommand {
    param(
        [Parameter(Mandatory)][string]$Cli,
        [Parameter(Mandatory)][System.Collections.IDictionary]$Environment,
        [Parameter(Mandatory)][string[]]$Arguments,
        [int]$Expected = 0,
        [int]$TimeoutSeconds = 30
    )
    $result = Invoke-MacCommand -File $Cli -Arguments (@('--no-launch') + $Arguments) `
        -Environment $Environment -TimeoutSeconds $TimeoutSeconds -AllowFailure
    try { $response = ConvertFrom-Json -InputObject $result.Stdout -AsHashtable -Depth 100 }
    catch { throw "CLI returned invalid JSON: $($result.Stdout); $($result.Stderr)" }
    if ($response -isnot [System.Collections.IDictionary] -or -not $response.Contains('ok') -or
        $result.ExitCode -ne $Expected -or $response.ok -ne ($Expected -eq 0)) {
        throw "CLI $($Arguments -join ' ') returned $($result.ExitCode): $($result.Stdout); $($result.Stderr)"
    }
    if ($Expected -eq 0) {
        if (-not $response.Contains('result')) { throw 'CLI response omitted its result' }
        return $response.result
    }
    if (-not $response.Contains('error')) { throw 'CLI response omitted its error' }
    return $response.error
}

function Wait-ControlServer([string]$Cli, [System.Collections.IDictionary]$Environment, $Fixture) {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    while ($clock.Elapsed.TotalSeconds -lt 30) {
        if ($Fixture.Process.get_HasExited()) {
            throw "Jiffshot exited before its control server started: $($Fixture.Process.ExitCode)"
        }
        $result = Invoke-MacCommand -File $Cli -Arguments @('--no-launch', 'status') `
            -Environment $Environment -TimeoutSeconds 5 -AllowFailure
        if ($result.ExitCode -eq 0) {
            try { $response = ConvertFrom-Json -InputObject $result.Stdout -AsHashtable -Depth 100 }
            catch { throw "Control status returned invalid JSON: $($result.Stdout); $($result.Stderr)" }
            if (-not $response.ok -or -not $response.Contains('result')) {
                throw "Control status omitted a successful result: $($result.Stdout)"
            }
            return $response.result
        }
        Start-Sleep -Milliseconds 100
    }
    throw 'the control server did not start within 30 seconds'
}

function Get-FixturePngCenter([string]$Path) {
    $values = [JiffshotFixture.Png]::Center($Path)
    return [pscustomobject]@{
        Width = $values[0]
        Height = $values[1]
        Pixel = [int[]]@($values[2], $values[3], $values[4])
    }
}

function Test-FixtureEqual($First, $Second) {
    $firstJson = ConvertTo-Json -InputObject $First -Depth 100 -Compress
    $secondJson = ConvertTo-Json -InputObject $Second -Depth 100 -Compress
    return [System.Text.Json.Nodes.JsonNode]::DeepEquals(
        [System.Text.Json.Nodes.JsonNode]::Parse($firstJson),
        [System.Text.Json.Nodes.JsonNode]::Parse($secondJson)
    )
}

function Assert-FixtureCondition([bool]$Condition, [string]$Message, $Details = $null) {
    if (-not $Condition) {
        if ($null -ne $Details) { $Message += ': ' + (ConvertTo-Json -InputObject $Details -Depth 100 -Compress) }
        throw $Message
    }
}

function Test-FixtureChildPath([string]$Path, [string]$Root) {
    $resolved = [IO.Path]::GetFullPath($Path)
    $parent = [IO.Path]::GetFullPath($Root).TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    return $resolved.StartsWith($parent, [StringComparison]::Ordinal)
}

Export-ModuleMember -Function Invoke-ControlCommand, Wait-ControlServer, Get-FixturePngCenter, `
    Test-FixtureEqual, Assert-FixtureCondition, Test-FixtureChildPath
