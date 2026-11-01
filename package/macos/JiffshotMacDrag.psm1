#Requires -Version 7.4

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacPackage.psm1')
Import-Module (Join-Path $PSScriptRoot 'JiffshotMacFixture.psm1')

if (-not ('JiffshotFixture.DragProbe' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Threading;

namespace JiffshotFixture {
    [StructLayout(LayoutKind.Sequential)]
    public struct Point { public double X; public double Y; }

    public sealed class DragProbe {
        private const string Graphics = "/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics";
        private const string Foundation = "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation";
        [DllImport(Graphics)] private static extern IntPtr CGEventCreate(IntPtr source);
        [DllImport(Graphics)] private static extern Point CGEventGetLocation(IntPtr evt);
        [DllImport(Graphics)] [return: MarshalAs(UnmanagedType.I1)]
        private static extern bool CGEventSourceButtonState(int source, uint button);
        [DllImport(Graphics)] private static extern ulong CGEventSourceFlagsState(int source);
        [DllImport(Graphics)] private static extern int CGWarpMouseCursorPosition(Point point);
        [DllImport(Foundation)] private static extern void CFRelease(IntPtr value);

        public double[] Position() {
            var evt = CGEventCreate(IntPtr.Zero);
            if (evt == IntPtr.Zero) throw new InvalidOperationException("could not inspect the fixture pointer");
            try {
                var point = CGEventGetLocation(evt);
                return new[] { point.X, point.Y };
            } finally { CFRelease(evt); }
        }

        public bool[] Buttons() {
            var states = new bool[32];
            for (uint i = 0; i < states.Length; i++) states[i] = CGEventSourceButtonState(0, i);
            return states;
        }

        public bool Modifiers() { return (CGEventSourceFlagsState(0) & (0xFUL << 17)) != 0; }

        public void Restore(double[] position) {
            int status = CGWarpMouseCursorPosition(new Point { X = position[0], Y = position[1] });
            if (status != 0) throw new InvalidOperationException("could not restore the fixture pointer: " + status);
        }
    }

    public sealed class InterruptGuard : IDisposable {
        private int interrupted;
        private readonly PosixSignalRegistration interrupt;
        private readonly PosixSignalRegistration terminate;

        public InterruptGuard() {
            interrupt = PosixSignalRegistration.Create(PosixSignal.SIGINT, Handle);
            try { terminate = PosixSignalRegistration.Create(PosixSignal.SIGTERM, Handle); }
            catch { interrupt.Dispose(); throw; }
        }

        private void Handle(PosixSignalContext context) {
            context.Cancel = true;
            Interlocked.Exchange(ref interrupted, 1);
        }

        public void ThrowIfInterrupted() {
            if (Volatile.Read(ref interrupted) != 0)
                throw new OperationCanceledException("the library drag fixture was interrupted");
        }

        public void Dispose() { terminate.Dispose(); interrupt.Dispose(); }
    }
}
'@
}

function New-DragInputProbe { return [JiffshotFixture.DragProbe]::new() }
function New-DragInterruptGuard { return [JiffshotFixture.InterruptGuard]::new() }

function Test-DragSamePoint($First, $Second) {
    return ($First.Count -eq 2 -and $Second.Count -eq 2 -and
        [Math]::Abs([double]$First[0] - [double]$Second[0]) -lt 1 -and
        [Math]::Abs([double]$First[1] - [double]$Second[1]) -lt 1)
}

function Read-DragState([string]$Path, [int]$Count, [bool]$HasFlag = $false) {
    try { $bytes = [IO.File]::ReadAllBytes($Path) }
    catch [IO.FileNotFoundException] { return $null }
    catch { throw "could not read fixture state in $([IO.Path]::GetFileName($Path)): $($_.Exception.Message)" }
    try {
        $utf8 = [Text.UTF8Encoding]::new($false, $true)
        $text = $utf8.GetString($bytes)
        $values = ConvertFrom-Json -InputObject $text -AsHashtable -NoEnumerate -Depth 20
        if ($values -isnot [array] -or $values.Count -ne $Count -or
            ($HasFlag -and $values[-1] -isnot [bool])) { throw 'invalid shape' }
        $end = if ($HasFlag) { $Count - 1 } else { $Count }
        for ($index = 0; $index -lt $end; $index++) {
            $value = $values[$index]
            if ($value -isnot [long] -and $value -isnot [int] -and $value -isnot [double] -and $value -isnot [decimal]) {
                throw 'invalid coordinate'
            }
            if (-not [double]::IsFinite([double]$value)) { throw 'non-finite coordinate' }
        }
        return ,$values
    }
    catch { throw "invalid fixture state in $([IO.Path]::GetFileName($Path))" }
}

function Test-DragMarker([string]$Path) {
    try { $attributes = [IO.File]::GetAttributes($Path) }
    catch [IO.FileNotFoundException] { return $false }
    catch { throw "could not inspect fixture marker $([IO.Path]::GetFileName($Path)): $($_.Exception.Message)" }
    if ($attributes -band [IO.FileAttributes]::Directory) {
        throw "fixture marker $([IO.Path]::GetFileName($Path)) is not a file"
    }
    return $true
}

function Test-AnyDragButton($Buttons) {
    return @($Buttons | Where-Object { $_ }).Count -gt 0
}

function Recover-InterruptedDrag([string]$Root, $Probe, [string]$Receiver, [System.Collections.IDictionary]$Environment) {
    $guard = Join-Path $Root 'drag-active.json'
    $state = Read-DragState $guard 7 $true
    $receiverStarted = Test-DragMarker (Join-Path $Root 'drag-receiver-started')
    $receiverFinished = Test-DragMarker (Join-Path $Root 'drag-receiver-finished')
    $receiverExited = Join-Path $Root 'drag-receiver-exited'
    if ($receiverStarted -and -not $receiverFinished) {
        $clock = [Diagnostics.Stopwatch]::StartNew()
        while (-not (Test-DragMarker $receiverExited) -and $clock.Elapsed.TotalSeconds -lt 2) {
            Start-Sleep -Milliseconds 20
        }
        if (-not (Test-DragMarker $receiverExited)) {
            throw 'the drag receiver may still be active; not sending recovery input'
        }
    }
    $buttons = $Probe.Buttons()
    if ($null -eq $state) {
        if ($buttons[0]) { throw 'a mouse button is down without an active fixture drag; not sending recovery input' }
        return $null
    }
    $original = [double[]]@($state[0], $state[1])
    $sourceA = [double[]]@($state[2], $state[3])
    $sourceB = [double[]]@($state[4], $state[5])
    $known = [Collections.Generic.List[double[]]]::new()
    $known.Add($sourceA)
    $known.Add($sourceB)
    $receiverState = Read-DragState (Join-Path $Root 'drag-pointer.json') 5 $true
    if ($null -ne $receiverState) {
        if (-not $receiverStarted) { throw 'the drag receiver published state without a start marker' }
        $start = [double[]]@($receiverState[0], $receiverState[1])
        if ($receiverState[4]) {
            if (-not (Test-DragSamePoint $start $sourceA) -and -not (Test-DragSamePoint $start $sourceB)) {
                throw "the drag receiver's starting point does not match the fixture"
            }
            $known.Add([double[]]@($receiverState[2], $receiverState[3]))
        }
    }
    if (-not $buttons[0]) {
        Start-Sleep -Milliseconds 100
        $buttons = $Probe.Buttons()
    }
    if ((Test-AnyDragButton @($buttons | Select-Object -Skip 1)) -or $Probe.Modifiers()) {
        throw 'other input is active; not sending recovery input'
    }
    $current = $Probe.Position()
    $knownPosition = $false
    foreach ($point in $known) {
        if (Test-DragSamePoint $current $point) { $knownPosition = $true; break }
    }
    $attempted = $false
    if ($buttons[0]) {
        if (-not $state[6]) { throw 'a mouse button is down before the fixture press; not sending recovery input' }
        if (-not $knownPosition) { throw 'the pointer moved away from the fixture; not sending recovery input' }
        $attempted = $true
        $x = ([double]$current[0]).ToString('R', [Globalization.CultureInfo]::InvariantCulture)
        $y = ([double]$current[1]).ToString('R', [Globalization.CultureInfo]::InvariantCulture)
        try {
            $result = Invoke-MacCommand -File $Receiver -Arguments @('--recover-synthetic-button', $x, $y) `
                -Environment $Environment -TimeoutSeconds 10 -AllowFailure
        }
        catch {
            if ($_.Exception.Message.Contains('timed out')) { throw 'the fixture mouse-button recovery timed out' }
            throw
        }
        if ($result.ExitCode -ne 0 -or $result.Stdout.Trim() -cnotin @(
            'released the fixture mouse button', 'the fixture mouse button was already released'
        )) {
            $detail = if ($result.Stderr.Trim()) { $result.Stderr.Trim() } else { $result.Stdout.Trim() }
            throw "could not release the fixture mouse button: $detail"
        }
        if (Test-AnyDragButton $Probe.Buttons()) { throw 'a mouse button remained pressed after fixture recovery' }
    }
    Remove-Item -LiteralPath $guard
    $restored = $false
    if ($knownPosition -and -not (Test-AnyDragButton $Probe.Buttons()) -and -not $Probe.Modifiers() -and
        (Test-DragSamePoint $Probe.Position() $current)) {
        $Probe.Restore($original)
        $restored = $true
    }
    return [pscustomobject]@{ Attempted = $attempted; Restored = $restored }
}

function Wait-DragFixturePress($Fixture, [string]$Root, $Probe, $Interrupt, [bool]$ReceiverActive) {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    while ($clock.Elapsed.TotalSeconds -lt 60) {
        $Interrupt.ThrowIfInterrupted()
        if ($Fixture.Process.get_HasExited()) { throw "the drag fixture exited before its synthetic press: $($Fixture.Process.ExitCode)" }
        $state = Read-DragState (Join-Path $Root 'drag-active.json') 7 $true
        if ($null -ne $state -and $state[6] -and $Probe.Buttons()[0]) {
            if (-not $ReceiverActive) { return }
            $receiverState = Read-DragState (Join-Path $Root 'drag-pointer.json') 5 $true
            if ($null -ne $receiverState -and $receiverState[4]) { return }
        }
        Start-Sleep -Milliseconds 20
    }
    throw 'the drag fixture did not reach its synthetic press'
}

function Wait-DragFixtureExit($Fixture, $Interrupt, [int]$TimeoutSeconds) {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    while ($clock.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
        $Interrupt.ThrowIfInterrupted()
        if ($Fixture.Process.WaitForExit(100)) { return $true }
    }
    return $false
}

function Invoke-DragFixtureProcess(
    $Fixture, [string]$Root, $Probe, [string]$Receiver, [System.Collections.IDictionary]$Environment,
    $Interrupt, [bool]$SourceTimeout, [bool]$ReceiverTimeout
) {
    $failures = [Collections.Generic.List[string]]::new()
    $timedOut = $false
    try {
        if ($SourceTimeout -or $ReceiverTimeout) {
            Wait-DragFixturePress $Fixture $Root $Probe $Interrupt $ReceiverTimeout
            $timedOut = $true
        }
        elseif (-not (Wait-DragFixtureExit $Fixture $Interrupt 90)) {
            $timedOut = $true
        }
    }
    catch { $failures.Add($_.Exception.Message) }

    $stopped = $false
    try {
        Stop-FixtureProcess $Fixture
        $stopped = $true
    }
    catch {
        $failures.Add("could not stop drag fixture: $($_.Exception.Message)")
        if ($Fixture.Completed) { $stopped = $true }
        else {
            try { $stopped = $Fixture.Process.get_HasExited() }
            catch { $failures.Add("could not verify drag source exit: $($_.Exception.Message)") }
        }
    }

    $recovery = $null
    if ($Probe) {
        if ($stopped) {
            try { $recovery = Recover-InterruptedDrag $Root $Probe $Receiver $Environment }
            catch { $failures.Add("could not recover drag fixture: $($_.Exception.Message)") }
        }
        else {
            $failures.Add('the drag source may still be active; not sending recovery input')
        }
    }
    if ($failures.Count -gt 0) { throw ($failures -join '; ') }
    return [pscustomobject]@{ TimedOut = $timedOut; ExitCode = $Fixture.ExitCode; Recovery = $recovery }
}

Export-ModuleMember -Function New-DragInputProbe, New-DragInterruptGuard, Test-DragSamePoint, Read-DragState, `
    Test-AnyDragButton, Recover-InterruptedDrag, Wait-DragFixturePress, Wait-DragFixtureExit, Invoke-DragFixtureProcess
