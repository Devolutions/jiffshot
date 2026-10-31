#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify Linux interval capture controls with a mock or real desktop portal.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir,
    [switch]$RealPortal
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force

$PngEndMarker = [byte[]](0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82)

function Get-FileInode {
    param([Parameter(Mandatory)][string]$Path)
    $inode = & stat -c '%i' -- $Path
    if ($LASTEXITCODE -ne 0) {
        throw "stat failed for $Path"
    }
    [long]$inode
}

function Format-E2EJson {
    param([AllowNull()]$Value)
    ConvertTo-Json -InputObject $Value -Depth 16 -Compress
}

function Get-StatusPixels {
    param([JiffshotE2E.X11Driver]$Driver)
    $Driver.Capture(0, 480, 800, 20)
}

function Get-SavedImages {
    <# Completed Screenshot-*.png files in Directory, sorted by path. #>
    param([Parameter(Mandatory)][string]$Directory)
    $images = [System.Collections.Generic.List[string]]::new()
    foreach ($path in [System.IO.Directory]::GetFiles($Directory, 'Screenshot-*.png')) {
        $bytes = [System.IO.File]::ReadAllBytes($path)
        if ($bytes.Length -ge $PngEndMarker.Length -and
            [System.Linq.Enumerable]::SequenceEqual([byte[]]$bytes[($bytes.Length - $PngEndMarker.Length)..($bytes.Length - 1)], $PngEndMarker)) {
            $images.Add($path)
        }
    }
    $images.Sort([System.StringComparer]::Ordinal)
    , $images.ToArray()
}

function Assert-SavedImage {
    <# Checks a 1280x720 PNG whose center pixel is within 8 of the expected RGB color. #>
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][int[]]$Expected)
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    $signature = [byte[]](0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A)
    Assert-E2E ($bytes.Length -ge 24 -and [System.Linq.Enumerable]::SequenceEqual([byte[]]$bytes[0..7], $signature)) "$Path is not a PNG"
    $width = [System.Buffers.Binary.BinaryPrimitives]::ReadUInt32BigEndian([System.ReadOnlySpan[byte]]::new($bytes, 16, 4))
    $height = [System.Buffers.Binary.BinaryPrimitives]::ReadUInt32BigEndian([System.ReadOnlySpan[byte]]::new($bytes, 20, 4))
    Assert-E2E ($width -eq 1280 -and $height -eq 720) "($width, $height)"
    $image = [JiffshotE2E.Png]::Decode($bytes)
    Assert-E2E ($image.Rgba.Length -eq $width * $height * 4) "$Path decoded to $($image.Rgba.Length) bytes"
    $color = $image.Pixel([int][Math]::Floor($width / 2), [int][Math]::Floor($height / 2))
    $close = $true
    for ($channel = 0; $channel -lt 3; $channel++) {
        if ([Math]::Abs($color[$channel] - $Expected[$channel]) -gt 8) {
            $close = $false
        }
    }
    Assert-E2E $close "($Path, [$($color[0..2] -join ', ')], ($($Expected -join ', ')))"
}

function Test-IdenticalCopy {
    param([string]$Source, [string]$Copied)
    if (-not [System.IO.File]::Exists($Copied)) {
        return $false
    }
    [System.Linq.Enumerable]::SequenceEqual([System.IO.File]::ReadAllBytes($Copied), [System.IO.File]::ReadAllBytes($Source))
}

function Wait-IdenticalCopy {
    param([Parameter(Mandatory)][string]$Source, [Parameter(Mandatory)][string]$Destination)
    $copied = Join-Path $Destination ([System.IO.Path]::GetFileName($Source))
    Wait-E2EUntil { Test-IdenticalCopy $Source $copied } "the byte-identical copy of $([System.IO.Path]::GetFileName($Source))" 15
    Assert-E2E (Test-IdenticalCopy $Source $copied) "$copied must be byte-identical to $Source"
}

function Set-SettingsAtomically {
    param([Parameter(Mandatory)][string]$Settings, [Parameter(Mandatory)]$Values)
    $temporary = Join-Path ([System.IO.Path]::GetDirectoryName($Settings)) 'settings-e2e-replacement.json'
    Write-E2EJson $temporary $Values
    [System.IO.File]::Move($temporary, $Settings, $true)
}

function Wait-StableSidebar {
    <# Waits until two sidebar captures 150 ms apart are identical, so labels are not read mid-scroll or mid-render. #>
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver)
    $previous = [ref]($Driver.Capture(0, 0, 800, 270))
    Wait-E2EUntil {
        Wait-E2ESeconds 0.15
        $current = $Driver.Capture(0, 0, 800, 270)
        $stable = $current.SameAs($previous.Value)
        $previous.Value = $current
        $stable
    } 'the sidebar to finish redrawing' 10
}

function Invoke-CancelRecordingClick {
    <#
    Clicks "Cancel recording" only at a position confirmed on a settled sidebar, then waits for the
    app to acknowledge the command in its status line. The click is repeated only when neither the
    status nor the portal shows that the previous click reached the button.
    #>
    param(
        [Parameter(Mandatory)][JiffshotE2E.TextReader]$Reader,
        [Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver,
        [Parameter(Mandatory)]$Portal,
        [Parameter(Mandatory)][string]$Session
    )
    $acknowledged = {
        $Portal.Closes.Contains($Session) -or
        (Get-E2EStatusText $Reader $Driver).ToLowerInvariant().Contains('cancel')
    }
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        Wait-StableSidebar $Driver
        $position = Find-E2EScrollingLabel $Reader $Driver 'Cancel recording'
        Wait-StableSidebar $Driver
        $confirmed = Find-E2ELabel $Reader $Driver 'Cancel recording'
        if ($null -eq $confirmed -or $confirmed[0] -ne $position[0] -or $confirmed[1] -ne $position[1]) {
            continue
        }
        $Driver.Click($confirmed[0], $confirmed[1])
        try {
            Wait-E2EUntil $acknowledged 'the app to acknowledge the cancel click' 4
            return
        }
        catch [System.TimeoutException] {
            Write-E2ETrace $Driver "interval-cancel-unacknowledged-$attempt"
            Write-Host "Cancel click $attempt was not acknowledged; status: '$(Get-E2EStatusText $Reader $Driver)'"
        }
    }
}

function Test-MockRecordingCancelClosesPortalSession {
    param([JiffshotE2E.X11Driver]$Driver, $Portal)
    $session = $Portal.ScreenStarts[$Portal.ScreenStarts.Count - 1][0]
    $reader = New-E2ETextReader
    try {
        Invoke-CancelRecordingClick $reader $Driver $Portal $session
        Wait-E2EUntil { $Portal.Closes.Contains($session) } 'closing the cancelled recording'
        Assert-E2E ($Portal.CaptureRequests -eq 3) 'cancellation must not request another source'
    }
    finally {
        $reader.Dispose()
    }
    Write-E2EPass 'test_mock_recording_cancel_closes_portal_session'
}

function Invoke-MockChecks {
    param([JiffshotE2E.X11Driver]$Driver, $Portal, [string]$Settings, $Original)
    Invoke-E2EScroll $Driver down
    Wait-E2ESeconds 0.5
    $Driver.Click(680, 115)
    Wait-E2ESeconds 0.2
    Invoke-E2EScroll $Driver down
    Wait-E2ESeconds 0.5
    Write-E2ETrace $Driver 'interval-settings'

    $beforeStatus = Get-StatusPixels $Driver
    Set-E2EText $Driver 220 182 '0'
    $Driver.Click(550, 182)
    Wait-E2EUntil { -not (Get-StatusPixels $Driver).SameAs($beforeStatus) } 'invalid interval error'
    Assert-E2E ($Portal.CaptureRequests -eq 0) 'zero seconds must not request the portal'
    Assert-E2E (Test-E2EDeepEqual (Read-E2EJson $Settings) $Original) 'invalid input must not alter settings'

    Set-E2EText $Driver 220 182 '2'
    $Driver.Click(270, 182)
    $previous = Get-FileInode $Settings
    $Driver.Click(550, 182)
    Wait-E2EUntil { (Get-FileInode $Settings) -ne $previous } 'saving interval preferences'
    $expected = Copy-E2EMap $Original @{ interval_seconds = 2; interval_skip_identical = $false }
    $saved = Read-E2EJson $Settings
    Assert-E2E (Test-E2EDeepEqual $saved $expected) (Format-E2EJson $saved)
    Wait-E2EUntil { $Portal.ScreenStarts.Count -eq 1 } 'interval source chooser'
    $options = $Portal.ScreenSelectionOptions[$Portal.ScreenSelectionOptions.Count - 1]
    Assert-E2E ($options['cursor_mode'].Value -eq 1) 'interval capture must request the saved hidden-cursor setting'

    Invoke-E2EScroll $Driver up
    Wait-E2ESeconds 0.4
    $Driver.Click(300, 45)
    $Driver.Click(300, 15)
    Wait-E2ESeconds 0.4
    Assert-E2E ($Portal.CaptureRequests -eq 1) 'recording and scrolling capture must not start during interval capture'
    Invoke-E2EScroll $Driver down
    Wait-E2ESeconds 0.4
    $Driver.Click(550, 182)
    $session, $request = $Portal.ScreenStarts[$Portal.ScreenStarts.Count - 1]
    Send-PortalResponse $Portal $request 1 @{}
    Wait-E2EUntil { $Portal.Closes.Contains($session) } 'closing the stopped portal session'
    Wait-E2ESeconds 0.4
    Assert-E2E (@(Get-ChildItem -LiteralPath $expected['capture_directory'] -Force).Count -eq 0) 'stopping interval capture must not save an image'

    $Driver.Click(550, 182)
    Wait-E2EUntil { $Portal.ScreenStarts.Count -eq 2 } 'restarting interval capture'
    Assert-E2E (Test-E2EDeepEqual (Read-E2EJson $Settings) $expected) 'restarting must retain saved preferences'
    $Driver.Click(550, 182)
    $session, $request = $Portal.ScreenStarts[$Portal.ScreenStarts.Count - 1]
    Send-PortalResponse $Portal $request 1 @{}
    Wait-E2EUntil { $Portal.Closes.Contains($session) } 'closing the restarted portal session'
    Wait-E2ESeconds 0.3

    Invoke-E2EScroll $Driver up
    Wait-E2ESeconds 0.4
    $Driver.Click(300, 45)
    Wait-E2EUntil { $Portal.ScreenStarts.Count -eq 3 } 'recording source chooser'
    Invoke-E2EScroll $Driver down
    Wait-E2ESeconds 0.4
    $Driver.Click(550, 182)
    Wait-E2ESeconds 0.3
    Assert-E2E ($Portal.CaptureRequests -eq 3) 'an active recording must block interval capture'
    Invoke-E2EScroll $Driver up
    Wait-E2ESeconds 0.4
    Test-MockRecordingCancelClosesPortalSession $Driver $Portal
    Invoke-E2EScroll $Driver up
    Wait-E2ESeconds 0.3

    $Driver.Click(300, 15)
    Wait-E2EUntil { $Portal.ScreenStarts.Count -eq 4 } 'scrolling source chooser'
    Invoke-E2EScroll $Driver down
    Wait-E2ESeconds 0.4
    $Driver.Click(550, 182)
    Wait-E2ESeconds 0.3
    Assert-E2E ($Portal.CaptureRequests -eq 4) 'active scrolling must block interval capture'
    Invoke-E2EScroll $Driver up
    Wait-E2ESeconds 0.4
    $Driver.Click(510, 15)
    $session, $request = $Portal.ScreenStarts[$Portal.ScreenStarts.Count - 1]
    Send-PortalResponse $Portal $request 1 @{}
    Wait-E2EUntil { $Portal.Closes.Contains($session) } 'closing the cancelled scrolling session'
    Write-E2ELine 'GUI validated interval settings, cursor mode, mutual exclusion and stop/restart'
}

function Test-RealIntervalFinalResultReportsCopyError {
    param([JiffshotE2E.X11Driver]$Driver, [JiffshotE2E.TextReader]$Reader, [string]$Original, [string]$MissingFolder)
    # Progress says "Automatic copy failed:", whereas the final result
    # includes the number of failed copies before any potentially long path.
    Wait-E2EStatus $Reader $Driver 'Automatic copy failed for' 15
    $finalStatus = Get-E2EStatusText $Reader $Driver
    Assert-E2E ($finalStatus.ToLowerInvariant().Contains('automatic copy failed for')) `
        "the final interval result must visibly retain copy errors: '$finalStatus'"
    Assert-E2E ([System.IO.File]::Exists($Original)) 'copy failures must preserve interval originals'
    Assert-E2E (-not [System.IO.Directory]::Exists($MissingFolder)) "$MissingFolder must not be recreated"
}

function Test-RealRecordingAutoCopyIsByteIdentical {
    param([JiffshotE2E.X11Driver]$Driver, [JiffshotE2E.TextReader]$Reader, [string]$Captures, [string]$Destination)
    Invoke-E2EScroll $Driver up
    Wait-E2ESeconds 0.5
    $previous = @([System.IO.Directory]::GetFiles($Captures, 'Recording-*.mp4'))
    $Driver.Click(300, 45)
    Wait-E2EStatus $Reader $Driver 'Screen recording is active' 25
    # Allow actual PipeWire frames to reach the encoder before stopping.
    Wait-E2ESeconds 1
    $Driver.Click(300, 45)
    Wait-E2EStatus $Reader $Driver 'Screen recording saved' 30
    $added = @([System.IO.Directory]::GetFiles($Captures, 'Recording-*.mp4') | Where-Object { $_ -notin $previous })
    Assert-E2E ($added.Count -eq 1) "expected one completed real recording, got $($added -join ', ')"
    $recording = $added[0]
    Assert-E2E ((Get-Item -LiteralPath $recording).Length -gt 1000) 'the original must contain encoded video'
    Wait-IdenticalCopy $recording $Destination
    $decoder = [JiffshotE2E.ChildProcess]::Start(
        @('gst-launch-1.0', '-q', 'filesrc', "location=$recording", '!', 'qtdemux', '!', 'h264parse', '!', 'avdec_h264', '!', 'fakesink'),
        $null, $null, $null)
    try {
        Wait-E2EUntil { $decoder.HasExited } 'decoding the real recording' 20
    }
    catch [System.TimeoutException] {
        $decoder.Stop()
        throw
    }
    Assert-E2E ($decoder.ExitCode -eq 0) "gst-launch-1.0 could not decode $recording (exit $($decoder.ExitCode))"
    Write-E2EPass 'test_real_recording_auto_copy_is_byte_identical'
}

function Invoke-RealChecks {
    param([JiffshotE2E.X11Driver]$Driver, [string]$Settings, $Original, [string]$Work, [string]$Captures, [string]$FirstFolder, [string]$SecondFolder)
    $reader = New-E2ETextReader
    try {
        Wait-E2EUntil { (Get-SavedImages $Captures).Count -ge 1 } 'first interval screenshot' 20
        $first = (Get-SavedImages $Captures)[0]
        Assert-SavedImage $first @(0x20, 0x40, 0x60)
        Wait-IdenticalCopy $first $FirstFolder
        Wait-E2ESeconds 2.4
        Assert-E2E ((Get-SavedImages $Captures).Count -eq 1) 'identical frames should not be saved again'

        $Original['linux_auto_copy_directory'] = $SecondFolder
        Set-SettingsAtomically $Settings $Original
        $color = $env:JIFFSHOT_DELAY_COLOR_FILE
        if (-not $color) {
            throw 'real portal mode requires JIFFSHOT_DELAY_COLOR_FILE'
        }
        [System.IO.File]::WriteAllText($color, 'a02040')
        Wait-E2EUntil { (Get-SavedImages $Captures).Count -ge 2 } 'changed interval screenshot' 8
        $second = (Get-SavedImages $Captures)[-1]
        Assert-SavedImage $second @(0xA0, 0x20, 0x40)
        Wait-IdenticalCopy $second $SecondFolder
        $firstCopies = @([System.IO.Directory]::GetFiles($FirstFolder, '*.png') | ForEach-Object { [System.IO.Path]::GetFileName($_) })
        Assert-E2E ($firstCopies.Count -eq 1 -and $firstCopies[0] -eq [System.IO.Path]::GetFileName($first)) `
            'a running interval must use the changed destination for the next image'
        Write-E2EPass 'test_real_interval_auto_copy_matches_saved_images'

        $missingFolder = Join-Path $Work 'removed-interval-destination'
        [System.IO.Directory]::CreateDirectory($missingFolder) | Out-Null
        Set-SettingsAtomically $Settings (Copy-E2EMap $Original @{ linux_auto_copy_directory = $missingFolder })
        [System.IO.Directory]::Delete($missingFolder)
        [System.IO.File]::WriteAllText($color, '605020')
        Wait-E2EUntil { (Get-SavedImages $Captures).Count -ge 3 } 'interval image after folder removal' 8
        $failedCopyOriginal = (Get-SavedImages $Captures)[-1]
        Assert-SavedImage $failedCopyOriginal @(0x60, 0x50, 0x20)
        Invoke-E2EScroll $Driver down
        Wait-E2ESeconds 0.5
        Write-E2ETrace $Driver 'interval-real-active'
        $Driver.Click(550, 182)
        Test-RealIntervalFinalResultReportsCopyError $Driver $reader $failedCopyOriginal $missingFolder
        Write-E2EPass 'test_real_interval_final_result_reports_copy_error'
        $stopped = (Get-SavedImages $Captures).Count
        [System.IO.File]::WriteAllText($color, '204060')
        Wait-E2ESeconds 2
        Assert-E2E ((Get-SavedImages $Captures).Count -eq $stopped) 'stopping must end interval capture'

        Set-SettingsAtomically $Settings $Original
        $Driver.Click(270, 182)
        $Driver.Click(550, 182)
        Wait-E2EUntil {
            $value = (Read-E2EJson $Settings)['interval_skip_identical']
            $value -is [bool] -and -not $value
        } 'saving repeat-identical preference'
        Wait-E2EUntil { (Get-SavedImages $Captures).Count -ge $stopped + 2 } 'two repeated screenshots' 10
        $images = Get-SavedImages $Captures
        foreach ($path in $images[$stopped..($images.Count - 1)]) {
            Wait-IdenticalCopy $path $SecondFolder
        }
        Assert-E2E (Test-E2EDeepEqual (Read-E2EJson $Settings) (Copy-E2EMap $Original @{ interval_skip_identical = $false })) `
            'restarting must save only the repeat-identical preference'
        $Driver.Click(550, 182)
        Wait-E2ESeconds 0.5
        $stopped = (Get-SavedImages $Captures).Count
        Wait-E2ESeconds 2
        Assert-E2E ((Get-SavedImages $Captures).Count -eq $stopped) 'restarted capture must also stop'
        Write-E2ELine 'Real portal saved changed and repeated images and stopped both sessions'
        Test-RealRecordingAutoCopyIsByteIdentical $Driver $reader $Captures $SecondFolder
    }
    finally {
        $reader.Dispose()
    }
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$portal = $null
if (-not $RealPortal) {
    $portal = New-MockPortal
    $portal.ScreenMode = 'hold-start'
}
$app = $server = $driver = $null
$work = New-E2EWorkDirectory 'jiffshot-interval-ui-'
try {
    $xdgRuntime = New-E2EPrivateDirectory (Join-Path $work 'runtime')
    $config = New-Item -ItemType Directory -Force -Path (Join-Path $work 'config/Jiffshot')
    $captures = (New-Item -ItemType Directory -Force -Path (Join-Path $work 'captures')).FullName
    $settings = Join-Path $config 'settings.json'
    $original = [ordered]@{
        theme                   = 'Light'
        capture_directory       = $captures
        copy_to_clipboard       = $false
        capture_cursor          = $false
        interval_seconds        = if ($RealPortal) { 1 } else { 5 }
        interval_skip_identical = $true
        other                   = [ordered]@{ keep = $true }
    }
    $firstFolder = Join-Path $work 'interval-copy-first'
    $secondFolder = Join-Path $work 'interval-copy-second'
    if ($RealPortal) {
        [System.IO.Directory]::CreateDirectory($firstFolder) | Out-Null
        [System.IO.Directory]::CreateDirectory($secondFolder) | Out-Null
        $original['linux_auto_copy_directory'] = $firstFolder
    }
    Write-E2EJson $settings $original
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $displayName = $server.Display
        $runtimeDirectory = if ($RealPortal) { $env:XDG_RUNTIME_DIR } else { $xdgRuntime }
        $environment = New-E2EEnvironment $displayName $work $runtime -RuntimeDirectory $runtimeDirectory
        $arguments = @($binaryPath)
        if ($RealPortal) {
            $arguments += '--interval'
        }
        $app = Start-E2EProcess $arguments $environment $work $appLog
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'the editor' 12
        $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
        Wait-E2ESeconds 0.5
        if ($RealPortal) {
            Invoke-RealChecks $driver $settings $original $work $captures $firstFolder $secondFolder
        }
        else {
            Invoke-MockChecks $driver $portal $settings $original
            Close-E2EX11Driver $driver
            $driver = $null
            Stop-E2EProcess $app
            $app = $null
            $invalid = Copy-E2EMap $original @{ interval_seconds = 0 }
            Write-E2EJson $settings $invalid
            $previous = $portal.CaptureRequests
            $app = Start-E2EProcess @($binaryPath, '--interval') $environment $work $appLog
            Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'the interval CLI editor' 12
            Wait-E2ESeconds 0.8
            Assert-E2E ($portal.CaptureRequests -eq $previous) '--interval must reject invalid saved settings before requesting a source'
            Assert-E2E (Test-E2EDeepEqual (Read-E2EJson $settings) $invalid) '--interval must not rewrite invalid saved settings'
            Write-E2ELine 'Interval CLI rejected invalid saved preferences'
        }
    }
    catch {
        if ($null -ne $driver) {
            Write-E2ETrace $driver 'interval-failure'
        }
        Write-E2ELogTail @($appLog, $xvfbLog)
        throw
    }
    finally {
        Close-E2EX11Driver $driver
        Stop-E2EProcess $app
        if ($null -ne $server) { Stop-E2EProcess $server.Process }
    }
}
finally {
    Unregister-E2EService $portal
    Remove-E2EWorkDirectory $work
}
