[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $Binary,
    [switch] $AllowDesktopInput
)

$ErrorActionPreference = 'Stop'
if (-not $AllowDesktopInput) {
    throw 'This test moves the pointer and replaces the system clipboard. Pass -AllowDesktopInput only on an isolated desktop.'
}
$binaryPath = (Resolve-Path -LiteralPath $Binary).Path
$temp = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
$root = Join-Path $temp "jiffshot-grab-text-$([guid]::NewGuid().ToString('N'))"
foreach ($directory in @($root, "$root\library", "$root\config", "$root\local")) {
    New-Item -ItemType Directory -Path $directory | Out-Null
}

$names = @(
    'JIFFSHOT_UITEST', 'JIFFSHOT_UITEST_GRAB_TEXT', 'JIFFSHOT_UITEST_GRAB_TEXT_FIXTURE',
    'JIFFSHOT_LIBRARY', 'JIFFSHOT_CONFIG_DIR', 'JIFFSHOT_LOCAL_DIR', 'APPDATA', 'LOCALAPPDATA'
)
$names += @(Get-ChildItem Env: | Where-Object { $_.Name -match '^(JIFFSHOT|RUSTOLONIA)_' } |
    ForEach-Object { $_.Name })
$names = @($names | Select-Object -Unique)
$previous = @{}
foreach ($name in $names) {
    $previous[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}

Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Threading;

public static class JiffshotGrabTextInput {
    [StructLayout(LayoutKind.Sequential)]
    public struct Point { public int x, y; }
    [StructLayout(LayoutKind.Sequential)]
    public struct MouseInput {
        public int dx, dy;
        public uint mouseData, flags, time;
        public UIntPtr extraInfo;
    }
    [StructLayout(LayoutKind.Explicit)]
    public struct InputData { [FieldOffset(0)] public MouseInput mouse; }
    [StructLayout(LayoutKind.Sequential)]
    public struct Input { public uint type; public InputData data; }

    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool SetProcessDpiAwarenessContext(IntPtr value);
    [DllImport("user32.dll")]
    private static extern IntPtr GetThreadDpiAwarenessContext();
    [DllImport("user32.dll")]
    private static extern int GetAwarenessFromDpiAwarenessContext(IntPtr value);
    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll", SetLastError = true)]
    private static extern uint SendInput(uint count, Input[] inputs, int size);
    [DllImport("user32.dll")]
    private static extern IntPtr WindowFromPoint(Point point);
    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);

    public static void EnablePerMonitorDpi() {
        if (SetProcessDpiAwarenessContext(new IntPtr(-4))) return;
        int error = Marshal.GetLastWin32Error();
        if (GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()) != 2)
            throw new Win32Exception(error, "Could not enable per-monitor DPI awareness");
    }
    public static uint ProcessAt(int x, int y) {
        IntPtr window = WindowFromPoint(new Point { x = x, y = y });
        if (window == IntPtr.Zero) return 0;
        uint process;
        GetWindowThreadProcessId(window, out process);
        return process;
    }
    private static void Mouse(uint flags) {
        var input = new Input[1];
        input[0].data.mouse.flags = flags;
        if (SendInput(1, input, Marshal.SizeOf(typeof(Input))) != 1) throw new Win32Exception();
    }
    public static void Drag(int left, int top, int right, int bottom) {
        if (!SetCursorPos(left, top)) throw new Win32Exception();
        Mouse(2);
        try {
            Thread.Sleep(150);
            if (!SetCursorPos(right, bottom)) throw new Win32Exception();
            Thread.Sleep(150);
        } finally {
            Mouse(4);
        }
    }
}
'@

$form = $null
$font = $null
$process = $null
$passed = $false
try {
    foreach ($name in $names) {
        if ($name -match '^(JIFFSHOT|RUSTOLONIA)_') {
            [Environment]::SetEnvironmentVariable($name, $null, 'Process')
        }
    }
    [JiffshotGrabTextInput]::EnablePerMonitorDpi()
    $screen = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
    if ($screen.Width -lt 700 -or $screen.Height -lt 360) {
        throw "The desktop is too small for the Grab Text fixture: $screen"
    }
    $form = [System.Windows.Forms.Form]::new()
    $form.FormBorderStyle = [System.Windows.Forms.FormBorderStyle]::None
    $form.StartPosition = [System.Windows.Forms.FormStartPosition]::Manual
    $form.Bounds = [System.Drawing.Rectangle]::new($screen.Left + 40, $screen.Top + 40, 620, 250)
    $form.BackColor = [System.Drawing.Color]::White
    $form.TopMost = $true
    $font = [System.Drawing.Font]::new('Arial', 32, [System.Drawing.FontStyle]::Bold)
    foreach ($fixture in @(
        @{ Text = 'JIFFSHOT FIXTURE'; Top = 10 },
        @{ Text = 'OUTSIDE TEXT'; Top = 135 }
    )) {
        $label = [System.Windows.Forms.Label]::new()
        $label.Text = $fixture.Text
        $label.Font = $font
        $label.AutoSize = $true
        $label.Location = [System.Drawing.Point]::new(15, $fixture.Top)
        $form.Controls.Add($label)
    }
    $form.Show()
    $form.Activate()
    $form.Refresh()
    [System.Windows.Forms.Application]::DoEvents()
    Start-Sleep -Milliseconds 500

    $area = [System.Drawing.Rectangle]::new($form.Left + 5, $form.Top + 5, $form.Width - 10, 110)
    $env:JIFFSHOT_UITEST = $root
    $env:JIFFSHOT_UITEST_GRAB_TEXT = '1'
    $env:JIFFSHOT_UITEST_GRAB_TEXT_FIXTURE = "$($area.X),$($area.Y),$($area.Width),$($area.Height)"
    $env:JIFFSHOT_LIBRARY = "$root\library"
    $env:JIFFSHOT_CONFIG_DIR = "$root\config"
    $env:JIFFSHOT_LOCAL_DIR = "$root\local"
    $env:APPDATA = "$root\config"
    $env:LOCALAPPDATA = "$root\local"
    $process = Start-Process -FilePath $binaryPath -PassThru `
        -RedirectStandardOutput "$root\stdout.txt" -RedirectStandardError "$root\stderr.txt"

    $ready = Join-Path $root 'grab-text-ready'
    $deadline = (Get-Date).AddSeconds(60)
    while (-not (Test-Path -LiteralPath $ready)) {
        [System.Windows.Forms.Application]::DoEvents()
        $process.Refresh()
        if ($process.HasExited) { throw "Jiffshot exited before the overlay opened: $($process.ExitCode)" }
        if ((Get-Date) -ge $deadline) { throw 'The Grab Text overlay did not become ready' }
        Start-Sleep -Milliseconds 50
    }
    $left = $area.Left + 1
    $top = $area.Top + 1
    $right = $area.Right - 2
    $bottom = $area.Bottom - 2
    while ([JiffshotGrabTextInput]::ProcessAt($left, $top) -ne $process.Id -or
           [JiffshotGrabTextInput]::ProcessAt($right, $bottom) -ne $process.Id) {
        [System.Windows.Forms.Application]::DoEvents()
        $process.Refresh()
        if ($process.HasExited) { throw "Jiffshot exited before the overlay became visible: $($process.ExitCode)" }
        if ((Get-Date) -ge $deadline) { throw 'The Grab Text overlay did not receive desktop input' }
        Start-Sleep -Milliseconds 50
    }
    [JiffshotGrabTextInput]::Drag($left, $top, $right, $bottom)
    $deadline = (Get-Date).AddSeconds(60)
    do {
        [System.Windows.Forms.Application]::DoEvents()
        $process.Refresh()
        if (-not $process.HasExited) { Start-Sleep -Milliseconds 50 }
    } while (-not $process.HasExited -and (Get-Date) -lt $deadline)
    if (-not $process.HasExited) { throw 'Jiffshot did not finish the Grab Text self-test' }
    if ($process.ExitCode -ne 0) { throw "Jiffshot exited with code $($process.ExitCode)" }
    $log = Get-Content -LiteralPath (Join-Path $root 'ui-log.txt')
    if ('grab text: both fixture labels were visible to native OCR' -cnotin $log -or
        'grab text: editor undo and redo history remained intact' -cnotin $log -or
        ($log | Where-Object { $_ -like 'FAILED:*' })) {
        throw 'The Grab Text self-test did not complete successfully'
    }
    if (Get-ChildItem -LiteralPath "$root\library" -Force) {
        throw 'Grab Text saved a capture to the scratch library'
    }
    $passed = $true
    Write-Host 'Windows Grab Text passed with a real screen, mouse selection, native OCR, clipboard and editor history'
} finally {
    if ($process) {
        $process.Refresh()
        if (-not $process.HasExited) {
            Stop-Process -Id $process.Id -Force
            $process.WaitForExit()
        }
    }
    if ($form) { $form.Close(); $form.Dispose() }
    if ($font) { $font.Dispose() }
    foreach ($name in $names) {
        [Environment]::SetEnvironmentVariable($name, $previous[$name], 'Process')
    }
    foreach ($file in @('stdout.txt', 'stderr.txt', 'ui-log.txt')) {
        $path = Join-Path $root $file
        if (Test-Path -LiteralPath $path) {
            Write-Host "--- $file ---"
            Get-Content -LiteralPath $path
        }
    }
    if ($passed) {
        Remove-Item -LiteralPath $root -Recurse -Force
    } else {
        Write-Host "Grab Text diagnostics retained in $root"
    }
}
