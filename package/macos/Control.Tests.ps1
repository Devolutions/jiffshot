#Requires -Version 7.4

Import-Module (Join-Path $PSScriptRoot 'JiffshotMacControl.psm1') -Force

BeforeAll {
    function Write-TestPng([string]$Path, [byte[]]$Rows, [int]$Width, [int]$Height, [byte]$Color) {
        function Write-Chunk([IO.Stream]$Stream, [string]$Name, [byte[]]$Payload) {
            $length = [BitConverter]::GetBytes([uint32]$Payload.Length)
            if ([BitConverter]::IsLittleEndian) { [Array]::Reverse($length) }
            $Stream.Write($length)
            $Stream.Write([Text.Encoding]::ASCII.GetBytes($Name))
            $Stream.Write($Payload)
            $Stream.Write([byte[]]@(0, 0, 0, 0))
        }
        $compressed = [IO.MemoryStream]::new()
        try {
            $zlib = [IO.Compression.ZLibStream]::new($compressed, [IO.Compression.CompressionMode]::Compress, $true)
            try { $zlib.Write($Rows) } finally { $zlib.Dispose() }
            $output = [IO.MemoryStream]::new()
            try {
                $output.Write([byte[]]@(137, 80, 78, 71, 13, 10, 26, 10))
                $widthBytes = [BitConverter]::GetBytes([uint32]$Width)
                $heightBytes = [BitConverter]::GetBytes([uint32]$Height)
                if ([BitConverter]::IsLittleEndian) { [Array]::Reverse($widthBytes); [Array]::Reverse($heightBytes) }
                Write-Chunk $output 'IHDR' ([byte[]]($widthBytes + $heightBytes + @(8, $Color, 0, 0, 0)))
                Write-Chunk $output 'IDAT' $compressed.ToArray()
                Write-Chunk $output 'IEND' ([byte[]]@())
                [IO.File]::WriteAllBytes($Path, $output.ToArray())
            }
            finally { $output.Dispose() }
        }
        finally { $compressed.Dispose() }
    }
}

Describe 'macOS control fixture helpers' {
    It 'decodes the center pixel with PNG filter <Filter>' -TestCases @(
        @{ Filter = 0; Rows = [byte[]]@(0,10,20,30,40,50,60, 0,11,22,33,210,220,230) }
        @{ Filter = 1; Rows = [byte[]]@(1,10,20,30,30,30,30, 1,11,22,33,199,198,197) }
        @{ Filter = 2; Rows = [byte[]]@(2,10,20,30,40,50,60, 2,1,2,3,170,170,170) }
        @{ Filter = 3; Rows = [byte[]]@(3,10,20,30,35,40,45, 3,6,12,18,185,184,184) }
        @{ Filter = 4; Rows = [byte[]]@(4,10,20,30,30,30,30, 4,1,2,3,170,170,170) }
    ) {
        param($Filter, $Rows)
        $path = Join-Path $TestDrive "filter-$Filter.png"
        Write-TestPng $path $Rows 2 2 2
        $center = Get-FixturePngCenter $path
        $center.Width | Should -Be 2
        $center.Height | Should -Be 2
        $center.Pixel | Should -Be @(210, 220, 230)
    }

    It 'decodes RGBA and rejects unsupported images' {
        $path = Join-Path $TestDrive 'rgba.png'
        Write-TestPng $path ([byte[]]@(0, 9, 10, 11, 200)) 1 1 6
        (Get-FixturePngCenter $path).Pixel | Should -Be @(9, 10, 11)
        [IO.File]::WriteAllBytes($path, [byte[]]@(1, 2, 3))
        { Get-FixturePngCenter $path } | Should -Throw '*not a PNG*'
        Write-TestPng $path ([byte[]]@(5, 9, 10, 11)) 1 1 2
        { Get-FixturePngCenter $path } | Should -Throw '*unsupported PNG filter*'
    }

    It 'compares JSON structures without depending on property order' {
        Test-FixtureEqual @{ x = 1; y = @(2, 3) } @{ y = @(2, 3); x = 1 } | Should -BeTrue
        Test-FixtureEqual @{ x = @(1, 2) } @{ x = @(2, 1) } | Should -BeFalse
        Test-FixtureEqual @{ x = 1 } @{ x = '1' } | Should -BeFalse
    }

    It 'checks fixture path boundaries instead of string prefixes' {
        Test-FixtureChildPath (Join-Path $TestDrive 'library/image.png') (Join-Path $TestDrive 'library') | Should -BeTrue
        Test-FixtureChildPath (Join-Path $TestDrive 'library-other/image.png') (Join-Path $TestDrive 'library') | Should -BeFalse
        Test-FixtureChildPath (Join-Path $TestDrive 'library/../outside.png') (Join-Path $TestDrive 'library') | Should -BeFalse
    }

    InModuleScope JiffshotMacControl {
        It 'reports an unavailable server exit state before issuing a status request' {
            $process = [pscustomobject]@{}
            $process | Add-Member ScriptMethod get_HasExited { throw 'exit state unavailable' }
            $fixture = [pscustomobject]@{ Process = $process }
            Mock Invoke-MacCommand {}
            { Wait-ControlServer '/fixture/cli' @{} $fixture } | Should -Throw '*exit state unavailable*'
            Should -Invoke Invoke-MacCommand -Times 0 -Exactly
        }

        It 'checks the CLI response and preserves arguments and errors' {
            Mock Invoke-MacCommand {
                [pscustomobject]@{ ExitCode = 0; Stdout = '{"ok":true,"result":{"value":3}}'; Stderr = '' }
            }
            $environment = @{ fixture = 'isolated' }
            $result = Invoke-ControlCommand '/fixture/cli' $environment @('sample', 'two words')
            $result.value | Should -Be 3
            Should -Invoke Invoke-MacCommand -Times 1 -Exactly -ParameterFilter {
                $File -ceq '/fixture/cli' -and $Arguments.Count -eq 3 -and
                $Arguments[0] -ceq '--no-launch' -and $Arguments[2] -ceq 'two words' -and
                $Environment['fixture'] -ceq 'isolated' -and $TimeoutSeconds -eq 30
            }
            Mock Invoke-MacCommand {
                [pscustomobject]@{ ExitCode = 2; Stdout = '{"ok":false,"error":{"code":"invalid_request"}}'; Stderr = '' }
            }
            (Invoke-ControlCommand '/fixture/cli' $environment @('bad') -Expected 2).code | Should -BeExactly 'invalid_request'
            { Invoke-ControlCommand '/fixture/cli' $environment @('bad') } | Should -Throw '*returned 2*'
            Mock Invoke-MacCommand {
                [pscustomobject]@{ ExitCode = 0; Stdout = 'not JSON'; Stderr = 'fixture diagnostic' }
            }
            { Invoke-ControlCommand '/fixture/cli' $environment @('bad') } | Should -Throw '*invalid JSON*fixture diagnostic*'
        }
    }
}
