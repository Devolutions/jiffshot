#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify that opening a slow file does not block or overwrite the editor.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force

function Invoke-DrawArrow {
    param([JiffshotE2E.X11Driver]$Driver)
    $Driver.Move(250, 350)
    $Driver.Button(1, $true)
    $Driver.Move(520, 430)
    $Driver.Button(1, $false)
    $Driver.Sync()
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$portal = New-MockPortal
$app = $server = $driver = $null
$work = New-E2EWorkDirectory 'jiffshot-load-ui-'
try {
    New-E2EPrivateDirectory (Join-Path $work 'runtime') | Out-Null
    $config = New-Item -ItemType Directory -Force -Path (Join-Path $work 'config/Jiffshot')
    Write-E2EJson (Join-Path $config 'settings.json') @{ theme = 'Light' }
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $displayName = $server.Display
        $environment = New-E2EEnvironment $displayName $work $runtime
        foreach ($edited in @($false, $true)) {
            $source = Join-Path $work ($edited ? 'edited.png' : 'unchanged.png')
            [JiffshotE2E.Posix]::MakeFifo($source)
            $app = Start-E2EProcess @($binaryPath, $source) $environment $work $appLog
            Wait-E2EUntil { $null -ne (Get-E2EWindowInfo $environment) } 'the editor while the file is blocked' 8
            $driver = New-E2EX11Driver $displayName (Get-E2EWindowInfo $environment)
            Wait-E2ESeconds 0.5
            if ($edited) {
                Invoke-DrawArrow $driver
                Wait-E2EUntil { $driver.RedCanvasPixels() -gt 40 } 'an annotation while the file is blocked'
                $before = $driver.RedCanvasPixels()
                $payload = Get-E2ESolidPng 255 255 255
            }
            else {
                $payload = Get-E2ESolidPng 255 0 0
            }
            Wait-E2EUntil { [JiffshotE2E.Posix]::SendFifo($source, $payload) } 'the blocked file reader'
            if ($edited) {
                Wait-E2ESeconds 2
                $after = $driver.RedCanvasPixels()
                Assert-E2E ($after -ge $before * 0.8) "the completed open replaced a newer annotation: before=$before, after=$after"
            }
            else {
                Wait-E2EUntil { $driver.RedCanvasPixels() -gt 5000 } 'the loaded image to reach the unchanged editor'
            }
            Close-E2EX11Driver $driver
            $driver = $null
            Stop-E2EProcess $app
            $app = $null
        }
        Write-E2ELine 'GUI stayed responsive while opening; late results preserved newer edits'
    }
    catch {
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
