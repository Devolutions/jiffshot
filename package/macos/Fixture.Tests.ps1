#Requires -Version 7.4

BeforeAll {
    Import-Module (Join-Path $PSScriptRoot 'JiffshotMacFixture.psm1') -Force
}

Describe 'macOS fixture utilities' {
    It 'creates isolated fixture directories and environment' {
        $root = New-FixtureRoot (Join-Path $TestDrive 'fixture with spaces') 'unused-'
        Initialize-FixtureDirectories $root
        $env:JIFFSHOT_UITEST_OLD = 'must not leak'
        try {
            $environment = New-FixtureEnvironment $root '/fixture/host.dylib' 'JIFFSHOT_UITEST_CONTROL'
            $environment['JIFFSHOT_UITEST'] | Should -BeExactly $root
            $environment['JIFFSHOT_LIBRARY'] | Should -BeExactly (Join-Path $root 'library')
            $environment['APPDATA'] | Should -BeExactly (Join-Path $root 'appdata')
            $environment['LOCALAPPDATA'] | Should -BeExactly (Join-Path $root 'localappdata')
            $environment['HOME'] | Should -BeExactly (Join-Path $root 'home')
            $environment['TMPDIR'] | Should -BeExactly (Join-Path $root 'tmp')
            $environment.ContainsKey('JIFFSHOT_UITEST_OLD') | Should -BeFalse
            $environment['JIFFSHOT_UITEST_CONTROL'] | Should -BeExactly '1'
        }
        finally {
            Remove-Item Env:JIFFSHOT_UITEST_OLD -ErrorAction Ignore
            Remove-FixtureRoot $root
        }
    }

    It 'excludes inherited Jiffshot and loader overrides from fixture processes' {
        $inherited = @{
            JIFFSHOT_TRACE = (Join-Path $TestDrive 'user-trace.log')
            JIFFSHOT_GUIDE_OUT = (Join-Path $TestDrive 'user-guide.png')
            JIFFSHOT_CAPTURE_DRY_RUN = '1'
            JIFFSHOT_UITEST_OLD = '1'
            RUSTOLONIA_HOST_DIR = '/wrong/host'
            RUSTOLONIA_HOST_LIB = '/wrong/host.dylib'
            RUSTOLONIA_HOST_BASE_URL = 'https://invalid.example'
            DYLD_LIBRARY_PATH = '/wrong/libraries'
            DYLD_INSERT_LIBRARIES = '/wrong/injected.dylib'
            FIXTURE_UNRELATED_VALUE = 'preserved'
        }
        $previous = @{}
        try {
            foreach ($key in $inherited.Keys) {
                $previous[$key] = [Environment]::GetEnvironmentVariable($key)
                [Environment]::SetEnvironmentVariable($key, $inherited[$key])
            }
            $environment = New-FixtureEnvironment $TestDrive '/fixture/host.dylib' 'JIFFSHOT_UITEST_CONTROL'
            foreach ($key in @(
                'JIFFSHOT_TRACE', 'JIFFSHOT_GUIDE_OUT', 'JIFFSHOT_CAPTURE_DRY_RUN', 'JIFFSHOT_UITEST_OLD',
                'RUSTOLONIA_HOST_DIR', 'RUSTOLONIA_HOST_BASE_URL', 'DYLD_LIBRARY_PATH', 'DYLD_INSERT_LIBRARIES'
            )) {
                $environment.ContainsKey($key) | Should -BeFalse -Because "$key must not affect a fixture"
            }
            $environment['RUSTOLONIA_HOST_LIB'] | Should -BeExactly '/fixture/host.dylib'
            $environment['JIFFSHOT_UITEST_CONTROL'] | Should -BeExactly '1'
            $environment['FIXTURE_UNRELATED_VALUE'] | Should -BeExactly 'preserved'

            $process = Start-FixtureProcess -File '/usr/bin/env' -Environment $environment
            try { $result = Wait-FixtureProcess $process 10 }
            finally { Stop-FixtureProcess $process }
            $result.ExitCode | Should -Be 0
            $result.Stdout | Should -Match '(?m)^RUSTOLONIA_HOST_LIB=/fixture/host\.dylib$'
            $result.Stdout | Should -Not -Match '(?m)^(JIFFSHOT_TRACE|DYLD_INSERT_LIBRARIES)='
        }
        finally {
            foreach ($key in $previous.Keys) {
                [Environment]::SetEnvironmentVariable($key, $previous[$key])
            }
        }
    }

    It 'preserves stdout and stderr in the live fixture log' {
        $log = Join-Path $TestDrive 'process.log'
        $process = Start-FixtureProcess -File '/bin/sh' -Arguments @('-c', 'printf "stdout marker\n"; printf "stderr marker\n" >&2') -LogPath $log
        try {
            $result = Wait-FixtureProcess $process 10
            $result.ExitCode | Should -Be 0
        }
        finally { Stop-FixtureProcess $process }
        $text = [IO.File]::ReadAllText($log)
        $text | Should -Match 'stdout marker'
        $text | Should -Match 'stderr marker'
    }

    It 'captures exact arguments and exit status without a log' {
        $process = Start-FixtureProcess -File '/bin/sh' -Arguments @('-c', 'printf "%s" "$1"; exit 7', 'fixture', 'two words')
        try {
            $result = Wait-FixtureProcess $process 10
            $result.ExitCode | Should -Be 7
            $result.Stdout | Should -BeExactly 'two words'
        }
        finally { Stop-FixtureProcess $process }
    }

    It 'allows repeated cleanup of the same fixture process' {
        $process = Start-FixtureProcess -File '/bin/sh' -Arguments @('-c', 'exit 0')
        [void](Wait-FixtureProcess $process 10)
        Stop-FixtureProcess $process
        $process.Stopped | Should -BeTrue
        { Stop-FixtureProcess $process } | Should -Not -Throw
    }

    It 'terminates and reaps a running fixture process before marking it stopped' {
        $process = Start-FixtureProcess -File '/bin/sleep' -Arguments @('30')
        $observer = [Diagnostics.Process]::GetProcessById($process.Process.Id)
        try {
            Stop-FixtureProcess $process
            $observer.WaitForExit(1000) | Should -BeTrue
            $process.Completed | Should -BeTrue
            $process.Stopped | Should -BeTrue
        }
        finally {
            Stop-FixtureProcess $process
            $observer.Dispose()
        }
    }

    It 'reports an unavailable exit state instead of assuming the process is running' {
        $process = [pscustomobject]@{}
        $process | Add-Member ScriptMethod get_HasExited { throw 'exit state unavailable' }
        $fixture = [pscustomobject]@{ Process = $process; Completed = $false; Stopped = $false }
        { Stop-FixtureProcess $fixture } | Should -Throw '*exit state unavailable*'
        $fixture.Stopped | Should -BeFalse
    }

    It 'rejects an already existing fixture root' {
        $path = Join-Path $TestDrive 'existing'
        [void][IO.Directory]::CreateDirectory($path)
        { New-FixtureRoot $path 'unused-' } | Should -Throw
    }
}
