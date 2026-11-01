#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify outgoing library drags using a dedicated Jiffshot process and synthetic AppKit events.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$HostLibrary,
    [switch]$ExternalDrop,
    [switch]$SimulateReceiverFailure,
    [switch]$SimulatePointerChange,
    [switch]$SimulateSourceOwnerFailure,
    [switch]$SimulateSourceTimeout,
    [switch]$SimulateReceiverTimeout,
    [string]$Output
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacPackage.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacFixture.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacDrag.psm1') -Force -DisableNameChecking

if ($SimulateReceiverFailure -and -not $ExternalDrop) { throw '-SimulateReceiverFailure requires -ExternalDrop' }
if ($SimulatePointerChange -and -not $SimulateReceiverFailure) { throw '-SimulatePointerChange requires -SimulateReceiverFailure' }
if ($SimulateSourceOwnerFailure -and -not $SimulateReceiverFailure) { throw '-SimulateSourceOwnerFailure requires -SimulateReceiverFailure' }
if ($SimulateSourceOwnerFailure -and $SimulatePointerChange) { throw '-SimulateSourceOwnerFailure cannot be combined with -SimulatePointerChange' }
if (($SimulateSourceTimeout -or $SimulateReceiverTimeout) -and
    (-not $ExternalDrop -or $SimulateReceiverFailure -or ($SimulateSourceTimeout -and $SimulateReceiverTimeout))) {
    throw 'Timeout simulations require -ExternalDrop and cannot be combined with other failures'
}

$markers = @(
    'a below-threshold gesture preserved the normal tile click',
    'pending drag vetoed quit and suppressed a return-inside tap',
    'late preparation was discarded after button release',
    'normal clicks recovered after cancellation',
    'AppKit received two Copy-only URLs, including the current unsaved edits; Escape cancelled once',
    'active drag retained its exports while quit and library rebuild were requested',
    'an external process read the active AppKit drag pasteboard',
    'active owner closure waited for one native completion before removing exports',
    'closing the owner cancelled the pending gesture',
    'closing the owner discarded the late snapshots',
    'rebuilding the library cancelled the pending gesture',
    'rebuilt tiles re-registered and stale snapshots were removed'
)
$binaryPath = Resolve-FixtureFile $Binary
$hostPath = Resolve-FixtureFile $HostLibrary
$root = New-FixtureRoot $Output 'jiffshot-library-drag-'
try {
    $probe = if ($ExternalDrop) { New-DragInputProbe } else { $null }
    if ($probe -and ((Test-AnyDragButton $probe.Buttons()) -or $probe.Modifiers())) {
        throw 'release held mouse buttons and modifiers before the drop fixture'
    }
    Initialize-FixtureDirectories $root
    $environment = New-FixtureEnvironment $root $hostPath 'JIFFSHOT_UITEST_LIBRARY_DRAG'
    $receiver = Join-Path $root 'drag-receiver'
    $source = if ($ExternalDrop) { 'DropReceiver.m' } else { 'Receiver.m' }
    try {
        [void](Invoke-MacCommand -File 'xcrun' -Arguments @(
            'clang', '-fobjc-arc', '-framework', 'Cocoa', '-framework', 'ImageIO',
            (Join-Path $PSScriptRoot "fixtures/library-drag/$source"), '-o', $receiver
        ))
    }
    catch { throw "could not build the external drag receiver: $($_.Exception.Message)" }
    $environment['JIFFSHOT_UITEST_DRAG_RECEIVER'] = $receiver
    if ($ExternalDrop) {
        $environment['JIFFSHOT_UITEST_DRAG_DROP'] = '1'
        foreach ($entry in @{
            JIFFSHOT_UITEST_DRAG_POINTER_FILE = 'drag-pointer.json'
            JIFFSHOT_UITEST_DRAG_ORIGINAL_POINTER_FILE = 'drag-original-pointer.json'
            JIFFSHOT_UITEST_DRAG_ACTIVE_FILE = 'drag-active.json'
            JIFFSHOT_UITEST_DRAG_RECEIVER_STARTED_FILE = 'drag-receiver-started'
            JIFFSHOT_UITEST_DRAG_RECEIVER_FINISHED_FILE = 'drag-receiver-finished'
            JIFFSHOT_UITEST_DRAG_RECEIVER_EXITED_FILE = 'drag-receiver-exited'
        }.GetEnumerator()) { $environment[$entry.Key] = Join-Path $root $entry.Value }
    }
    if ($SimulateReceiverFailure) { $environment['JIFFSHOT_UITEST_DRAG_RECEIVER_EXIT_AFTER_WARP'] = '1' }
    if ($SimulatePointerChange) { $environment['JIFFSHOT_UITEST_DRAG_RECEIVER_MOVE_BEFORE_EXIT'] = '1' }
    if ($SimulateSourceOwnerFailure) { $environment['JIFFSHOT_UITEST_DRAG_FORCE_OWNER_FAILURE'] = '1' }
    if ($SimulateReceiverTimeout) { $environment['JIFFSHOT_UITEST_DRAG_RECEIVER_HOLD_AFTER_WARP'] = '1' }

    $interrupt = New-DragInterruptGuard
    try {
        $process = Start-FixtureProcess -File $binaryPath -Environment $environment -LogPath (Join-Path $root 'process.log')
        $result = Invoke-DragFixtureProcess $process $root $probe $receiver $environment $interrupt `
            ([bool]$SimulateSourceTimeout) ([bool]$SimulateReceiverTimeout)
        $timedOut = $result.TimedOut
        $status = $result.ExitCode
        $recovery = $result.Recovery
        $interrupt.ThrowIfInterrupted()
    }
    finally { $interrupt.Dispose() }

    if ($timedOut) {
        if (-not ($SimulateSourceTimeout -or $SimulateReceiverTimeout)) { throw 'library drag fixture did not finish' }
        if ($null -eq $recovery -or -not $recovery.Restored -or (Test-AnyDragButton $probe.Buttons())) {
            throw 'the interrupted fixture did not safely recover its synthetic mouse button and pointer'
        }
        if ($SimulateSourceTimeout -and -not $recovery.Attempted) {
            throw 'the interrupted source did not exercise mouse-button recovery'
        }
        if ($SimulateReceiverTimeout -and -not [IO.File]::Exists((Join-Path $root 'drag-receiver-exited'))) {
            throw 'the interrupted receiver did not stop before input recovery'
        }
        $original = Read-DragState (Join-Path $root 'drag-original-pointer.json') 2
        if ($null -eq $original -or -not (Test-DragSamePoint $probe.Position() $original)) {
            throw 'the interrupted fixture did not restore its original pointer'
        }
        if ($Output) { Write-Host "Interrupted library drag fixture recovered: $root" }
        else { Remove-FixtureRoot $root; Write-Host 'Interrupted library drag fixture recovered' }
        return
    }
    if ($null -ne $recovery -and -not $SimulateSourceOwnerFailure) {
        throw 'the drag fixture left an active recovery marker; inspect ui-log.txt and process.log'
    }
    if ($SimulateSourceOwnerFailure -and ($null -eq $recovery -or -not $recovery.Attempted -or -not $recovery.Restored)) {
        throw 'the fixture did not recover after the source-owner failure'
    }
    $logPath = Join-Path $root 'ui-log.txt'
    if (-not [IO.File]::Exists($logPath)) {
        throw "the library drag fixture exited before producing its UI log (exit $status); inspect process.log"
    }
    $log = [IO.File]::ReadAllText($logPath)
    if ($probe) {
        if ((Test-AnyDragButton $probe.Buttons()) -or $probe.Modifiers()) {
            throw 'mouse buttons or modifiers remained active after the drop fixture'
        }
        $original = Read-DragState (Join-Path $root 'drag-original-pointer.json') 2
        if ($null -eq $original) { throw 'the fixture did not record its original pointer' }
        $current = $probe.Position()
        if ($SimulatePointerChange) {
            $pointerState = Read-DragState (Join-Path $root 'drag-pointer.json') 5 $true
            if ($null -eq $pointerState) { throw 'the fixture did not record its pointer handoff' }
            $expected = [double[]]@($pointerState[2] + 20, $pointerState[3])
            if (-not (Test-DragSamePoint $current $expected)) { throw 'the fixture did not preserve the simulated pointer movement' }
            if ((Test-AnyDragButton $probe.Buttons()) -or $probe.Modifiers() -or
                -not (Test-DragSamePoint $probe.Position() $current)) {
                throw 'input changed again; leaving the pointer at the newer position'
            }
            $probe.Restore([double[]]$original)
        }
        elseif (-not (Test-DragSamePoint $current $original)) {
            throw 'the pointer changed during the fixture; leaving it at the newer position'
        }
    }
    if ($ExternalDrop) {
        $markers = @($markers | ForEach-Object {
            if ($_ -ceq 'an external process read the active AppKit drag pasteboard') {
                'an external AppKit application accepted both dragged snapshots'
            }
            elseif ($_ -ceq 'active owner closure waited for one native completion before removing exports') {
                'a copied drag retained its snapshots after native completion'
            }
            else { $_ }
        })
    }
    if ($SimulateReceiverFailure) {
        $expected = if ($SimulateSourceOwnerFailure) {
            'failed external drag cleaned up: injected fixture source-owner failure; could not safely release the synthetic mouse button'
        }
        elseif ($SimulatePointerChange) {
            'failed external drag cleaned up: the pointer moved during the drop fixture; preserving its newer position'
        }
        else {
            'failed external drag cleaned up: external drag receiver failed: injected receiver exit after moving the fixture pointer'
        }
        if ($status -eq 0 -or -not $log.Contains($expected) -or -not $log.Contains('FAILED: a step panicked')) {
            throw "receiver-failure fixture did not clean up as expected (exit $status); inspect ui-log.txt and process.log"
        }
    }
    elseif ($status -ne 0) { throw "library drag fixture failed (exit $status); inspect ui-log.txt and process.log" }
    else { Assert-FixtureMarkers $logPath $markers }
}
catch {
    [Console]::Error.WriteLine("$($_.Exception.Message)`nFixture retained at $root")
    throw
}
if ($Output) { Write-Host "Library drag fixture passed: $root" }
else { Remove-FixtureRoot $root; Write-Host 'Library drag fixture passed' }
