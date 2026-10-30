// Builds the Jiffshot MSI: a per-machine install of jiffshot.exe and the
// Rustolonia host libraries beside it into
// C:\Program Files\Devolutions\Jiffshot, with a Start menu shortcut and the
// .jsd document association.
//
//   Jiffshot.Package.exe -bin <dir> -platform x64|arm64 [-version X.Y.Z] [-out <dir>]
//
// -bin is the folder holding jiffshot.exe and its DLLs (a Cargo release
// output or a CI artifact). The version defaults to the one in Cargo.toml.

using System;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Text.RegularExpressions;
using WixSharp;
using File = WixSharp.File;

namespace Jiffshot.Package;

internal static class Program
{
    private const string Vendor = "Devolutions";
    private const string Publisher = "Devolutions Inc.";
    private const string ProductName = "Jiffshot";
    private const string InstallFolder = "Jiffshot";
    private const string ExecutableName = "jiffshot.exe";
    private const string DocumentProgId = "jiffshot.Document";

    /// <summary>Identifies Jiffshot across versions: never change it.</summary>
    private static readonly Guid UpgradeCode = new("68498651-3ba2-495a-9a99-a008177cf534");

    private static int Main(string[] args)
    {
        try
        {
            var options = Options.Parse(args);
            var msi = Build(options);
            Console.WriteLine($"Built {msi}");
            return 0;
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"error: {e.Message}");
            return 1;
        }
    }

    private static string Build(Options o)
    {
        var exe = Path.Combine(o.Bin, ExecutableName);
        if (!System.IO.File.Exists(exe))
            throw new FileNotFoundException($"{ExecutableName} is not in {o.Bin}");
        var exeVersion = FileVersionInfo.GetVersionInfo(exe);
        var expectedFileVersion = $"{o.Version.Major}.{o.Version.Minor}.{o.Version.Build}.0";
        if (exeVersion.FileVersion != expectedFileVersion || exeVersion.ProductVersion != o.Version.ToString())
            throw new InvalidDataException(
                $"{ExecutableName} has version '{exeVersion.FileVersion}' / '{exeVersion.ProductVersion}', expected '{expectedFileVersion}' / '{o.Version}'");
        var libraries = Directory.GetFiles(o.Bin, "*.dll");
        if (!libraries.Any(l => Path.GetFileName(l).Equals("rustolonia_host.dll", StringComparison.OrdinalIgnoreCase)))
            throw new FileNotFoundException($"the Rustolonia host (rustolonia_host.dll) is not in {o.Bin}");

        var installDir = $"[{InstallDirProperty}]";
        var program = new File(exe,
            new FileShortcut(ProductName, "%ProgramMenu%") { WorkingDirectory = InstallDirProperty });
        var files = new WixEntity[] { program }.Concat(libraries.Select(l => new File(l))).ToArray();

        var project = new Project($"{Vendor} {ProductName}",
            // %ProgramFiles% is the 32-bit folder in WixSharp's ARM64 packages.
            new Dir($@"%ProgramFiles64Folder%\{Vendor}", new InstallDir(InstallFolder, files)),
            // Jiffshot documents open in Jiffshot (a running one opens them in its editor).
            Machine(@"Software\Classes\.jsd", "", DocumentProgId),
            Machine(@"Software\Classes\.jsd\OpenWithProgids", DocumentProgId, ""),
            Machine($@"Software\Classes\{DocumentProgId}", "", "Jiffshot document"),
            Machine($@"Software\Classes\{DocumentProgId}\DefaultIcon", "", $"{installDir}{ExecutableName},0"),
            Machine($@"Software\Classes\{DocumentProgId}\shell\open\command", "", $"\"{installDir}{ExecutableName}\" \"%1\""),
            Machine($@"Software\{Vendor}\{ProductName}", "InstallDir", installDir))
        {
            GUID = UpgradeCode,
            UpgradeCode = UpgradeCode,
            // A new product code for every build: each install is a major upgrade.
            ProductId = Guid.NewGuid(),
            Version = o.Version,
            Platform = o.Platform,
            // Windows Installer 5 (Windows 7 and later): ARM64 packages need it.
            InstallerVersion = 500,
            Scope = InstallScope.perMachine,
            Language = "en-US",
            OutDir = o.Out,
            OutFileName = $"{Vendor}.{ProductName}-{o.PlatformName}-{o.Version}",
            UI = WUI.WixUI_ProgressOnly,
            MajorUpgrade = new MajorUpgrade
            {
                AllowSameVersionUpgrades = true,
                DowngradeErrorMessage = "A newer version of Jiffshot is already installed.",
                Schedule = UpgradeSchedule.afterInstallInitialize,
            },
            ControlPanelInfo = new ProductInfo
            {
                Manufacturer = Publisher,
                ProductIcon = o.Icon,
                UrlInfoAbout = "https://github.com/Devolutions/jiffshot",
                HelpLink = "https://github.com/Devolutions/jiffshot/issues",
            },
        };
        // WixSharp makes an HKCU value the key path of the shortcut's
        // components, for per-user installs. This one is per-machine, so
        // jiffshot.exe stays the key path, and the validation rules that
        // assume a per-user Start menu do not apply.
        AutoElements.DisableAutoUserProfileRegistry = true;
        // WixSharp marks only the file components 64-bit on ARM64; the
        // registry ones must be too, or they land in the 32-bit view.
        project.WixSourceGenerated += doc =>
        {
            foreach (var component in doc.Descendants().Where(e => e.Name.LocalName == "Component"))
                component.SetAttributeValue("Bitness", "always64");
        };

        // Light leaves an MSI behind even when validation fails, and BuildMsi
        // still returns it, so its errors are caught from the output.
        var errors = 0;
        Compiler.OutputWriteLine = line =>
        {
            Console.WriteLine(line);
            if (Regex.IsMatch(line, @"\berror [A-Z]+[0-9]+\b"))
                errors++;
        };
        var msi = project.BuildMsi();
        if (msi is null || errors > 0)
            throw new InvalidOperationException("WiX failed to build the MSI: see its output above");
        return msi;
    }

    private const string InstallDirProperty = "INSTALLDIR";

    /// <summary>A value under HKEY_LOCAL_MACHINE, in the 64-bit view (x64 and ARM64).</summary>
    private static RegValue Machine(string key, string name, string value) =>
        new(RegistryHive.LocalMachine, key, name, value) { Win64 = true };

    private sealed class Options
    {
        public string Bin { get; private set; } = "";
        public Platform Platform { get; private set; } = Platform.x64;
        public string PlatformName { get; private set; } = "x64";
        public Version Version { get; private set; } = new(0, 0, 0);
        public string Out { get; private set; } = "";
        public string Icon { get; private set; } = "";

        public static Options Parse(string[] args)
        {
            var root = RepositoryRoot();
            var o = new Options { Out = Path.Combine(root, "package", "out"), Icon = Path.Combine(root, "assets", "jiffshot.ico") };
            string? version = null;
            for (var i = 0; i < args.Length; i++)
            {
                string Value() => i + 1 < args.Length ? args[++i] : throw new ArgumentException($"{args[i]} needs a value");
                switch (args[i])
                {
                    case "-bin":
                        o.Bin = Path.GetFullPath(Value());
                        break;
                    case "-platform":
                        (o.Platform, o.PlatformName) = Value().ToLowerInvariant() switch
                        {
                            "x64" or "x86_64" or "amd64" or "win-x64" => (Platform.x64, "x64"),
                            "arm64" or "aarch64" or "win-arm64" => (Platform.arm64, "arm64"),
                            var other => throw new ArgumentException($"unsupported platform {other}: use x64 or arm64"),
                        };
                        break;
                    case "-version":
                        version = Value();
                        break;
                    case "-out":
                        o.Out = Path.GetFullPath(Value());
                        break;
                    default:
                        throw new ArgumentException($"unknown argument {args[i]}");
                }
            }
            if (o.Bin.Length == 0)
                throw new ArgumentException("-bin <folder with jiffshot.exe> is required");
            o.Version = Version.Parse(version ?? CargoVersion(root));
            Directory.CreateDirectory(o.Out);
            return o;
        }

        /// <summary>The repository root: the first parent holding Cargo.toml.</summary>
        private static string RepositoryRoot()
        {
            for (var dir = new DirectoryInfo(AppContext.BaseDirectory); dir != null; dir = dir.Parent)
            {
                if (System.IO.File.Exists(Path.Combine(dir.FullName, "Cargo.toml")))
                    return dir.FullName;
            }
            throw new DirectoryNotFoundException("run from inside the Jiffshot repository");
        }

        /// <summary>The version in Cargo.toml's [workspace.package].</summary>
        private static string CargoVersion(string root)
        {
            var toml = System.IO.File.ReadAllText(Path.Combine(root, "Cargo.toml"));
            var section = Regex.Match(toml, @"\[workspace\.package\](?<body>[^\[]*)");
            var version = Regex.Match(section.Groups["body"].Value, @"^\s*version\s*=\s*""(?<v>[0-9]+\.[0-9]+\.[0-9]+)""", RegexOptions.Multiline);
            if (!version.Success)
                throw new InvalidDataException("no version in Cargo.toml's [workspace.package]");
            return version.Groups["v"].Value;
        }
    }
}
