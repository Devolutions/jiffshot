[CmdletBinding()]
param(
    [string] $Binary,
    [switch] $DisposableProfile,
    [string] $ProfileSid,
    [ValidateSet('Fresh', 'Existing')]
    [string] $ProfileState = 'Existing',
    [switch] $VerifyOptOutInteractively,
    [switch] $VerifyWindowsDisableInteractively,
    [switch] $VerifyUserNoticeInteractively,
    [ValidateRange(10, 300)]
    [int] $TimeoutSeconds = 45
)

<#
.SYNOPSIS
Exercise portable/MSI startup in a disposable, interactive Windows profile.

.DESCRIPTION
This changes that profile's HKCU Run value and launches the real GUI, including
a screen-selection overlay. Use a throwaway VM or user profile with an
interactive Explorer desktop, no other Jiffshot processes, and no concurrent
startup tests or logon activity. The switch is an attestation, not a way to
detect whether a profile is actually disposable. The script refuses existing
Run and StartupApproved values, including empty or disabled values.

Run Fresh and Existing in different clean profiles. Existing seeds an old
settings file with no startup marker and the Capture Widget enabled. Fresh
starts without a settings file, then enables the widget in scratch settings
after the first launch. A path containing spaces and Unicode is useful when
checking command quoting. JIFFSHOT_UITEST is deliberately not set: that mode
blocks real Windows startup registration.

The scratch background fixtures separately contain an unsupported capture
hotkey and a malformed startup record. The harness verifies that each
automatic warning is generated, stays hidden until Jiffshot opens, and then
produces a startup notice. The original scratch settings are restored.

  $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
  pwsh -NoProfile -File .\app\tests\windows_startup_e2e.ps1 `
    -Binary 'C:\Portable Jiffshot\jiffshot.exe' -DisposableProfile `
    -ProfileSid $sid -ProfileState Existing

The optional interactive checks ask the operator to change settings in the
disposable profile. The Windows-disable check only compares the opaque
StartupApproved record before/after a normal relaunch; the operator must verify
in Windows that the app is disabled. The script never writes or removes that
record. Discard the profile afterwards; Windows may leave approval metadata.
With -VerifyUserNoticeInteractively, put text rather than an image on the
disposable profile's clipboard, then use the tray's "From the clipboard"
action while the app is still hidden. The script waits for its visible toast.

The automated checks execute the exact Run command via CreateProcess, but that
is NOT a Windows logoff/sign-in. They prove a resident process and hidden
windows, not that Explorer displayed a usable notification icon. To avoid a
session-wide quit command, the script force-terminates only processes created
through its retained handles. For acceptance in a disposable VM, separately:

  1. In a clean profile, launch normally, enable the Capture Widget and verify
     the portable/MSI Run entry. Sign out and sign back in for real.
  2. Verify the Jiffshot tray icon is usable and no app window, widget, or
     automatic notification is shown. With no image on the clipboard, use
     the tray's "From the clipboard" action and verify its user-initiated
     notice is visible without opening Jiffshot. Open Jiffshot from the tray
     and verify the saved widget returns. When an automatic warning was
     deliberately provoked, verify it appears after opening.
     Exercise --window, opening an image, and a capture.
  3. Opt out in Jiffshot, restart, upgrade and sign in again; it must stay off.
     Separately disable startup in Windows Settings/Task Manager and verify
     normal launches and another sign-in do not re-enable it. Also upgrade a
     previous installation whose entry was already Windows-disabled, and check
     policy in a managed VM. The script refuses that pre-existing state.
  4. Repeat the real sign-in checks for an installed MSIX, including startup
     task activation for JiffshotStartup and Windows-controlled disable. This
     script does not install or exercise an MSIX.

The manual logon uses the VM user's real profile settings. The Run command has
no JIFFSHOT_* environment overrides, so the script's scratch settings do not
automatically survive into Explorer's logon environment.
#>

$script:StartupRunKey = 'Software\Microsoft\Windows\CurrentVersion\Run'
$script:StartupApprovalKeys = @(
    'Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run',
    'Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run32'
)
$script:StartupValueName = 'Jiffshot'
$script:UnsupportedHotkey = 'jiffshot-e2e-unsupported-hotkey'
$script:InvalidStartupBackend = 'jiffshot-e2e-invalid-backend'

# The functions above the entry point are also used by the isolated harness
# tests. Dot-sourcing this file only defines functions; it never runs the E2E.
function Assert-DisposableConsent {
    param([bool] $OptedIn, [string] $ExpectedSid, [string] $CurrentSid, [bool] $Windows, [bool] $Interactive)

    if (-not $OptedIn) {
        throw 'Pass -DisposableProfile only in an exclusive, disposable Windows user/VM.'
    }
    if (-not $Windows -or -not $Interactive) {
        throw 'An interactive Windows user session is required.'
    }
    if ([string]::IsNullOrWhiteSpace($ExpectedSid) -or
        -not [string]::Equals($ExpectedSid, $CurrentSid, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Pass -ProfileSid with the exact SID of the disposable current user.'
    }
}

function Get-StartupHarnessMutexName {
    param([string] $Sid)
    return 'Global\Jiffshot.StartupE2E.' + $Sid
}

function Assert-PortableBinaryLocation {
    param([string] $Path, [IO.DriveType] $DriveType)

    if ($Path -notmatch '^[A-Za-z]:\\' -or $Path -match '(?i)\\WindowsApps\\' -or $Path.Contains('"') -or
        $DriveType -notin @([IO.DriveType]::Fixed, [IO.DriveType]::Removable, [IO.DriveType]::CDRom)) {
        throw 'Use an unpackaged portable/MSI executable on a local drive, not an MSIX or network path.'
    }
}

function Assert-CleanStartupState {
    param([object[]] $Entries)

    foreach ($entry in $Entries) {
        if ($entry.Present) {
            throw "Refusing an existing Jiffshot value in $($entry.View): $($entry.KeyPath). Nothing was changed."
        }
    }
}

function Test-ExpectedStartupCommand {
    param($Entry, [string] $BinaryPath)

    if (-not $Entry.Present -or [string] $Entry.Kind -cne 'String' -or
        $Entry.Value -isnot [string]) {
        return $false
    }
    $match = [regex]::Match($Entry.Value, '^"(?<path>[^"]+)" --background$')
    return $match.Success -and
        [string]::Equals($match.Groups['path'].Value, $BinaryPath, [StringComparison]::OrdinalIgnoreCase)
}

function Get-RunRemovalDecision {
    param([bool] $CleanBaseline, [bool] $ObservedOwned, $Original, $Current)

    if (-not $Current.Present) { return 'absent' }
    if (-not $CleanBaseline -or -not $ObservedOwned -or $null -eq $Original -or
        -not $Original.Present -or $Original.Value -isnot [string] -or
        $Current.Value -isnot [string]) {
        return 'preserve'
    }
    foreach ($property in @('KeyPath', 'View', 'Name', 'Kind')) {
        if (-not [string]::Equals([string] $Original.$property, [string] $Current.$property,
                [StringComparison]::OrdinalIgnoreCase)) {
            return 'preserve'
        }
    }
    if (-not [string]::Equals($Original.Value, $Current.Value, [StringComparison]::Ordinal)) {
        return 'preserve'
    }
    return 'remove'
}

function Invoke-GuardedRunRemoval {
    param([bool] $CleanBaseline, [bool] $ObservedOwned, $Original, $Current, [scriptblock] $Delete)
    $decision = Get-RunRemovalDecision $CleanBaseline $ObservedOwned $Original $Current
    if ($decision -eq 'remove') { & $Delete | Out-Null }
    return $decision
}

function Invoke-StartupHarnessCleanup {
    param($Mutex, [scriptblock] $Cleanup)

    try {
        & $Cleanup
    } finally {
        try { $Mutex.ReleaseMutex() } finally { $Mutex.Dispose() }
    }
}

function Assert-RecordedRunValues {
    param([object[]] $OwnedSnapshots, [object[]] $CurrentValues)

    if ($OwnedSnapshots.Count -eq 0) { throw 'No test-owned Run value was recorded.' }
    foreach ($original in $OwnedSnapshots) {
        $matching = @($CurrentValues | Where-Object { $_.View -eq $original.View })
        if ($matching.Count -ne 1 -or
            (Get-RunRemovalDecision $true $true $original $matching[0]) -ne 'remove') {
            throw "The recorded Jiffshot Run value changed or disappeared in $($original.View)."
        }
    }
    foreach ($current in $CurrentValues) {
        if ($current.Present -and @($OwnedSnapshots | Where-Object { $_.View -eq $current.View }).Count -ne 1) {
            throw "An unowned Jiffshot Run value appeared in $($current.View)."
        }
    }
}

function Assert-OwnedProcessSet {
    param([uint32[]] $RunningIds, [uint32[]] $OwnedLiveIds, [uint32] $PrimaryId, [uint32] $TrayOwnerId)

    if ($PrimaryId -eq 0 -or $PrimaryId -notin $OwnedLiveIds -or
        $PrimaryId -notin $RunningIds -or $TrayOwnerId -ne $PrimaryId) {
        throw 'The running tray window is not owned by the test-created primary process.'
    }
    foreach ($id in $RunningIds) {
        if ($id -notin $OwnedLiveIds) {
            throw "Refusing to send a launch/command while unowned Jiffshot PID $id exists."
        }
    }
}

function Assert-BackgroundSnapshot {
    param(
        [object[]] $Windows, [uint32] $ForegroundId, [uint32] $PrimaryId,
        [bool] $ForegroundEvent, [bool] $VisibleEvent
    )

    $visible = @($Windows | Where-Object { $_.Visible -and -not $_.Cloaked })
    if ($visible.Count -ne 0) {
        $details = ($visible | ForEach-Object { "'$($_.Title)' [$($_.ClassName)]" }) -join ', '
        throw "Background Jiffshot has visible top-level windows: $details"
    }
    if ($ForegroundId -eq $PrimaryId) {
        throw 'Background Jiffshot owns the foreground window.'
    }
    if ($ForegroundEvent) { throw 'Background Jiffshot received a foreground event.' }
    if ($VisibleEvent) { throw 'Background Jiffshot showed a top-level window or notification.' }
}

function Get-RegistryViews {
    if ([Environment]::Is64BitOperatingSystem) {
        return @([Microsoft.Win32.RegistryView]::Registry64, [Microsoft.Win32.RegistryView]::Registry32)
    }
    return @([Microsoft.Win32.RegistryView]::Default)
}

function Read-StartupValueFromKey {
    param($Key, [string] $KeyPath, [Microsoft.Win32.RegistryView] $View)

    $result = [pscustomobject] @{
        KeyPath = $KeyPath
        View = $View.ToString()
        Name = $script:StartupValueName
        Present = $false
        Kind = $null
        Value = $null
    }
    if ($null -ne $Key -and @($Key.GetValueNames() | Where-Object { $_ -ieq $script:StartupValueName }).Count) {
        $result.Present = $true
        $result.Kind = $Key.GetValueKind($script:StartupValueName).ToString()
        $result.Value = $Key.GetValue(
            $script:StartupValueName, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    }
    return $result
}

function Read-StartupValue {
    param([string] $KeyPath, [Microsoft.Win32.RegistryView] $View)

    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, $View)
    $key = $null
    try {
        $key = $base.OpenSubKey($KeyPath, $false)
        return Read-StartupValueFromKey $key $KeyPath $View
    } finally {
        if ($null -ne $key) { $key.Dispose() }
        $base.Dispose()
    }
}

function Get-RunValues {
    foreach ($view in @(Get-RegistryViews)) {
        Read-StartupValue $script:StartupRunKey $view
    }
}

function Get-ApprovalValues {
    foreach ($view in @(Get-RegistryViews)) {
        foreach ($key in $script:StartupApprovalKeys) {
            Read-StartupValue $key $view
        }
    }
}

function Get-ApprovalSignature {
    param([object[]] $Entries)

    foreach ($entry in $Entries) {
        if (-not $entry.Present) {
            '{0}|{1}|absent' -f $entry.View, $entry.KeyPath
        } elseif ($entry.Kind -ceq 'Binary' -and $entry.Value -is [byte[]]) {
            '{0}|{1}|Binary|{2}' -f $entry.View, $entry.KeyPath, [Convert]::ToBase64String($entry.Value)
        } else {
            throw "Unexpected StartupApproved value type in $($entry.View): $($entry.Kind)"
        }
    }
}

function Remove-OwnedRunValues {
    param([bool] $CleanBaseline, [object[]] $OwnedSnapshots)

    $safe = $true
    foreach ($view in @(Get-RegistryViews)) {
        $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, $view)
        $key = $null
        try {
            # Open and re-read the exact value immediately before deleting it.
            $key = $base.OpenSubKey($script:StartupRunKey, $true)
            $current = Read-StartupValueFromKey $key $script:StartupRunKey $view
            $original = @($OwnedSnapshots | Where-Object { $_.View -eq $view.ToString() }) | Select-Object -First 1
            $decision = Invoke-GuardedRunRemoval $CleanBaseline ($null -ne $original) $original $current {
                $key.DeleteValue($script:StartupValueName, $true)
            }
            if ($decision -eq 'remove') {
                Write-Host "Removed the unchanged test-owned HKCU Run value ($view)."
            } elseif ($decision -eq 'preserve') {
                $safe = $false
                Write-Warning "Preserved an unowned or changed Jiffshot Run value in $view; inspect it manually."
            }
        } finally {
            if ($null -ne $key) { $key.Dispose() }
            $base.Dispose()
        }
    }
    return $safe
}

function Initialize-StartupNative {
    if ('JiffshotStartupE2E.Native' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Collections;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using Microsoft.Win32.SafeHandles;

namespace JiffshotStartupE2E {
    public sealed class WindowInfo {
        public string Title;
        public string ClassName;
        public bool Visible;
        public bool Cloaked;
    }

    // The handle is returned by CreateProcess itself and stays open until
    // cleanup. Termination never reopens a PID, so PID reuse cannot redirect it.
    public sealed class OwnedProcess : IDisposable {
        private readonly SafeProcessHandle handle;
        public uint Id { get; private set; }
        internal OwnedProcess(IntPtr process, uint id) {
            handle = new SafeProcessHandle(process, true);
            Id = id;
        }
        public bool IsRunning {
            get {
                uint result = Native.WaitForSingleObject(handle, 0);
                if (result == 258) return true;
                if (result == 0) return false;
                throw new Win32Exception(Marshal.GetLastWin32Error(), "WaitForSingleObject");
            }
        }
        public uint ExitCode {
            get {
                if (IsRunning) throw new InvalidOperationException("The process is still running.");
                uint code;
                if (!Native.GetExitCodeProcess(handle, out code)) throw new Win32Exception();
                return code;
            }
        }
        public string ImagePath {
            get {
                var name = new StringBuilder(32768);
                int size = name.Capacity;
                if (!Native.QueryFullProcessImageName(handle, 0, name, ref size)) throw new Win32Exception();
                return name.ToString();
            }
        }
        public void Stop() {
            if (!IsRunning) return;
            if (!Native.TerminateProcess(handle, 1) && IsRunning) throw new Win32Exception();
            if (Native.WaitForSingleObject(handle, 10000) != 0)
                throw new InvalidOperationException("The owned process did not terminate.");
        }
        public void Dispose() { handle.Dispose(); }
    }

    // Kept independent of Win32 so the event filtering can be tested without
    // creating or inspecting any real windows.
    public sealed class WindowEvents {
        private readonly HashSet<uint> foreground = new HashSet<uint>();
        private readonly HashSet<uint> shown = new HashSet<uint>();
        private readonly object gate = new object();

        public void Record(uint type, uint owner, bool topLevel, bool cloaked) {
            if (owner == 0) return;
            lock (gate) {
                if (type == 3) foreground.Add(owner);             // EVENT_SYSTEM_FOREGROUND
                if (type == 0x8002 && topLevel && !cloaked) shown.Add(owner); // EVENT_OBJECT_SHOW
            }
        }
        public bool SawForeground(uint owner) { lock (gate) return foreground.Contains(owner); }
        public bool SawShown(uint owner) { lock (gate) return shown.Contains(owner); }
    }

    public sealed class WindowObserver : IDisposable {
        private const uint SyncMessage = 0x8001;
        private readonly WindowEvents events = new WindowEvents();
        private readonly ManualResetEvent ready = new ManualResetEvent(false);
        private readonly AutoResetEvent synchronized = new AutoResetEvent(false);
        private readonly Thread thread;
        private Native.WinEventProc callback;
        private uint threadId;
        private int failure;

        internal WindowObserver() {
            thread = new Thread(Observe);
            thread.IsBackground = true;
            thread.Start();
            if (!ready.WaitOne(5000)) throw new InvalidOperationException("Window observer did not start.");
            if (failure != 0) throw new Win32Exception(failure, "SetWinEventHook");
        }
        private void Observe() {
            threadId = Native.GetCurrentThreadId();
            Native.MSG message;
            Native.PeekMessage(out message, IntPtr.Zero, 0, 0, 0); // create the message queue
            callback = OnEvent;
            IntPtr foreground = Native.SetWinEventHook(3, 3, IntPtr.Zero, callback, 0, 0, 0);
            if (foreground == IntPtr.Zero) {
                failure = Marshal.GetLastWin32Error();
                if (failure == 0) failure = 1;
                ready.Set();
                return;
            }
            IntPtr shown = Native.SetWinEventHook(0x8002, 0x8002, IntPtr.Zero, callback, 0, 0, 0);
            if (shown == IntPtr.Zero) {
                failure = Marshal.GetLastWin32Error();
                if (failure == 0) failure = 1;
                Native.UnhookWinEvent(foreground);
                ready.Set();
                return;
            }
            ready.Set();
            try {
                while (Native.GetMessage(out message, IntPtr.Zero, 0, 0) > 0) {
                    if (message.message == SyncMessage) {
                        synchronized.Set();
                        continue;
                    }
                    Native.TranslateMessage(ref message);
                    Native.DispatchMessage(ref message);
                }
            } finally {
                Native.UnhookWinEvent(shown);
                Native.UnhookWinEvent(foreground);
            }
        }
        private void OnEvent(IntPtr hook, uint type, IntPtr window, int obj, int child, uint source, uint time) {
            if (window == IntPtr.Zero) return;
            if (type == 0x8002 && (obj != 0 || child != 0 || Native.GetAncestor(window, 2) != window)) return;
            uint owner;
            Native.GetWindowThreadProcessId(window, out owner);
            int cloak = 0;
            bool cloaked = type == 0x8002 &&
                Native.DwmGetWindowAttribute(window, 14, out cloak, sizeof(int)) >= 0 && cloak != 0;
            // A show event for a top-level window matters even if it is hidden
            // again before polling. A failed cloak query is treated as visible.
            events.Record(type, owner, true, cloaked);
        }
        public bool SawForeground(uint processId) {
            if (!thread.IsAlive) throw new InvalidOperationException("Window observer stopped unexpectedly.");
            return events.SawForeground(processId);
        }
        public bool SawShown(uint processId) {
            if (!thread.IsAlive) throw new InvalidOperationException("Window observer stopped unexpectedly.");
            return events.SawShown(processId);
        }
        public void Flush() {
            if (!thread.IsAlive) throw new InvalidOperationException("Window observer stopped unexpectedly.");
            if (!Native.PostThreadMessage(threadId, SyncMessage, UIntPtr.Zero, IntPtr.Zero))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "Synchronize window observer");
            if (!synchronized.WaitOne(5000)) throw new InvalidOperationException("Window observer did not synchronize.");
        }
        public void Dispose() {
            if (thread.IsAlive) {
                if (!Native.PostThreadMessage(threadId, 0x0012, UIntPtr.Zero, IntPtr.Zero))
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "Stop window observer");
                if (!thread.Join(5000)) throw new InvalidOperationException("Window observer did not stop.");
            }
            ready.Dispose();
            synchronized.Dispose();
        }
    }

    public static class Native {
        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        private struct STARTUPINFO {
            public int cb; public string reserved; public string desktop; public string title;
            public int x, y, xSize, ySize, xChars, yChars, fill, flags;
            public short show, reserved2; public IntPtr reservedPtr, input, output, error;
        }
        [StructLayout(LayoutKind.Sequential)]
        private struct PROCESS_INFORMATION { public IntPtr process, thread; public uint id, threadId; }
        [StructLayout(LayoutKind.Sequential)]
        internal struct MSG {
            public IntPtr window; public uint message; public UIntPtr wParam; public IntPtr lParam;
            public uint time; public int x, y; public uint privateData;
        }
        internal delegate void WinEventProc(IntPtr hook, uint type, IntPtr window, int obj, int child, uint source, uint time);
        private delegate bool EnumProc(IntPtr window, IntPtr state);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern bool CreateProcessW(string app, StringBuilder command, IntPtr processAttributes,
            IntPtr threadAttributes, bool inherit, uint flags, IntPtr environment, string directory,
            ref STARTUPINFO startup, out PROCESS_INFORMATION process);
        [DllImport("kernel32.dll", SetLastError = true)] internal static extern uint WaitForSingleObject(SafeProcessHandle handle, uint timeout);
        [DllImport("kernel32.dll", SetLastError = true)] internal static extern bool GetExitCodeProcess(SafeProcessHandle handle, out uint code);
        [DllImport("kernel32.dll", SetLastError = true)] internal static extern bool TerminateProcess(SafeProcessHandle handle, uint code);
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        internal static extern bool QueryFullProcessImageName(SafeProcessHandle handle, uint flags, StringBuilder name, ref int size);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern bool CloseHandle(IntPtr handle);
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern IntPtr OpenMutexW(uint access, bool inherit, string name);
        [DllImport("kernel32.dll")] internal static extern uint GetCurrentThreadId();
        [DllImport("kernel32.dll", SetLastError = true)] private static extern bool ProcessIdToSessionId(uint process, out uint session);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] private static extern IntPtr FindWindowW(string className, string title);
        [DllImport("user32.dll", SetLastError = true)] private static extern bool EnumWindows(EnumProc callback, IntPtr state);
        [DllImport("user32.dll")] private static extern bool IsWindowVisible(IntPtr window);
        [DllImport("user32.dll")] private static extern IntPtr GetForegroundWindow();
        [DllImport("user32.dll")] internal static extern IntPtr GetAncestor(IntPtr window, uint flags);
        [DllImport("user32.dll", SetLastError = true)] internal static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] private static extern int GetWindowTextW(IntPtr window, StringBuilder text, int max);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] private static extern int GetClassNameW(IntPtr window, StringBuilder text, int max);
        [DllImport("dwmapi.dll")] internal static extern int DwmGetWindowAttribute(IntPtr window, int attribute, out int value, int size);
        [DllImport("user32.dll", SetLastError = true)] internal static extern IntPtr SetWinEventHook(uint first, uint last, IntPtr module, WinEventProc callback, uint process, uint thread, uint flags);
        [DllImport("user32.dll")] internal static extern bool UnhookWinEvent(IntPtr hook);
        [DllImport("user32.dll")] internal static extern int GetMessage(out MSG message, IntPtr window, uint first, uint last);
        [DllImport("user32.dll")] internal static extern bool PeekMessage(out MSG message, IntPtr window, uint first, uint last, uint remove);
        [DllImport("user32.dll")] internal static extern bool TranslateMessage(ref MSG message);
        [DllImport("user32.dll")] internal static extern IntPtr DispatchMessage(ref MSG message);
        [DllImport("user32.dll", SetLastError = true)] internal static extern bool PostThreadMessage(uint thread, uint message, UIntPtr wParam, IntPtr lParam);
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern SafeFileHandle CreateFileW(string path, uint access, uint share, IntPtr security, uint creation, uint flags, IntPtr template);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern bool GetNamedPipeServerProcessId(SafeFileHandle pipe, out uint process);
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)] private static extern bool WaitNamedPipeW(string name, uint timeout);

        public static IDictionary<string, string> ChildEnvironment(
                IDictionary source, string config, string local, string library) {
            var environment = new SortedDictionary<string, string>(StringComparer.OrdinalIgnoreCase);
            foreach (DictionaryEntry item in source) {
                string key = (string)item.Key;
                if (key.StartsWith("JIFFSHOT_", StringComparison.OrdinalIgnoreCase) ||
                    key.StartsWith("RUSTOLONIA_", StringComparison.OrdinalIgnoreCase) || key.StartsWith("=")) continue;
                environment[key] = (string)item.Value;
            }
            environment["JIFFSHOT_CONFIG_DIR"] = config;
            environment["JIFFSHOT_LOCAL_DIR"] = local;
            environment["JIFFSHOT_LIBRARY"] = library;
            environment["APPDATA"] = config;
            environment["LOCALAPPDATA"] = local;
            return environment;
        }
        public static OwnedProcess Start(string command, string directory, string config, string local, string library) {
            IDictionary<string, string> environment = ChildEnvironment(
                Environment.GetEnvironmentVariables(), config, local, library);
            var block = new StringBuilder();
            foreach (var item in environment) block.Append(item.Key).Append('=').Append(item.Value).Append('\0');
            block.Append('\0');
            IntPtr environmentBlock = Marshal.StringToHGlobalUni(block.ToString());
            try {
                var startup = new STARTUPINFO();
                startup.cb = Marshal.SizeOf(typeof(STARTUPINFO));
                PROCESS_INFORMATION process;
                // Null application name makes Windows parse the actual Run command.
                if (!CreateProcessW(null, new StringBuilder(command), IntPtr.Zero, IntPtr.Zero, false,
                        0x00000400, environmentBlock, directory, ref startup, out process))
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateProcess");
                CloseHandle(process.thread);
                return new OwnedProcess(process.process, process.id);
            } finally {
                Marshal.FreeHGlobal(environmentBlock);
            }
        }
        public static bool InteractiveShell() {
            return FindWindowW("Shell_TrayWnd", null) != IntPtr.Zero && GetForegroundWindow() != IntPtr.Zero;
        }
        public static bool InstanceMutexExists() {
            IntPtr mutex = OpenMutexW(0x00100000, false, @"Local\Jiffshot.Capture");
            if (mutex != IntPtr.Zero) { CloseHandle(mutex); return true; }
            int error = Marshal.GetLastWin32Error();
            if (error == 2) return false;
            throw new Win32Exception(error, "Could not safely inspect the Jiffshot instance mutex");
        }
        public static uint TrayOwner() {
            IntPtr window = FindWindowW("JiffshotCaptureTray", null);
            uint owner = 0;
            if (window != IntPtr.Zero && (GetWindowThreadProcessId(window, out owner) == 0 || owner == 0))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "Could not identify the tray window owner");
            return owner;
        }
        public static uint ForegroundOwner() {
            uint owner = 0;
            IntPtr window = GetForegroundWindow();
            if (window != IntPtr.Zero) GetWindowThreadProcessId(window, out owner);
            return owner;
        }
        public static uint Session(uint process) {
            uint session;
            if (!ProcessIdToSessionId(process, out session)) throw new Win32Exception();
            return session;
        }
        public static WindowInfo[] Windows(uint process) {
            var windows = new List<WindowInfo>();
            int failure = 0;
            bool enumerated = EnumWindows(delegate(IntPtr window, IntPtr state) {
                uint owner;
                GetWindowThreadProcessId(window, out owner);
                if (owner != process) return true;
                bool visible = IsWindowVisible(window);
                int cloaked = 0;
                if (visible) {
                    failure = DwmGetWindowAttribute(window, 14, out cloaked, sizeof(int));
                    if (failure < 0) return false;
                }
                var title = new StringBuilder(2048);
                var className = new StringBuilder(256);
                GetWindowTextW(window, title, title.Capacity);
                GetClassNameW(window, className, className.Capacity);
                windows.Add(new WindowInfo { Title = title.ToString(), ClassName = className.ToString(),
                    Visible = visible, Cloaked = cloaked != 0 });
                return true;
            }, IntPtr.Zero);
            if (failure < 0) Marshal.ThrowExceptionForHR(failure);
            if (!enumerated) throw new Win32Exception(Marshal.GetLastWin32Error(), "EnumWindows");
            return windows.ToArray();
        }
        public static WindowObserver WatchWindows() { return new WindowObserver(); }
        private static string EditorPipe(string sid, uint session) {
            return @"\\.\pipe\Jiffshot.Editor." + sid + "." + session;
        }
        // Connect without sending a command, solely to verify the server PID.
        public static uint EditorPipeOwner(string sid, uint session) {
            using (SafeFileHandle pipe = CreateFileW(EditorPipe(sid, session), 0x40000000, 0,
                    IntPtr.Zero, 3, 0, IntPtr.Zero)) {
                if (pipe.IsInvalid) {
                    int error = Marshal.GetLastWin32Error();
                    if (error == 2 || error == 231) return 0;
                    throw new Win32Exception(error, "Could not inspect the editor pipe owner");
                }
                uint owner;
                if (!GetNamedPipeServerProcessId(pipe, out owner)) throw new Win32Exception();
                return owner;
            }
        }
        public static bool EditorPipeAvailable(string sid, uint session) {
            if (WaitNamedPipeW(EditorPipe(sid, session), 50)) return true;
            int error = Marshal.GetLastWin32Error();
            if (error == 2 || error == 121 || error == 231) return false;
            throw new Win32Exception(error, "Could not inspect the editor pipe");
        }
    }
}
'@
}

function Get-JiffshotProcessIds {
    $processes = [Diagnostics.Process]::GetProcessesByName('jiffshot')
    try {
        foreach ($process in $processes) {
            try {
                if (-not $process.HasExited) { [uint32] $process.Id }
            } catch [InvalidOperationException] {
                # The process exited while the snapshot was being inspected.
            }
        }
    } finally {
        foreach ($process in $processes) { $process.Dispose() }
    }
}

function Assert-ClearInstanceSnapshot {
    param([uint32[]] $RunningIds, [bool] $InstanceMutexExists, [uint32] $TrayOwnerId)
    if ($RunningIds.Count -ne 0 -or $InstanceMutexExists -or $TrayOwnerId -ne 0) {
        throw "Refusing an existing Jiffshot instance or session mutex (PIDs: $($RunningIds -join ', '))."
    }
}

function Assert-NoExistingInstance {
    $ids = @(Get-JiffshotProcessIds)
    if ($ids.Count -ne 0) { Assert-ClearInstanceSnapshot $ids $false 0 }
    Assert-ClearInstanceSnapshot $ids ([JiffshotStartupE2E.Native]::InstanceMutexExists()) `
        ([JiffshotStartupE2E.Native]::TrayOwner())
}

function Assert-OwnedInstance {
    param($Primary, [object[]] $Processes, [string] $BinaryPath, [uint32] $Session)

    if (-not $Primary.IsRunning) { throw "Owned primary PID $($Primary.Id) exited unexpectedly." }
    if (-not [string]::Equals($Primary.ImagePath, $BinaryPath, [StringComparison]::OrdinalIgnoreCase) -or
        [JiffshotStartupE2E.Native]::Session($Primary.Id) -ne $Session) {
        throw 'The primary process image or session does not match the test setup.'
    }
    $owned = @($Processes | Where-Object { $_.IsRunning } | ForEach-Object { [uint32] $_.Id })
    Assert-OwnedProcessSet @(Get-JiffshotProcessIds) $owned $Primary.Id ([JiffshotStartupE2E.Native]::TrayOwner())
}

function Wait-Until {
    param([scriptblock] $Condition, [string] $Description, [int] $Seconds, $Primary = $null)

    $clock = [Diagnostics.Stopwatch]::StartNew()
    do {
        if ($null -ne $Primary -and -not $Primary.IsRunning) {
            throw "Owned PID $($Primary.Id) exited while waiting for $Description."
        }
        if (& $Condition) { return }
        Start-Sleep -Milliseconds 100
    } while ($clock.Elapsed.TotalSeconds -lt $Seconds)
    throw "Timed out waiting for $Description."
}

function Wait-OwnedTray {
    param($Primary, [int] $Seconds, $Observer = $null)
    Wait-Until {
        if ($null -ne $Observer) { Assert-BackgroundWindows $Primary $Observer }
        $owner = [JiffshotStartupE2E.Native]::TrayOwner()
        if ($owner -ne 0 -and $owner -ne $Primary.Id) {
            throw "The Jiffshot tray window belongs to unowned PID $owner."
        }
        $owner -eq $Primary.Id
    } 'the test-owned tray message window' $Seconds $Primary
}

function Wait-OwnedExit {
    param($Process, [int] $Seconds, [string] $Description, $Observer = $null)
    Wait-Until {
        if ($null -ne $Observer) { Assert-BackgroundWindows $Process $Observer }
        -not $Process.IsRunning
    } $Description $Seconds
    if ($Process.ExitCode -ne 0) { throw "$Description exited with code $($Process.ExitCode)." }
}

function Wait-EmptySession {
    param([int] $Seconds)
    Wait-Until {
        $ids = @(Get-JiffshotProcessIds)
        if ($ids.Count -ne 0) { throw "An unowned or unexpectedly running Jiffshot PID remains: $($ids -join ', ')." }
        [JiffshotStartupE2E.Native]::TrayOwner() -eq 0 -and
            -not [JiffshotStartupE2E.Native]::InstanceMutexExists()
    } 'the test session to release its instance' $Seconds
}

function Start-OwnedCommand {
    param([string] $Command, $Context)
    $process = [JiffshotStartupE2E.Native]::Start(
        $Command, $Context.Root, $Context.Config, $Context.Local, $Context.Library)
    [void] $Context.Processes.Add($process)
    Write-Host "Started test-owned PID $($process.Id): $Command"
    return $process
}

function Assert-BackgroundWindows {
    param($Process, $Observer)

    Assert-BackgroundSnapshot @([JiffshotStartupE2E.Native]::Windows($Process.Id)) `
        ([JiffshotStartupE2E.Native]::ForegroundOwner()) $Process.Id `
        ($Observer.SawForeground($Process.Id)) ($Observer.SawShown($Process.Id))
}

function Observe-Hidden {
    param($Primary, $Observer, [object[]] $Processes, [string] $BinaryPath, [uint32] $Session)

    $clock = [Diagnostics.Stopwatch]::StartNew()
    do {
        Assert-OwnedInstance $Primary $Processes $BinaryPath $Session
        Assert-BackgroundWindows $Primary $Observer
        Start-Sleep -Milliseconds 100
    } while ($clock.Elapsed.TotalSeconds -lt 4)
    $Observer.Flush()
    Assert-BackgroundWindows $Primary $Observer
}

function Wait-VisibleTitle {
    param($Primary, [string] $Title, [int] $Seconds, [switch] $Contains)
    Wait-Until {
        $found = @([JiffshotStartupE2E.Native]::Windows($Primary.Id) | Where-Object {
            $_.Visible -and -not $_.Cloaked -and
                $(if ($Contains) { $_.Title.Contains($Title) } else { $_.Title -ceq $Title })
        })
        $found.Count -ne 0
    } "the visible '$Title' window" $Seconds $Primary
}

function Read-ScratchSettings {
    param([string] $Path)
    if (-not (Test-Path -LiteralPath $Path)) { return [pscustomobject] @{} }
    $settings = Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
    if ($null -eq $settings -or $settings -isnot [pscustomobject]) {
        throw 'Scratch Jiffshot settings are not a JSON object.'
    }
    return $settings
}

function Assert-SavedWidget {
    param([string] $Path)
    $settings = Read-ScratchSettings $Path
    if ($null -eq $settings.capture -or $settings.capture.show_widget -ne $true) {
        throw 'The saved Capture Widget preference was lost or changed.'
    }
}

function Enable-ScratchWidget {
    param($Settings)
    if ($null -eq $Settings -or $Settings -isnot [pscustomobject]) {
        throw 'Scratch Jiffshot settings are not a JSON object.'
    }
    if ($null -eq $Settings.PSObject.Properties['capture']) {
        $Settings | Add-Member -NotePropertyName capture -NotePropertyValue ([pscustomobject] @{})
    }
    if ($Settings.capture -isnot [pscustomobject]) { throw 'The scratch capture settings are not an object.' }
    $Settings.capture | Add-Member -NotePropertyName show_widget -NotePropertyValue $true -Force
    return $Settings
}

function Enable-ScratchBackgroundFixture {
    param($Settings)
    $settings = Enable-ScratchWidget $Settings
    $settings.capture | Add-Member -NotePropertyName capture_hotkey -NotePropertyValue $script:UnsupportedHotkey -Force
    return $settings
}

function Restore-ScratchHotkey {
    param($Settings, [bool] $WasPresent, $Value)
    if ($Settings.capture -isnot [pscustomobject]) { throw 'The scratch capture settings are not an object.' }
    if ($WasPresent) {
        $Settings.capture | Add-Member -NotePropertyName capture_hotkey -NotePropertyValue $Value -Force
    } else {
        $Settings.capture.PSObject.Properties.Remove('capture_hotkey')
    }
    return $Settings
}

function Set-ScratchStartupRecord {
    param($Settings, $Record)
    if ($null -eq $Settings -or $Settings -isnot [pscustomobject]) {
        throw 'Scratch Jiffshot settings are not a JSON object.'
    }
    $Settings | Add-Member -NotePropertyName windows_startup -NotePropertyValue $Record -Force
    return $Settings
}

function Write-ScratchSettings {
    param([string] $Path, $Settings)
    [IO.File]::WriteAllText($Path, ($Settings | ConvertTo-Json -Depth 100), [Text.UTF8Encoding]::new($false))
}

function Set-ScratchBackgroundFixture {
    param([string] $Path)
    $settings = Enable-ScratchBackgroundFixture (Read-ScratchSettings $Path)
    Write-ScratchSettings $Path $settings
}

function Assert-ExpectedRunValues {
    param([object[]] $Entries, [string] $BinaryPath)
    $present = @($Entries | Where-Object { $_.Present })
    foreach ($entry in $present) {
        if (-not (Test-ExpectedStartupCommand $entry $BinaryPath)) {
            throw "Unexpected Jiffshot Run value in $($entry.View); it will not be overwritten or removed."
        }
    }
    return $present.Count -ne 0
}

function Invoke-WindowsStartupE2E {
    param(
        [string] $Binary, [bool] $DisposableProfile, [string] $ProfileSid, [string] $ProfileState,
        [bool] $VerifyOptOutInteractively, [bool] $VerifyWindowsDisableInteractively,
        [bool] $VerifyUserNoticeInteractively, [int] $TimeoutSeconds
    )

    # Do not even resolve a binary or inspect the registry before explicit consent.
    if (-not $DisposableProfile) {
        Assert-DisposableConsent $false '' '' $false $false
    }
    $isWindows = [Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT
    if (-not $isWindows) { throw 'This harness requires Windows.' }
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    Assert-DisposableConsent $true $ProfileSid $sid $isWindows ([Environment]::UserInteractive)
    if ([string]::IsNullOrWhiteSpace($Binary)) { throw 'Pass -Binary with the portable/MSI jiffshot.exe.' }
    $item = Get-Item -LiteralPath $Binary
    if ($item -isnot [IO.FileInfo] -or $item.Name -ine 'jiffshot.exe') {
        throw 'Binary must be a local portable/MSI file named jiffshot.exe.'
    }
    $binaryPath = [IO.Path]::GetFullPath($item.FullName)
    $driveType = [IO.DriveType]::Unknown
    if ($binaryPath -match '^[A-Za-z]:\\') {
        $driveType = ([IO.DriveInfo]::new([IO.Path]::GetPathRoot($binaryPath))).DriveType
    }
    Assert-PortableBinaryLocation $binaryPath $driveType

    Initialize-StartupNative
    if (-not [JiffshotStartupE2E.Native]::InteractiveShell()) {
        throw 'An unlocked interactive desktop with the Explorer taskbar is required.'
    }
    $newMutex = $false
    # HKCU is shared across this user's sessions, so serialize the harness
    # across sessions as well as within the current desktop.
    $testMutex = [Threading.Mutex]::new($true, (Get-StartupHarnessMutexName $sid), [ref] $newMutex)
    if (-not $newMutex) {
        $testMutex.Dispose()
        throw 'Another startup harness is already using this Windows user profile.'
    }

    $root = $null
    $context = $null
    $baselineClean = $false
    $ownedValues = @()
    $failure = $null
    $cleanupState = [pscustomobject] @{ Failed = $false }
    $passed = $false
    try {
        Assert-NoExistingInstance
        Assert-CleanStartupState (@(Get-RunValues) + @(Get-ApprovalValues))
        $baselineClean = $true
        $root = Join-Path ([IO.Path]::GetTempPath()) "jiffshot-startup-e2e-$([guid]::NewGuid().ToString('N'))"
        $null = New-Item -ItemType Directory -Path $root
        $context = [pscustomobject] @{
            Root = $root
            Config = (Join-Path $root 'config')
            Local = (Join-Path $root 'local')
            Library = (Join-Path $root 'library')
            Processes = [Collections.Generic.List[object]]::new()
        }
        foreach ($directory in @($context.Config, $context.Local, $context.Library)) {
            $null = New-Item -ItemType Directory -Path $directory
        }
        $settingsPath = Join-Path $context.Config 'jiffshot.json'
        if ($ProfileState -eq 'Existing') {
            [IO.File]::WriteAllText($settingsPath,
                '{"theme":"Light","capture":{"show_widget":true},"startup_e2e_legacy":"preserve"}',
                [Text.UTF8Encoding]::new($false))
        }
        $session = [JiffshotStartupE2E.Native]::Session([uint32] $PID)
        $normalCommand = '"' + $binaryPath + '"'

        # Recheck immediately before the first normal GUI launch.
        Assert-NoExistingInstance
        Assert-CleanStartupState (@(Get-RunValues) + @(Get-ApprovalValues))
        $initial = Start-OwnedCommand $normalCommand $context
        Wait-OwnedTray $initial $TimeoutSeconds
        Assert-OwnedInstance $initial $context.Processes.ToArray() $binaryPath $session
        Wait-Until {
            Assert-OwnedInstance $initial $context.Processes.ToArray() $binaryPath $session
            Assert-ExpectedRunValues @(Get-RunValues) $binaryPath
        } 'the default Run value' $TimeoutSeconds $initial
        $ownedValues = @((Get-RunValues) | Where-Object { $_.Present })
        if (-not (Assert-ExpectedRunValues $ownedValues $binaryPath)) { throw 'The Run value disappeared.' }
        Write-Host "First $ProfileState GUI launch created the expected Run command."
        $initial.Stop()
        Wait-EmptySession $TimeoutSeconds

        if ($ProfileState -eq 'Existing') {
            Assert-SavedWidget $settingsPath
            if ((Read-ScratchSettings $settingsPath).startup_e2e_legacy -cne 'preserve') {
                throw 'The existing settings fixture was not preserved.'
            }
        }
        $settings = Read-ScratchSettings $settingsPath
        $savedStartup = $settings.windows_startup
        if ($savedStartup -isnot [pscustomobject] -or $savedStartup.backend -cne 'desktop' -or
            $savedStartup.requested -ne $true) {
            throw 'The first launch did not persist the enabled desktop startup marker.'
        }
        $savedHotkeyProperty = if ($settings.capture -is [pscustomobject]) {
            $settings.capture.PSObject.Properties['capture_hotkey']
        } else { $null }
        $hadSavedHotkey = $null -ne $savedHotkeyProperty
        $savedHotkey = if ($hadSavedHotkey) { $savedHotkeyProperty.Value } else { $null }
        Set-ScratchBackgroundFixture $settingsPath
        Assert-SavedWidget $settingsPath

        $currentValues = @(Get-RunValues)
        if (-not (Assert-ExpectedRunValues $currentValues $binaryPath)) { throw 'The owned Run value disappeared.' }
        Assert-RecordedRunValues $ownedValues $currentValues
        $runCommand = @($currentValues | Where-Object { $_.Present })[0].Value
        Assert-NoExistingInstance
        $observer = [JiffshotStartupE2E.Native]::WatchWindows()
        try {
            $resident = Start-OwnedCommand $runCommand $context
            Wait-OwnedTray $resident $TimeoutSeconds $observer
            $logPath = Join-Path $context.Local 'capture.log'
            $expectedWarning = "Jiffshot hotkeys: $($script:UnsupportedHotkey) is not a supported capture hotkey"
            Wait-Until {
                Assert-OwnedInstance $resident $context.Processes.ToArray() $binaryPath $session
                Assert-BackgroundWindows $resident $observer
                (Test-Path -LiteralPath $logPath) -and
                    ([string] (Get-Content -LiteralPath $logPath -Raw)).Contains($expectedWarning)
            } 'the automatic hotkey warning to be generated' $TimeoutSeconds $resident
            Observe-Hidden $resident $observer $context.Processes.ToArray() $binaryPath $session
        } finally {
            $observer.Dispose()
        }
        Assert-SavedWidget $settingsPath
        Write-Host 'The real Run command stayed resident with every app window and automatic notice hidden.'

        if ($VerifyUserNoticeInteractively) {
            Assert-OwnedInstance $resident $context.Processes.ToArray() $binaryPath $session
            Assert-RecordedRunValues $ownedValues @(Get-RunValues)
            Write-Host "Put text, not an image, on this disposable profile's clipboard."
            Write-Host "Within $TimeoutSeconds seconds, use the tray's 'From the clipboard' action without opening Jiffshot."
            Wait-VisibleTitle $resident 'Jiffshot: Capture from clipboard' $TimeoutSeconds
            Wait-Until {
                Assert-OwnedInstance $resident $context.Processes.ToArray() $binaryPath $session
                @([JiffshotStartupE2E.Native]::Windows($resident.Id) | Where-Object {
                    $_.Visible -and -not $_.Cloaked
                }).Count -eq 0
            } 'the user-initiated notification to dismiss without opening another window' $TimeoutSeconds $resident
            Write-Host 'A user-initiated notification was visible while Jiffshot remained otherwise hidden.'
        } else {
            Write-Host 'NOT RUN: interactive user-initiated notification check (see the manual acceptance steps).'
        }

        Assert-OwnedInstance $resident $context.Processes.ToArray() $binaryPath $session
        Assert-RecordedRunValues $ownedValues @(Get-RunValues)
        $observer = [JiffshotStartupE2E.Native]::WatchWindows()
        try {
            $duplicate = Start-OwnedCommand $runCommand $context
            Wait-OwnedExit $duplicate $TimeoutSeconds 'duplicate --background launch' $observer
            $observer.Flush()
            Assert-BackgroundWindows $duplicate $observer
            Observe-Hidden $resident $observer $context.Processes.ToArray() $binaryPath $session
        } finally {
            $observer.Dispose()
        }
        Write-Host 'A duplicate background launch exited without showing or foregrounding the owned instance.'

        Assert-OwnedInstance $resident $context.Processes.ToArray() $binaryPath $session
        Assert-RecordedRunValues $ownedValues @(Get-RunValues)
        $open = Start-OwnedCommand ($normalCommand + ' --window') $context
        Wait-OwnedExit $open $TimeoutSeconds '--window launch'
        Wait-VisibleTitle $resident 'Jiffshot: Jiffshot startup' $TimeoutSeconds
        Wait-VisibleTitle $resident 'Jiffshot Capture' $TimeoutSeconds
        Wait-VisibleTitle $resident 'Jiffshot Capture Widget' $TimeoutSeconds
        Assert-SavedWidget $settingsPath
        Write-Host '--window opened Capture, restored the saved widget, and displayed the deferred startup notice.'

        # A malformed scratch startup marker produces a different automatic
        # notice. It must also stay hidden until the user opens Jiffshot.
        $resident.Stop()
        Wait-EmptySession $TimeoutSeconds
        Assert-NoExistingInstance
        Assert-RecordedRunValues $ownedValues @(Get-RunValues)
        $settings = Restore-ScratchHotkey (Read-ScratchSettings $settingsPath) $hadSavedHotkey $savedHotkey
        $settings = Set-ScratchStartupRecord $settings ([pscustomobject] @{
            backend = $script:InvalidStartupBackend; requested = $true
        })
        Write-ScratchSettings $settingsPath $settings
        $observer = [JiffshotStartupE2E.Native]::WatchWindows()
        try {
            $resident = Start-OwnedCommand $runCommand $context
            Wait-OwnedTray $resident $TimeoutSeconds $observer
            Wait-Until {
                Assert-OwnedInstance $resident $context.Processes.ToArray() $binaryPath $session
                Assert-BackgroundWindows $resident $observer
                $text = if (Test-Path -LiteralPath $logPath) {
                    [string] (Get-Content -LiteralPath $logPath -Raw)
                } else { '' }
                $text.Contains('Could not initialize Windows startup: Invalid startup settings:') -and
                    $text.Contains($script:InvalidStartupBackend)
            } 'the automatic startup warning to be generated' $TimeoutSeconds $resident
            Observe-Hidden $resident $observer $context.Processes.ToArray() $binaryPath $session
        } finally {
            $observer.Dispose()
        }
        Assert-OwnedInstance $resident $context.Processes.ToArray() $binaryPath $session
        Assert-RecordedRunValues $ownedValues @(Get-RunValues)
        $open = Start-OwnedCommand ($normalCommand + ' --window') $context
        Wait-OwnedExit $open $TimeoutSeconds '--window launch for startup warning'
        Wait-VisibleTitle $resident 'Jiffshot: Jiffshot startup' $TimeoutSeconds
        Wait-VisibleTitle $resident 'Jiffshot Capture' $TimeoutSeconds
        Wait-VisibleTitle $resident 'Jiffshot Capture Widget' $TimeoutSeconds
        Write-Host 'The automatic startup warning stayed hidden until Jiffshot was opened.'
        $resident.Stop()
        Wait-EmptySession $TimeoutSeconds
        $settings = Set-ScratchStartupRecord (Read-ScratchSettings $settingsPath) $savedStartup
        Write-ScratchSettings $settingsPath $settings

        # A normal no-argument launch is a separate user action from --window.
        # Restart hidden so a no-op cannot pass by observing an already-open window.
        Assert-NoExistingInstance
        Assert-RecordedRunValues $ownedValues @(Get-RunValues)
        $observer = [JiffshotStartupE2E.Native]::WatchWindows()
        try {
            $resident = Start-OwnedCommand $runCommand $context
            Wait-OwnedTray $resident $TimeoutSeconds $observer
            Observe-Hidden $resident $observer $context.Processes.ToArray() $binaryPath $session
        } finally {
            $observer.Dispose()
        }
        Assert-OwnedInstance $resident $context.Processes.ToArray() $binaryPath $session
        Assert-RecordedRunValues $ownedValues @(Get-RunValues)
        $open = Start-OwnedCommand $normalCommand $context
        Wait-OwnedExit $open $TimeoutSeconds 'ordinary no-argument launch'
        Wait-VisibleTitle $resident 'Jiffshot Capture' $TimeoutSeconds
        Wait-VisibleTitle $resident 'Jiffshot Capture Widget' $TimeoutSeconds
        Assert-SavedWidget $settingsPath
        Write-Host 'An ordinary no-argument launch opened Jiffshot and restored the saved widget.'

        # File handoff uses a separate per-user/session pipe. Verify its server
        # PID without sending data, then wait for the pipe to accept the file.
        Assert-OwnedInstance $resident $context.Processes.ToArray() $binaryPath $session
        Wait-Until {
            $owner = [JiffshotStartupE2E.Native]::EditorPipeOwner($sid, $session)
            if ($owner -ne 0 -and $owner -ne $resident.Id) {
                throw "The editor pipe belongs to unowned PID $owner."
            }
            $owner -eq $resident.Id
        } 'the owned editor pipe' $TimeoutSeconds $resident
        Wait-Until { [JiffshotStartupE2E.Native]::EditorPipeAvailable($sid, $session) } `
            'the editor pipe to accept another connection' $TimeoutSeconds $resident
        $fixture = Join-Path $root 'startup fixture.png'
        [IO.File]::WriteAllBytes($fixture, [Convert]::FromBase64String(
            'iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAYAAABytg0kAAAAEUlEQVR4nGP4z8DwH4QZYAwAR8oH+WdZbrcAAAAASUVORK5CYII='))
        Assert-OwnedInstance $resident $context.Processes.ToArray() $binaryPath $session
        Assert-RecordedRunValues $ownedValues @(Get-RunValues)
        $file = Start-OwnedCommand ($normalCommand + ' "' + $fixture + '"') $context
        Wait-OwnedExit $file $TimeoutSeconds 'file-open launch'
        Wait-VisibleTitle $resident 'startup fixture.png' $TimeoutSeconds -Contains
        Write-Host 'A file launch reached the owned editor.'

        Assert-OwnedInstance $resident $context.Processes.ToArray() $binaryPath $session
        Assert-RecordedRunValues $ownedValues @(Get-RunValues)
        $capture = Start-OwnedCommand ($normalCommand + ' --capture') $context
        Wait-OwnedExit $capture $TimeoutSeconds '--capture launch'
        Wait-VisibleTitle $resident 'Jiffshot selection' $TimeoutSeconds
        Write-Host '--capture opened the owned selection overlay.'
        $resident.Stop()
        Wait-EmptySession $TimeoutSeconds

        if ($VerifyWindowsDisableInteractively) {
            Assert-NoExistingInstance
            Assert-RecordedRunValues $ownedValues @(Get-RunValues)
            $probe = Start-OwnedCommand $normalCommand $context
            Wait-OwnedTray $probe $TimeoutSeconds
            Assert-OwnedInstance $probe $context.Processes.ToArray() $binaryPath $session
            Write-Host 'In Windows Settings/Task Manager, disable Jiffshot startup (not the in-app switch).'
            if ((Read-Host 'Verify Windows shows Disabled, then type DISABLED') -cne 'DISABLED') {
                throw 'The Windows-disable check was not confirmed.'
            }
            $approval = @(Get-ApprovalValues)
            if (@($approval | Where-Object { $_.Present -and $_.Kind -ceq 'Binary' }).Count -eq 0) {
                throw 'No binary StartupApproved record was observable; verify this Windows state manually.'
            }
            $before = @(Get-ApprovalSignature $approval)
            if (-not (Assert-ExpectedRunValues @(Get-RunValues) $binaryPath)) {
                throw 'The Run value disappeared during the Windows-disable check.'
            }
            Assert-RecordedRunValues $ownedValues @(Get-RunValues)
            $probe.Stop()
            Wait-EmptySession $TimeoutSeconds
            Assert-NoExistingInstance
            Assert-RecordedRunValues $ownedValues @(Get-RunValues)
            $probe = Start-OwnedCommand $normalCommand $context
            Wait-OwnedTray $probe $TimeoutSeconds
            Assert-OwnedInstance $probe $context.Processes.ToArray() $binaryPath $session
            $after = @(Get-ApprovalSignature @(Get-ApprovalValues))
            if (($before -join "`n") -cne ($after -join "`n")) {
                throw 'A normal launch changed the Windows StartupApproved record.'
            }
            if (-not (Assert-ExpectedRunValues @(Get-RunValues) $binaryPath)) {
                throw 'The Run value disappeared after the Windows-disable relaunch.'
            }
            Assert-RecordedRunValues $ownedValues @(Get-RunValues)
            $probe.Stop()
            Wait-EmptySession $TimeoutSeconds
            Write-Host 'The operator-confirmed Windows disable record survived a normal relaunch.'
            Write-Host 'StartupApproved is intentionally not removed by this script; discard the profile.'
            if ($VerifyOptOutInteractively) {
                if ((Read-Host 'Re-enable Jiffshot in Windows, then type ENABLED for the in-app opt-out check') -cne 'ENABLED') {
                    throw 'Windows startup was not confirmed re-enabled for the opt-out check.'
                }
            }
        } else {
            Write-Host 'NOT RUN: Windows-controlled disable/policy verification (see the manual acceptance steps).'
        }

        if ($VerifyOptOutInteractively) {
            Assert-NoExistingInstance
            Assert-RecordedRunValues $ownedValues @(Get-RunValues)
            $probe = Start-OwnedCommand $normalCommand $context
            Wait-OwnedTray $probe $TimeoutSeconds
            Assert-OwnedInstance $probe $context.Processes.ToArray() $binaryPath $session
            $open = Start-OwnedCommand ($normalCommand + ' --window') $context
            Wait-OwnedExit $open $TimeoutSeconds '--window launch for opt-out'
            Wait-VisibleTitle $probe 'Jiffshot Capture' $TimeoutSeconds
            Write-Host 'Use Jiffshot Preferences or its tray menu to turn Run when Windows starts OFF.'
            if ((Read-Host 'After opting out in Jiffshot, type OPTED-OUT') -cne 'OPTED-OUT') {
                throw 'The in-app opt-out was not confirmed.'
            }
            Wait-Until { @((Get-RunValues) | Where-Object { $_.Present }).Count -eq 0 } `
                'the in-app opt-out to remove the Run value' $TimeoutSeconds $probe
            $probe.Stop()
            Wait-EmptySession $TimeoutSeconds
            Assert-NoExistingInstance
            Assert-CleanStartupState @(Get-RunValues)
            $probe = Start-OwnedCommand $normalCommand $context
            Wait-OwnedTray $probe $TimeoutSeconds
            $clock = [Diagnostics.Stopwatch]::StartNew()
            do {
                Assert-OwnedInstance $probe $context.Processes.ToArray() $binaryPath $session
                if (@((Get-RunValues) | Where-Object { $_.Present }).Count -ne 0) {
                    throw 'A normal relaunch recreated the Run value after the in-app opt-out.'
                }
                Start-Sleep -Milliseconds 100
            } while ($clock.Elapsed.TotalSeconds -lt 4)
            $probe.Stop()
            Wait-EmptySession $TimeoutSeconds
            Write-Host 'The in-app opt-out remained off after a normal relaunch.'
        } else {
            Write-Host 'NOT RUN: interactive in-app opt-out persistence (see the manual acceptance steps).'
        }
        $passed = $true
    } catch {
        $failure = $_
    } finally {
        Invoke-StartupHarnessCleanup $testMutex {
            $allStopped = $true
            if ($null -ne $context) {
                for ($i = $context.Processes.Count - 1; $i -ge 0; $i--) {
                    try {
                        $context.Processes[$i].Stop()
                    } catch {
                        $allStopped = $false
                        $cleanupState.Failed = $true
                        Write-Warning "Could not stop test-owned PID $($context.Processes[$i].Id): $_"
                    }
                }
            }
            if ($baselineClean -and $null -ne $context -and $context.Processes.Count -gt 0) {
                try {
                    if (-not $allStopped -or @(Get-JiffshotProcessIds).Count -ne 0 -or
                        [JiffshotStartupE2E.Native]::TrayOwner() -ne 0 -or
                        [JiffshotStartupE2E.Native]::InstanceMutexExists()) {
                        $cleanupState.Failed = $true
                        Write-Warning 'Skipped registry cleanup because an instance is still present or ownership is uncertain.'
                    } elseif (-not (Remove-OwnedRunValues $baselineClean $ownedValues)) {
                        $cleanupState.Failed = $true
                    }
                } catch {
                    $cleanupState.Failed = $true
                    Write-Warning "Registry cleanup was not safe or did not complete: $_"
                }
            }
            if ($null -ne $context) {
                foreach ($process in $context.Processes) { $process.Dispose() }
            }
            if ($null -ne $root) {
                if ($passed -and -not $cleanupState.Failed) {
                    try {
                        Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction Stop
                    } catch {
                        $cleanupState.Failed = $true
                        Write-Warning "Scratch cleanup failed at '$root': $_"
                    }
                } else {
                    Write-Host "Scratch diagnostics retained in $root"
                }
            }
        }
    }
    if ($null -ne $failure) { throw $failure }
    if ($cleanupState.Failed) { throw 'The checks completed, but safe cleanup was incomplete.' }
    Write-Host 'Automated launch checks passed. Actual Windows logon and tray-icon acceptance remain manual.'
}

if ($MyInvocation.InvocationName -ne '.') {
    $ErrorActionPreference = 'Stop'
    Invoke-WindowsStartupE2E $Binary $DisposableProfile.IsPresent $ProfileSid $ProfileState `
        $VerifyOptOutInteractively.IsPresent $VerifyWindowsDisableInteractively.IsPresent `
        $VerifyUserNoticeInteractively.IsPresent $TimeoutSeconds
}
