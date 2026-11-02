#Requires -Version 7.4

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:Root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$script:HostLibraries = @(
    'librustolonia_host.dylib',
    'libAvaloniaNative.dylib',
    'libHarfBuzzSharp.dylib',
    'libSkiaSharp.dylib'
)
$script:MinimumMacOS = '13.0'
$script:PopupGateMarker = 'jiffshot-native-popup-gate-v1: supported'
$script:TerminationMarker = 'jiffshot-native-termination-v1: supported'
$script:FileDragNativeMarker = 'jiffshot-native-file-drag-v1: supported'
$script:FileDragAbiMarker = 'jiffshot-file-drag-abi-v1: supported'
$script:HostDiscoveryMarker = "jiffshot-host-discovery-v1$([char]0)"
$script:HostDiscoveryError = 'jiffshot-host-discovery-v1: '

function Invoke-MacCommand {
    param(
        [Parameter(Mandatory)][string]$File,
        [string[]]$Arguments = @(),
        [string]$WorkingDirectory,
        [System.Collections.IDictionary]$Environment,
        [int]$TimeoutSeconds = 0,
        [switch]$AllowFailure
    )
    $info = [Diagnostics.ProcessStartInfo]::new($File)
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    if ($WorkingDirectory) { $info.WorkingDirectory = $WorkingDirectory }
    if ($null -ne $Environment) {
        $info.Environment.Clear()
        foreach ($key in $Environment.Keys) {
            $info.Environment[[string]$key] = [string]$Environment[$key]
        }
    }
    $process = [Diagnostics.Process]::Start($info)
    try {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if ($TimeoutSeconds -gt 0 -and -not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $process.Kill($true)
            $process.WaitForExit()
            throw "Command '$File' timed out after $TimeoutSeconds seconds"
        }
        $process.WaitForExit()
        $result = [pscustomobject]@{
            ExitCode = $process.ExitCode
            Stdout = $stdout.GetAwaiter().GetResult()
            Stderr = $stderr.GetAwaiter().GetResult()
        }
    }
    finally {
        $process.Dispose()
    }
    if (-not $AllowFailure -and $result.ExitCode -ne 0) {
        $detail = if ($result.Stderr.Trim()) { $result.Stderr.Trim() } else { $result.Stdout.Trim() }
        throw "Command '$File' failed with exit status $($result.ExitCode): $detail"
    }
    return $result
}

function Invoke-MacRun {
    param([string]$File, [string[]]$Arguments)
    $result = Invoke-MacCommand -File $File -Arguments $Arguments
    if ($result.Stdout) { [Console]::Out.Write($result.Stdout) }
    if ($result.Stderr) { [Console]::Error.Write($result.Stderr) }
}

function Get-MacWorkspaceVersion {
    $text = [IO.File]::ReadAllText((Join-Path $script:Root 'Cargo.toml'))
    $section = [regex]::Match($text, '(?ms)^\[workspace\.package\][ \t]*\r?\n(.*?)(?=^\[|\z)')
    if (-not $section.Success) { throw 'Cargo.toml has no workspace.package section' }
    $versions = [regex]::Matches($section.Groups[1].Value, '(?m)^[ \t]*version[ \t]*=[ \t]*"([0-9]+\.[0-9]+\.[0-9]+)"[ \t]*(?:#.*)?\r?$')
    if ($versions.Count -ne 1) { throw 'Cargo.toml must contain one workspace version in X.Y.Z format' }
    return $versions[0].Groups[1].Value
}

function ConvertTo-MacVersion([string]$Version) {
    if ($Version -cnotmatch '^[0-9]+(?:\.[0-9]+){0,2}$') { throw "Invalid macOS version: $Version" }
    $parts = @($Version.Split('.') | ForEach-Object { [int]$_ })
    while ($parts.Count -lt 3) { $parts += 0 }
    return [version]::new($parts[0], $parts[1], $parts[2])
}

function Read-MacDeploymentTarget([string]$Output) {
    $targets = [Collections.Generic.List[string]]::new()
    $blocks = [regex]::Split($Output, '(?m)^Load command [0-9]+[ \t]*\r?$')
    foreach ($block in $blocks | Select-Object -Skip 1) {
        $command = [regex]::Match($block, '(?m)^[ \t]*cmd[ \t]+(\S+)[ \t]*\r?$')
        if (-not $command.Success) { continue }
        if ($command.Groups[1].Value -ceq 'LC_BUILD_VERSION') {
            $platform = [regex]::Match($block, '(?m)^[ \t]*platform[ \t]+(\S+)[ \t]*\r?$')
            $version = [regex]::Match($block, '(?m)^[ \t]*minos[ \t]+(\S+)[ \t]*\r?$')
            if (-not $platform.Success -or $platform.Groups[1].Value -cnotin @('MACOS', '1') -or -not $version.Success) {
                throw 'Mach-O build metadata does not identify a macOS deployment target'
            }
            $targets.Add($version.Groups[1].Value)
        }
        elseif ($command.Groups[1].Value -ceq 'LC_VERSION_MIN_MACOSX') {
            $version = [regex]::Match($block, '(?m)^[ \t]*version[ \t]+(\S+)[ \t]*\r?$')
            if (-not $version.Success) { throw 'Mach-O build metadata does not identify a macOS deployment target' }
            $targets.Add($version.Groups[1].Value)
        }
    }
    if ($targets.Count -ne 1) { throw 'Mach-O must contain exactly one macOS deployment target' }
    [void](ConvertTo-MacVersion $targets[0])
    return $targets[0]
}

function Get-MacDeploymentTarget([string]$Path, [string]$Arch) {
    try {
        $result = Invoke-MacCommand -File '/usr/bin/vtool' -Arguments @('-show-build', '-arch', $Arch, $Path)
    }
    catch { throw "Cannot inspect $Path (${Arch}): $($_.Exception.Message)" }
    try { return Read-MacDeploymentTarget $result.Stdout }
    catch { throw "Cannot determine the macOS deployment target for $Path (${Arch}): $($_.Exception.Message)" }
}

function Get-MacInfoPlist([string]$Version) {
    return [ordered]@{
        CFBundleDevelopmentRegion = 'en'
        CFBundleDisplayName = 'Jiffshot'
        CFBundleExecutable = 'jiffshot'
        CFBundleIconFile = 'Jiffshot.icns'
        CFBundleIdentifier = 'com.devolutions.jiffshot'
        CFBundleInfoDictionaryVersion = '6.0'
        CFBundleName = 'Jiffshot'
        CFBundlePackageType = 'APPL'
        CFBundleShortVersionString = $Version
        CFBundleVersion = $Version
        LSMinimumSystemVersion = $script:MinimumMacOS
        NSHighResolutionCapable = $true
        NSCameraUsageDescription = 'Jiffshot uses the camera when you include a webcam in a recording.'
        NSMicrophoneUsageDescription = 'Jiffshot uses the microphone when you include your voice in a recording.'
        NSScreenCaptureUsageDescription = 'Jiffshot captures the screen and system audio when you request a capture or recording.'
        NSAppleEventsUsageDescription = 'Jiffshot uses automation when you choose to print or share a capture with another application.'
        CFBundleDocumentTypes = @([ordered]@{
            CFBundleTypeName = 'Jiffshot Document'
            CFBundleTypeRole = 'Editor'
            LSHandlerRank = 'Owner'
            LSItemContentTypes = @('com.devolutions.jiffshot.document')
        })
        UTExportedTypeDeclarations = @([ordered]@{
            UTTypeIdentifier = 'com.devolutions.jiffshot.document'
            UTTypeDescription = 'Jiffshot Document'
            UTTypeConformsTo = @('public.data')
            UTTypeTagSpecification = [ordered]@{ 'public.filename-extension' = @('jsd') }
        })
    }
}

function Add-PlistValue([Xml.XmlDocument]$Document, [Xml.XmlNode]$Parent, $Value) {
    if ($Value -is [System.Collections.IDictionary]) {
        $node = $Document.CreateElement('dict')
        foreach ($key in $Value.Keys) {
            $entry = $Document.CreateElement('key')
            $entry.InnerText = [string]$key
            [void]$node.AppendChild($entry)
            Add-PlistValue $Document $node $Value[$key]
        }
    }
    elseif ($Value -is [bool]) {
        $node = $Document.CreateElement($(if ($Value) { 'true' } else { 'false' }))
    }
    elseif ($Value -is [string]) {
        $node = $Document.CreateElement('string')
        $node.InnerText = $Value
    }
    elseif ($Value -is [System.Collections.IEnumerable]) {
        $node = $Document.CreateElement('array')
        foreach ($entry in $Value) { Add-PlistValue $Document $node $entry }
    }
    else { throw "Unsupported plist value: $Value" }
    [void]$Parent.AppendChild($node)
}

function Write-MacPlist([string]$Path, [System.Collections.IDictionary]$Values) {
    $document = [Xml.XmlDocument]::new()
    [void]$document.AppendChild($document.CreateXmlDeclaration('1.0', 'UTF-8', $null))
    $plist = $document.CreateElement('plist')
    $plist.SetAttribute('version', '1.0')
    [void]$document.AppendChild($plist)
    Add-PlistValue $document $plist $Values
    $settings = [Xml.XmlWriterSettings]::new()
    $settings.Encoding = [Text.UTF8Encoding]::new($false)
    $settings.Indent = $true
    $writer = [Xml.XmlWriter]::Create($Path, $settings)
    try { $document.Save($writer) } finally { $writer.Dispose() }
}

function New-MacIcon([string]$Destination, [string]$Temporary) {
    $icons = Join-Path $Temporary 'Jiffshot.iconset'
    [void][IO.Directory]::CreateDirectory($icons)
    $source = Join-Path $Temporary 'icon.png'
    Invoke-MacRun '/usr/bin/sips' @('-s', 'format', 'png', (Join-Path $script:Root 'assets/jiffshot.ico'), '--out', $source)
    foreach ($size in 16, 32, 128, 256, 512) {
        foreach ($scale in 1, 2) {
            $suffix = if ($scale -eq 2) { '@2x' } else { '' }
            $name = "icon_${size}x${size}${suffix}.png"
            $pixels = [string]($size * $scale)
            Invoke-MacRun '/usr/bin/sips' @('-z', $pixels, $pixels, $source, '--out', (Join-Path $icons $name))
        }
    }
    Invoke-MacRun '/usr/bin/iconutil' @('-c', 'icns', $icons, '-o', $Destination)
}

function Sign-MacPath([string]$Path, [string]$Identity, [string]$Keychain, [string]$Entitlements, [bool]$Runtime = $true) {
    $arguments = @('--force', '--sign', $Identity)
    if ($Keychain) { $arguments += @('--keychain', $Keychain) }
    if ($Identity -cne '-') {
        $arguments += '--timestamp'
        if ($Runtime) { $arguments += @('--options', 'runtime') }
    }
    if ($Entitlements) { $arguments += @('--entitlements', $Entitlements) }
    Invoke-MacRun '/usr/bin/codesign' ($arguments + $Path)
}

function Submit-MacNotarization([string]$Path, [string]$Profile, [string]$Keychain) {
    $arguments = @('notarytool', 'submit', $Path, '--keychain-profile', $Profile, '--wait')
    if ($Keychain) { $arguments += @('--keychain', $Keychain) }
    Invoke-MacRun '/usr/bin/xcrun' $arguments
}

function Get-CleanHostEnvironment {
    $environment = @{}
    foreach ($entry in [Environment]::GetEnvironmentVariables().GetEnumerator()) {
        $key = [string]$entry.Key
        if (-not $key.StartsWith('RUSTOLONIA_', [StringComparison]::Ordinal) -and
            -not $key.StartsWith('DYLD_', [StringComparison]::Ordinal)) {
            $environment[$key] = [string]$entry.Value
        }
    }
    return $environment
}

function Confirm-MacHostCapability([string]$Binary, [string]$HostPath, [string]$Argument, [string]$Marker, [string]$Description) {
    $environment = Get-CleanHostEnvironment
    $environment['RUSTOLONIA_HOST_LIB'] = $HostPath
    $environment['RUSTOLONIA_HOST_DIR'] = [IO.Path]::GetDirectoryName($HostPath)
    try {
        $result = Invoke-MacCommand -File $Binary -Arguments @($Argument) -Environment $environment -TimeoutSeconds 30 -AllowFailure
    }
    catch { throw "Could not check the staged Rustolonia ${Description}: $($_.Exception.Message)" }
    if ($result.ExitCode -ne 0 -or $Marker -cnotin @($result.Stdout -split '\r?\n')) {
        $detail = if ($result.Stderr.Trim()) { $result.Stderr.Trim() } elseif ($result.Stdout.Trim()) { $result.Stdout.Trim() } else { "exit status $($result.ExitCode)" }
        throw "The staged Rustolonia host does not pass the native $Description check: $detail"
    }
}

function Confirm-MacPopupGate([string]$Binary, [string]$HostPath) {
    Confirm-MacHostCapability $Binary $HostPath '--internal-check-native-popup-gate' $script:PopupGateMarker 'popup-gate'
}

function Confirm-MacTermination([string]$Binary, [string]$HostPath) {
    Confirm-MacHostCapability $Binary $HostPath '--internal-check-native-termination' $script:TerminationMarker 'termination'
}

function Confirm-MacFileDrag([string]$Binary, [string]$HostPath) {
    Confirm-MacHostCapability $Binary $HostPath '--internal-check-native-file-drag' $script:FileDragNativeMarker 'file-drag companion'
    Confirm-MacHostCapability $Binary $HostPath '--internal-check-file-drag-abi' $script:FileDragAbiMarker 'file-drag host ABI'
}

function Resolve-MacPhysicalPath([string]$Path) {
    $result = Invoke-MacCommand -File '/bin/realpath' -Arguments @('--', $Path)
    if (-not $result.Stdout.EndsWith("`n")) { throw "Invalid realpath output for $Path" }
    return $result.Stdout.Substring(0, $result.Stdout.Length - 1)
}

function Get-MacDiscoveredHost([string]$Binary, [string]$WorkingDirectory) {
    try {
        $result = Invoke-MacCommand -File $Binary -Arguments @('--internal-check-host-discovery') `
            -WorkingDirectory $WorkingDirectory -Environment (Get-CleanHostEnvironment) -TimeoutSeconds 30 -AllowFailure
    }
    catch { throw "Could not check Rustolonia host discovery: $($_.Exception.Message)" }
    if ($result.ExitCode -eq 0 -and $result.Stdout.StartsWith($script:HostDiscoveryMarker, [StringComparison]::Ordinal)) {
        $path = $result.Stdout.Substring($script:HostDiscoveryMarker.Length)
        if ($path.Length) { return Resolve-MacPhysicalPath $path }
    }
    if ($result.ExitCode -eq 1 -and -not $result.Stdout -and
        $result.Stderr.StartsWith($script:HostDiscoveryError, [StringComparison]::Ordinal) -and
        $result.Stderr.Substring($script:HostDiscoveryError.Length).Trim()) {
        return $null
    }
    $detail = if ($result.Stderr.Trim()) { $result.Stderr.Trim() } else { "exit status $($result.ExitCode)" }
    throw "The Rustolonia host discovery probe failed: $detail"
}

function Confirm-MacHostDiscovery([string]$Binary, [string]$HostPath, [string]$Temporary) {
    $found = Get-MacDiscoveredHost $Binary ([IO.Path]::GetDirectoryName($Binary))
    if ($found -cne (Resolve-MacPhysicalPath $HostPath)) {
        throw "The staged application did not discover its bundled host: $found"
    }
    $isolated = Join-Path $Temporary 'isolated-host-probe'
    [void][IO.Directory]::CreateDirectory($isolated)
    $unbundled = Join-Path $isolated 'jiffshot'
    [IO.File]::Copy($Binary, $unbundled)
    $found = Get-MacDiscoveredHost $unbundled $isolated
    if ($null -ne $found) { throw "The application can discover an unstaged Rustolonia host: $found" }
}

function Invoke-MacPackage {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$Binary,
        [Parameter(Mandatory)][string]$Output,
        [Parameter(Mandatory)][ValidateSet('arm64', 'x86_64')][string]$Arch,
        [string]$Identity = '-',
        [string]$Keychain,
        [string]$NotaryProfile,
        [bool]$RequirePopupGate = $true,
        [bool]$RequireTermination = $true,
        [bool]$RequireFileDrag = $true
    )
    $Binary = [IO.Path]::GetFullPath($Binary)
    $Output = [IO.Path]::GetFullPath($Output)
    if (-not [IO.File]::Exists($Binary)) { throw "Application binary not found: $Binary" }
    $directory = [IO.Path]::GetDirectoryName($Binary)
    $cli = Join-Path $directory 'jiffshot-cli'
    $libraries = @($script:HostLibraries | ForEach-Object { Join-Path $directory $_ })
    foreach ($path in @($Binary, $cli) + $libraries) {
        if (-not [IO.File]::Exists($path)) { throw "Required bundle binary not found: $path" }
        Invoke-MacRun '/usr/bin/lipo' @($path, '-verify_arch', $Arch)
        $minimum = Get-MacDeploymentTarget $path $Arch
        if ((ConvertTo-MacVersion $minimum) -gt (ConvertTo-MacVersion $script:MinimumMacOS)) {
            throw "$path requires macOS $minimum; the bundle must support macOS $script:MinimumMacOS"
        }
    }
    if ($NotaryProfile -and $Identity -ceq '-') { throw 'Notarization requires a Developer ID signing identity' }
    if (-not ($RequirePopupGate -and $RequireTermination -and $RequireFileDrag)) {
        if ($Identity -cne '-' -or $NotaryProfile) { throw 'Signing or notarization requires all native host capability checks' }
        [Console]::Error.WriteLine('WARNING: native host capability checks were skipped; this package may not run and is not suitable for distribution.')
    }
    $version = Get-MacWorkspaceVersion
    if ($env:JIFFSHOT_VERSION -and $env:JIFFSHOT_VERSION -cne $version) {
        throw "JIFFSHOT_VERSION $env:JIFFSHOT_VERSION does not match Cargo.toml version $version"
    }
    [void][IO.Directory]::CreateDirectory($Output)
    $appOutput = Join-Path $Output 'Jiffshot.app'
    $dmgOutput = Join-Path $Output "Devolutions.Jiffshot-macos-$Arch-$version.dmg"
    if ([IO.Directory]::Exists($appOutput) -or [IO.File]::Exists($appOutput) -or [IO.File]::Exists($dmgOutput) -or [IO.Directory]::Exists($dmgOutput)) {
        throw 'The output app or disk image already exists; choose an empty output directory'
    }
    $temporary = [IO.Directory]::CreateTempSubdirectory('jiffshot-macos-').FullName
    try {
        $staging = Join-Path $temporary 'image'
        $app = Join-Path $staging 'Jiffshot.app'
        $macos = Join-Path $app 'Contents/MacOS'
        $frameworks = Join-Path $app 'Contents/Frameworks'
        $resources = Join-Path $app 'Contents/Resources'
        foreach ($path in $macos, $frameworks, $resources) { [void][IO.Directory]::CreateDirectory($path) }
        [IO.File]::Copy($Binary, (Join-Path $macos 'jiffshot'))
        [IO.File]::Copy($cli, (Join-Path $macos 'jiffshot-cli'))
        foreach ($library in $libraries) { [IO.File]::Copy($library, (Join-Path $frameworks ([IO.Path]::GetFileName($library)))) }
        Write-MacPlist (Join-Path $app 'Contents/Info.plist') (Get-MacInfoPlist $version)
        New-MacIcon (Join-Path $resources 'Jiffshot.icns') $temporary
        foreach ($library in $libraries) {
            Sign-MacPath (Join-Path $frameworks ([IO.Path]::GetFileName($library))) $Identity $Keychain $null
        }
        Sign-MacPath (Join-Path $macos 'jiffshot-cli') $Identity $Keychain $null
        Sign-MacPath $app $Identity $Keychain (Join-Path $script:Root 'package/macos/entitlements.plist')
        Invoke-MacRun '/usr/bin/codesign' @('--verify', '--deep', '--strict', $app)
        $stagedBinary = Join-Path $macos 'jiffshot'
        $stagedHost = Join-Path $frameworks $script:HostLibraries[0]
        if ($RequirePopupGate -or $RequireTermination -or $RequireFileDrag) { Confirm-MacHostDiscovery $stagedBinary $stagedHost $temporary }
        if ($RequirePopupGate) { Confirm-MacPopupGate $stagedBinary $stagedHost }
        if ($RequireTermination) { Confirm-MacTermination $stagedBinary $stagedHost }
        if ($RequireFileDrag) { Confirm-MacFileDrag $stagedBinary $stagedHost }
        if ($NotaryProfile) {
            $archive = Join-Path $temporary 'Jiffshot.zip'
            Invoke-MacRun '/usr/bin/ditto' @('-c', '-k', '--keepParent', $app, $archive)
            Submit-MacNotarization $archive $NotaryProfile $Keychain
            Invoke-MacRun '/usr/bin/xcrun' @('stapler', 'staple', $app)
        }
        [void][IO.Directory]::CreateSymbolicLink((Join-Path $staging 'Applications'), '/Applications')
        $temporaryDmg = Join-Path $temporary ([IO.Path]::GetFileName($dmgOutput))
        Invoke-MacRun '/usr/bin/hdiutil' @('create', '-quiet', '-volname', 'Jiffshot', '-srcfolder', $staging, '-fs', 'HFS+', '-format', 'UDZO', $temporaryDmg)
        if ($Identity -cne '-') { Sign-MacPath $temporaryDmg $Identity $Keychain $null $false }
        if ($NotaryProfile) {
            Submit-MacNotarization $temporaryDmg $NotaryProfile $Keychain
            Invoke-MacRun '/usr/bin/xcrun' @('stapler', 'staple', $temporaryDmg)
        }
        Invoke-MacRun '/usr/bin/ditto' @($app, $appOutput)
        [IO.File]::Copy($temporaryDmg, $dmgOutput)
    }
    finally {
        [IO.Directory]::Delete($temporary, $true)
    }
    Write-Host "Packaged $appOutput and $dmgOutput"
    if ($Identity -ceq '-') { Write-Host 'The package is ad-hoc signed, not notarized, and is intended for local testing.' }
    elseif (-not $NotaryProfile) { Write-Host 'The package is Developer ID signed but has not been notarized.' }
}

Export-ModuleMember -Function Invoke-MacPackage, Invoke-MacCommand, Write-MacPlist, Get-CleanHostEnvironment
