#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify webcam preferences in a private Xvfb with a mock portal.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force

function Get-FileInode {
    param([Parameter(Mandatory)][string]$Path)
    $inode = & stat -c '%i' -- $Path
    if ($LASTEXITCODE -ne 0) {
        throw "stat failed for $Path"
    }
    [long]$inode
}

function Save-WebcamPreferences {
    param([JiffshotE2E.X11Driver]$Driver, [string]$Settings, [string]$Description)
    $previous = Get-FileInode $Settings
    $Driver.Click(100, 183)
    Wait-E2EUntil { (Get-FileInode $Settings) -ne $previous } $Description
    , (Read-E2EJson $Settings)
}

function Get-StatusPixels {
    param([JiffshotE2E.X11Driver]$Driver)
    $Driver.Capture(0, 480, 800, 20)
}

function Format-E2EJson {
    param([AllowNull()]$Value)
    ConvertTo-Json -InputObject $Value -Depth 16 -Compress
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$portal = New-MockPortal
$app = $server = $driver = $null
$work = New-E2EWorkDirectory 'jiffshot-webcam-ui-'
try {
    New-E2EPrivateDirectory (Join-Path $work 'runtime') | Out-Null
    $config = New-Item -ItemType Directory -Force -Path (Join-Path $work 'config/Jiffshot')
    $settings = Join-Path $config 'settings.json'
    $original = [ordered]@{
        theme               = 'Light'
        capture_directory   = Join-Path $work 'captures'
        webcam_device       = '/dev/video7'
        webcam_corner       = 'top-left'
        webcam_size_percent = 40
    }
    Write-E2EJson $settings $original
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $displayName = $server.Display
        $environment = New-E2EEnvironment $displayName $work $runtime
        $app = Start-E2EProcess @($binaryPath) $environment $work $appLog
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'the editor' 12
        $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
        Wait-E2ESeconds 0.5
        Invoke-E2EScroll $driver down
        Wait-E2ESeconds 0.5
        Write-E2ETrace $driver 'initial'

        $initial = Save-WebcamPreferences $driver $settings 'saving legacy preferences'
        Assert-E2E (Test-E2EDeepEqual $initial (Copy-E2EMap $original @{ webcam_shape = 'rectangle' })) `
            "legacy settings must save as a rectangle without losing other preferences: $(Format-E2EJson $initial)"

        Set-E2EText $driver 185 150 '/dev/video9'
        Set-E2EText $driver 600 150 '50'
        $choices = @(
            @('rectangle', 'top-left'),
            @('rectangle', 'top-right'),
            @('rectangle', 'bottom-left'),
            @('rectangle', 'bottom-right'),
            @('circle', 'top-left'),
            @('circle', 'top-right'),
            @('circle', 'bottom-left'),
            @('circle', 'bottom-right')
        )
        $saved = $null
        foreach ($index in @(0, 1, 2, 3, 4, 5, 7, 3, 6)) {
            $shape, $corner = $choices[$index]
            $driver.Click(420, 150)
            $driver.Press('Home')
            for ($step = 0; $step -lt $index; $step++) {
                $driver.Press('Down')
            }
            $driver.Press('Return')
            $saved = Save-WebcamPreferences $driver $settings "saving $shape at $corner"
            $expected = Copy-E2EMap $original @{
                webcam_device       = '/dev/video9'
                webcam_corner       = $corner
                webcam_size_percent = 50
                webcam_shape        = $shape
            }
            Assert-E2E (Test-E2EDeepEqual $saved $expected) "($index, $(Format-E2EJson $saved))"
        }
        Write-E2ETrace $driver 'edited'

        Close-E2EX11Driver $driver
        $driver = $null
        Stop-E2EProcess $app
        $app = Start-E2EProcess @($binaryPath) $environment $work $appLog
        Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'the reopened editor' 12
        $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
        Wait-E2ESeconds 0.5
        Invoke-E2EScroll $driver down
        Wait-E2ESeconds 0.5
        $reloaded = Save-WebcamPreferences $driver $settings 'saving the reloaded circle selection'
        Assert-E2E (Test-E2EDeepEqual $reloaded $saved) `
            "reopening the editor must restore the circle, corner, device, and size: $(Format-E2EJson $reloaded)"
        Write-E2ETrace $driver 'reloaded'
        Wait-E2ESeconds 0.3
        $beforeStatus = Get-StatusPixels $driver

        Set-E2EText $driver 600 150 '99'
        $driver.Click(100, 183)
        Wait-E2EUntil { -not (Get-StatusPixels $driver).SameAs($beforeStatus) } 'the invalid-size error'
        Assert-E2E (Test-E2EDeepEqual (Read-E2EJson $settings) $saved) 'an invalid webcam size must not overwrite saved preferences'
        Write-E2ETrace $driver 'invalid'

        Write-E2EJson $settings (Copy-E2EMap $saved @{ webcam_size_percent = 99 })
        Invoke-E2EScroll $driver up
        Wait-E2ESeconds 0.5
        Write-E2ETrace $driver 'top'
        $driver.Click(420, 45)
        $driver.Click(300, 45)
        Wait-E2ESeconds 0.8
        Assert-E2E ($portal.CaptureRequests -eq 0) 'invalid webcam preferences must prevent a recording portal request'
        Write-E2ETrace $driver 'blocked'

        foreach ($invalidShape in @('oval', $null)) {
            $shapeText = if ($null -eq $invalidShape) { 'None' } else { "'$invalidShape'" }
            $invalid = Copy-E2EMap $saved @{ webcam_shape = $invalidShape }
            Write-E2EJson $settings $invalid
            $beforeStatus = Get-StatusPixels $driver
            $driver.Click(300, 45)
            Wait-E2EUntil { -not (Get-StatusPixels $driver).SameAs($beforeStatus) } "the invalid-shape error for $shapeText"
            Assert-E2E ($portal.CaptureRequests -eq 0) "invalid webcam shape $shapeText must gate recording"
            Assert-E2E (Test-E2EDeepEqual (Read-E2EJson $settings) $invalid) 'invalid webcam settings must not be rewritten'
        }

        Write-E2EJson $settings $saved
        $driver.Click(300, 45)
        Wait-E2EUntil { $portal.CaptureRequests -eq 1 } 'the recording portal request after restoring valid preferences'
        Write-E2ELine 'GUI saved all webcam positions, reloaded a circle, rejected invalid values, and gated recording'
    }
    catch {
        if ($null -ne $driver) {
            Write-E2ETrace $driver 'failure'
        }
        Write-E2ELogTail @($xvfbLog, $appLog)
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
