Set-StrictMode -Version Latest

if ($IsWindows -and -not ('JiffshotMsiDatabase' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

public sealed class JiffshotMsiDatabase : IDisposable
{
    public const int OpenReadOnly = 0;
    public const int OpenTransact = 1;
    public const int OpenCreate = 3;

    private const uint ErrorNoMoreItems = 259;
    private const uint ErrorMoreData = 234;
    private const uint PidTemplate = 7;
    private const uint PidPageCount = 14;
    private const ushort VtI4 = 3;
    private const ushort VtLpstr = 30;

    private uint _db;

    [DllImport("msi.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
    private static extern uint MsiOpenDatabaseW(string szDatabasePath, IntPtr szPersist, out uint phDatabase);

    [DllImport("msi.dll", ExactSpelling = true)]
    private static extern uint MsiDatabaseCommit(uint hDatabase);

    [DllImport("msi.dll", ExactSpelling = true)]
    private static extern uint MsiCloseHandle(uint hAny);

    [DllImport("msi.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
    private static extern uint MsiDatabaseOpenViewW(uint hDatabase, string szQuery, out uint phView);

    [DllImport("msi.dll", ExactSpelling = true)]
    private static extern uint MsiViewExecute(uint hView, uint hRecord);

    [DllImport("msi.dll", ExactSpelling = true)]
    private static extern uint MsiViewFetch(uint hView, out uint phRecord);

    [DllImport("msi.dll", ExactSpelling = true)]
    private static extern uint MsiViewClose(uint hView);

    [DllImport("msi.dll", ExactSpelling = true)]
    private static extern uint MsiCreateRecord(uint cParams);

    [DllImport("msi.dll", ExactSpelling = true)]
    private static extern uint MsiRecordGetFieldCount(uint hRecord);

    [DllImport("msi.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
    private static extern uint MsiRecordGetStringW(uint hRecord, uint iField, StringBuilder szValueBuf, ref uint pcchValueBuf);

    [DllImport("msi.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
    private static extern uint MsiRecordSetStringW(uint hRecord, uint iField, string szValue);

    [DllImport("msi.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
    private static extern uint MsiGetSummaryInformationW(uint hDatabase, string szDatabasePath, uint uiUpdateCount, out uint phSummaryInfo);

    [DllImport("msi.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
    private static extern uint MsiSummaryInfoGetPropertyW(
        uint hSummaryInfo,
        uint uiProperty,
        out uint puiDataType,
        out int piValue,
        IntPtr pftValue,
        StringBuilder szValueBuf,
        ref uint pcchValueBuf);

    [DllImport("msi.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
    private static extern uint MsiSummaryInfoSetPropertyW(
        uint hSummaryInfo,
        uint uiProperty,
        uint uiDataType,
        int iValue,
        IntPtr pftValue,
        string szValue);

    [DllImport("msi.dll", ExactSpelling = true)]
    private static extern uint MsiSummaryInfoPersist(uint hSummaryInfo);

    public JiffshotMsiDatabase(string path, int mode)
    {
        Check(MsiOpenDatabaseW(path, (IntPtr)mode, out _db), "MsiOpenDatabaseW");
    }

    public (string Template, int PageCount) ReadSummary()
    {
        Check(MsiGetSummaryInformationW(_db, null, 0, out var summary), "MsiGetSummaryInformationW");
        try
        {
            uint capacity = 256;
            var buffer = new StringBuilder((int)capacity);
            uint rc = MsiSummaryInfoGetPropertyW(summary, PidTemplate, out _, out _, IntPtr.Zero, buffer, ref capacity);
            if (rc == ErrorMoreData)
            {
                capacity++;
                buffer = new StringBuilder((int)capacity);
                rc = MsiSummaryInfoGetPropertyW(summary, PidTemplate, out _, out _, IntPtr.Zero, buffer, ref capacity);
            }
            Check(rc, "MsiSummaryInfoGetPropertyW(PID_TEMPLATE)");

            uint emptyCap = 0;
            Check(MsiSummaryInfoGetPropertyW(summary, PidPageCount, out _, out int pageCount, IntPtr.Zero, null, ref emptyCap), "MsiSummaryInfoGetPropertyW(PID_PAGECOUNT)");
            return (buffer.ToString(), pageCount);
        }
        finally
        {
            MsiCloseHandle(summary);
        }
    }

    public void WriteSummary(string template, int pageCount)
    {
        Check(MsiGetSummaryInformationW(_db, null, 20, out var summary), "MsiGetSummaryInformationW");
        try
        {
            Check(MsiSummaryInfoSetPropertyW(summary, PidTemplate, VtLpstr, 0, IntPtr.Zero, template), "MsiSummaryInfoSetPropertyW(PID_TEMPLATE)");
            Check(MsiSummaryInfoSetPropertyW(summary, PidPageCount, VtI4, pageCount, IntPtr.Zero, null), "MsiSummaryInfoSetPropertyW(PID_PAGECOUNT)");
            Check(MsiSummaryInfoPersist(summary), "MsiSummaryInfoPersist");
        }
        finally
        {
            MsiCloseHandle(summary);
        }
    }

    public string[][] QueryRows(string sql)
    {
        Check(MsiDatabaseOpenViewW(_db, sql, out var view), sql);
        try
        {
            Check(MsiViewExecute(view, 0), sql);
            var rows = new List<string[]>();
            while (true)
            {
                uint fetch = MsiViewFetch(view, out var record);
                if (fetch == ErrorNoMoreItems)
                    break;
                Check(fetch, sql);
                try
                {
                    uint count = MsiRecordGetFieldCount(record);
                    var values = new string[count];
                    for (uint i = 1; i <= count; i++)
                    {
                        uint capacity = 256;
                        var buffer = new StringBuilder((int)capacity);
                        uint rc = MsiRecordGetStringW(record, i, buffer, ref capacity);
                        if (rc == ErrorMoreData)
                        {
                            capacity++;
                            buffer = new StringBuilder((int)capacity);
                            rc = MsiRecordGetStringW(record, i, buffer, ref capacity);
                        }
                        Check(rc, "MsiRecordGetStringW");
                        values[i - 1] = buffer.ToString();
                    }
                    rows.Add(values);
                }
                finally
                {
                    MsiCloseHandle(record);
                }
            }
            MsiViewClose(view);
            return rows.ToArray();
        }
        finally
        {
            MsiCloseHandle(view);
        }
    }

    public void Execute(string sql) => Execute(sql, Array.Empty<string>());

    public void Execute(string sql, string[] parameters)
    {
        Check(MsiDatabaseOpenViewW(_db, sql, out var view), sql);
        try
        {
            uint record = 0;
            try
            {
                if (parameters != null && parameters.Length > 0)
                {
                    record = MsiCreateRecord((uint)parameters.Length);
                    for (int i = 0; i < parameters.Length; i++)
                        Check(MsiRecordSetStringW(record, (uint)(i + 1), parameters[i]), "MsiRecordSetStringW");
                }
                Check(MsiViewExecute(view, record), sql);
                MsiViewClose(view);
            }
            finally
            {
                if (record != 0)
                    MsiCloseHandle(record);
            }
        }
        finally
        {
            MsiCloseHandle(view);
        }
    }

    public void Commit() => Check(MsiDatabaseCommit(_db), "MsiDatabaseCommit");

    public void Dispose()
    {
        if (_db != 0)
        {
            MsiCloseHandle(_db);
            _db = 0;
        }
    }

    private static void Check(uint rc, string operation)
    {
        if (rc != 0)
            throw new Win32Exception((int)rc, $"{operation} failed with Win32/MSI error {rc}");
    }
}
'@
}

function Get-MsiPlatformExpectation {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [ValidateSet('x64', 'arm64')]
        [string]$Platform
    )

    if ($Platform -ceq 'arm64') {
        return [pscustomobject]@{
            Platform         = 'arm64'
            TemplatePrefix   = 'Arm64;'
            NativeArch       = 'ARM64'
            Condition        = 'Installed OR NATIVE_ARCHITECTURE ~= "ARM64"'
            Message          = 'This installer requires Windows on ARM64. Download the x64 MSI for an Intel or AMD PC.'
        }
    }

    return [pscustomobject]@{
        Platform         = 'x64'
        TemplatePrefix   = 'x64;'
        NativeArch       = 'AMD64'
        Condition        = 'Installed OR NATIVE_ARCHITECTURE ~= "AMD64"'
        Message          = 'This installer requires Windows x64. Download the ARM64 MSI for a Windows on ARM device.'
    }
}

function Confirm-MsiPlatformCheck {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$MsiPath,
        [Parameter(Mandatory)]
        [ValidateSet('x64', 'arm64')]
        [string]$Platform
    )

    $resolvedPath = (Resolve-Path -LiteralPath $MsiPath).Path
    $expected = Get-MsiPlatformExpectation -Platform $Platform
    $database = [JiffshotMsiDatabase]::new($resolvedPath, [JiffshotMsiDatabase]::OpenReadOnly)
    try {
        function Test-MsiTable([string]$TableName) {
            $rows = $database.QueryRows("SELECT ``Name`` FROM ``_Tables`` WHERE ``Name`` = '$TableName'")
            return $rows.Length -eq 1
        }

        $summary = $database.ReadSummary()
        $template = [string]$summary.Item1
        $pageCount = [int]$summary.Item2
        if (-not $template.StartsWith($expected.TemplatePrefix, [StringComparison]::Ordinal)) {
            throw "MSI '$resolvedPath' SummaryInformation Template '$template' does not start with '$($expected.TemplatePrefix)'."
        }
        if ($pageCount -lt 500) {
            throw "MSI '$resolvedPath' SummaryInformation PageCount '$pageCount' must be at least 500."
        }

        $launchRows = $database.QueryRows("SELECT ``Condition``, ``Description`` FROM ``LaunchCondition``")
        foreach ($row in $launchRows) {
            if ($row[0] -cmatch '\bArm64\b') {
                throw "MSI '$resolvedPath' LaunchCondition '$($row[0])' references nonexistent Windows Installer property 'Arm64'."
            }
        }
        $matchingLaunch = @($launchRows | Where-Object { $_[0] -ceq $expected.Condition })
        if ($matchingLaunch.Count -ne 1 -or $matchingLaunch[0][1] -cne $expected.Message) {
            throw "MSI '$resolvedPath' ($Platform) is missing expected LaunchCondition '$($expected.Condition)' -> '$($expected.Message)'."
        }

        if (-not (Test-MsiTable 'AppSearch') -or -not (Test-MsiTable 'RegLocator')) {
            throw "MSI '$resolvedPath' is missing the AppSearch or RegLocator table required for native architecture detection."
        }

        $appSearchRows = $database.QueryRows("SELECT ``Property``, ``Signature_`` FROM ``AppSearch`` WHERE ``Property`` = 'NATIVE_ARCHITECTURE'")
        if ($appSearchRows.Length -ne 1 -or $appSearchRows[0][1] -cne 'NativeArchitecture') {
            throw "MSI '$resolvedPath' must map AppSearch property 'NATIVE_ARCHITECTURE' to signature 'NativeArchitecture'."
        }

        foreach ($disallowed in @(
            @{ Table = 'Signature'; Column = 'Signature' },
            @{ Table = 'CompLocator'; Column = 'Signature_' }
        )) {
            if (Test-MsiTable $disallowed.Table) {
                $badRows = $database.QueryRows("SELECT ``$($disallowed.Column)`` FROM ``$($disallowed.Table)`` WHERE ``$($disallowed.Column)`` = 'NativeArchitecture'")
                if ($badRows.Length -ne 0) {
                    throw "MSI '$resolvedPath' must not list 'NativeArchitecture' in $($disallowed.Table)."
                }
            }
        }

        # Root = 2 (HKLM), Type = 18 (msidbLocatorTypeRawValue [2] | msidbLocatorType64bit [16]).
        $regRows = $database.QueryRows("SELECT ``Root``, ``Key``, ``Name``, ``Type`` FROM ``RegLocator`` WHERE ``Signature_`` = 'NativeArchitecture'")
        if ($regRows.Length -ne 1 -or
            $regRows[0][0] -cne '2' -or
            $regRows[0][1] -cne 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment' -or
            $regRows[0][2] -cne 'PROCESSOR_ARCHITECTURE' -or
            $regRows[0][3] -cne '18') {
            throw "MSI '$resolvedPath' has an invalid RegLocator row for 'NativeArchitecture'."
        }

        $presetNativeArch = $database.QueryRows("SELECT ``Value`` FROM ``Property`` WHERE ``Property`` = 'NATIVE_ARCHITECTURE'")
        if ($presetNativeArch.Length -ne 0) {
            throw "MSI '$resolvedPath' must not pre-populate 'NATIVE_ARCHITECTURE' in the Property table."
        }

        $secureRows = $database.QueryRows("SELECT ``Value`` FROM ``Property`` WHERE ``Property`` = 'SecureCustomProperties'")
        $secureProps = if ($secureRows.Length -eq 1) { @($secureRows[0][0] -split ';') } else { @() }
        if ('NATIVE_ARCHITECTURE' -cnotin $secureProps) {
            throw "MSI '$resolvedPath' must include 'NATIVE_ARCHITECTURE' in SecureCustomProperties so UAC elevation preserves AppSearch results."
        }

        foreach ($sequenceTable in @('InstallUISequence', 'InstallExecuteSequence')) {
            $appSearchSeq = $database.QueryRows("SELECT ``Condition``, ``Sequence`` FROM ``$sequenceTable`` WHERE ``Action`` = 'AppSearch'")
            $launchSeq = $database.QueryRows("SELECT ``Condition``, ``Sequence`` FROM ``$sequenceTable`` WHERE ``Action`` = 'LaunchConditions'")
            if ($appSearchSeq.Length -ne 1 -or $launchSeq.Length -ne 1) {
                throw "MSI '$resolvedPath' must schedule both AppSearch and LaunchConditions in $sequenceTable."
            }
            if (-not [string]::IsNullOrEmpty($appSearchSeq[0][0]) -or -not [string]::IsNullOrEmpty($launchSeq[0][0])) {
                throw "MSI '$resolvedPath' must run AppSearch and LaunchConditions unconditionally in $sequenceTable."
            }
            if ([int]$appSearchSeq[0][1] -ge [int]$launchSeq[0][1]) {
                throw "MSI '$resolvedPath' schedules AppSearch ($($appSearchSeq[0][1])) at or after LaunchConditions ($($launchSeq[0][1])) in $sequenceTable."
            }
        }
    } finally {
        $database.Dispose()
    }
}

function Set-MsiPlatformConditionForTest {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$SourceMsiPath,
        [Parameter(Mandatory)]
        [string]$DestinationMsiPath,
        [Parameter(Mandatory)]
        [ValidateSet('x64', 'arm64')]
        [string]$TargetPlatform
    )

    Copy-Item -LiteralPath $SourceMsiPath -Destination $DestinationMsiPath -Force
    $resolvedPath = (Resolve-Path -LiteralPath $DestinationMsiPath).Path
    $expected = Get-MsiPlatformExpectation -Platform $TargetPlatform
    $database = [JiffshotMsiDatabase]::new($resolvedPath, [JiffshotMsiDatabase]::OpenTransact)
    try {
        foreach ($row in $database.QueryRows("SELECT ``Condition`` FROM ``LaunchCondition``")) {
            if ($row[0].Contains('NATIVE_ARCHITECTURE', [StringComparison]::Ordinal)) {
                $database.Execute("DELETE FROM ``LaunchCondition`` WHERE ``Condition`` = ?", @($row[0]))
            }
        }
        $database.Execute("INSERT INTO ``LaunchCondition`` (``Condition``, ``Description``) VALUES (?, ?)", @($expected.Condition, $expected.Message))
        $database.Commit()
    } finally {
        $database.Dispose()
    }
}
