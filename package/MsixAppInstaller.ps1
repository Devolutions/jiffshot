$script:AppxNamespace = 'http://schemas.microsoft.com/appx/manifest/foundation/windows10'
$script:Uap13Namespace = 'http://schemas.microsoft.com/appx/manifest/uap/windows10/13'
$script:AppInstallerNamespace = 'http://schemas.microsoft.com/appx/appinstaller/2021'
$script:EmbeddedAppInstallerName = 'Update.appinstaller'

function Get-MsixAppInstallerName {
    [CmdletBinding()]
    param([Parameter(Mandatory)][ValidateSet('x64', 'arm64')][string]$Architecture)

    "Devolutions.Jiffshot-windows-$Architecture.appinstaller"
}

function Get-MsixReleasePackageName {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][ValidateSet('x64', 'arm64')][string]$Architecture,
        [Parameter(Mandatory)][string]$Version
    )

    "Devolutions.Jiffshot-windows-$Architecture-$Version.msix"
}

# Builds the App Installer feed for a package manifest. The feed URI always names the
# latest release, so Windows follows it to newer versions; the package URI names the
# exact release asset that contains this manifest.
function ConvertTo-MsixAppInstaller {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][xml]$Manifest,
        [Parameter(Mandatory)][string]$Repository
    )

    if ($Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') {
        throw "Expected an owner/repository name, got '$Repository'."
    }
    $namespaces = [System.Xml.XmlNamespaceManager]::new($Manifest.NameTable)
    $namespaces.AddNamespace('appx', $script:AppxNamespace)
    $namespaces.AddNamespace('uap13', $script:Uap13Namespace)
    $identity = $Manifest.SelectSingleNode('/appx:Package/appx:Identity', $namespaces)
    if ($null -eq $identity) { throw 'The package manifest has no identity.' }
    $name = $identity.GetAttribute('Name')
    $publisher = $identity.GetAttribute('Publisher')
    $packageVersion = $identity.GetAttribute('Version')
    $architecture = $identity.GetAttribute('ProcessorArchitecture')
    if ([string]::IsNullOrWhiteSpace($name) -or [string]::IsNullOrWhiteSpace($publisher)) {
        throw 'The package manifest must contain a name and publisher.'
    }
    if ($packageVersion -notmatch '^(?<v>(0|[1-9][0-9]{0,4})\.(0|[1-9][0-9]{0,4})\.(0|[1-9][0-9]{0,4}))\.0$') {
        throw "The package version must be major.minor.build.0, got '$packageVersion'."
    }
    $version = $Matches['v']
    if ($architecture -notin @('x64', 'arm64')) {
        throw "The package architecture must be x64 or arm64, got '$architecture'."
    }
    $reference = $Manifest.SelectSingleNode(
        '/appx:Package/appx:Properties/uap13:AutoUpdate/uap13:AppInstaller', $namespaces)
    if ($null -eq $reference -or $reference.GetAttribute('File') -cne $script:EmbeddedAppInstallerName) {
        throw "The package manifest must reference the embedded $script:EmbeddedAppInstallerName."
    }

    $releases = "https://github.com/$Repository/releases"
    $packageUri = "$releases/download/v$version/$(Get-MsixReleasePackageName -Architecture $architecture -Version $version)"
    $document = [xml]::new()
    [void]$document.AppendChild($document.CreateXmlDeclaration('1.0', 'utf-8', $null))
    $installer = $document.CreateElement('AppInstaller', $script:AppInstallerNamespace)
    $installer.SetAttribute('Version', $packageVersion)
    $installer.SetAttribute('Uri', "$releases/latest/download/$(Get-MsixAppInstallerName -Architecture $architecture)")
    [void]$document.AppendChild($installer)

    $package = $document.CreateElement('MainPackage', $script:AppInstallerNamespace)
    $package.SetAttribute('Name', $name)
    $package.SetAttribute('Publisher', $publisher)
    $package.SetAttribute('Version', $packageVersion)
    $package.SetAttribute('ProcessorArchitecture', $architecture)
    $package.SetAttribute('Uri', $packageUri)
    [void]$installer.AppendChild($package)

    # Check on every launch without prompting or blocking it, and in the background.
    $settings = $document.CreateElement('UpdateSettings', $script:AppInstallerNamespace)
    $onLaunch = $document.CreateElement('OnLaunch', $script:AppInstallerNamespace)
    $onLaunch.SetAttribute('HoursBetweenUpdateChecks', '0')
    $onLaunch.SetAttribute('ShowPrompt', 'false')
    $onLaunch.SetAttribute('UpdateBlocksActivation', 'false')
    [void]$settings.AppendChild($onLaunch)
    [void]$settings.AppendChild($document.CreateElement('AutomaticBackgroundTask', $script:AppInstallerNamespace))
    [void]$installer.AppendChild($settings)

    $writerSettings = [System.Xml.XmlWriterSettings]::new()
    $writerSettings.Encoding = [System.Text.UTF8Encoding]::new($false)
    $writerSettings.Indent = $true
    $writerSettings.NewLineChars = "`n"
    $stream = [System.IO.MemoryStream]::new()
    try {
        $writer = [System.Xml.XmlWriter]::Create($stream, $writerSettings)
        try { $document.Save($writer) } finally { $writer.Dispose() }
        , $stream.ToArray()
    } finally {
        $stream.Dispose()
    }
}

function New-MsixAppInstaller {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$ManifestPath,
        [Parameter(Mandatory)][string]$Repository,
        [Parameter(Mandatory)][string]$OutputPath
    )

    [xml]$manifest = Get-Content -LiteralPath $ManifestPath -Raw
    $bytes = ConvertTo-MsixAppInstaller -Manifest $manifest -Repository $Repository
    $output = [System.IO.Path]::GetFullPath($OutputPath)
    [void][System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($output))
    [System.IO.File]::WriteAllBytes($output, $bytes)
}

# Verifies that a package embeds exactly the published feed, and that the feed matches
# the package identity and its published file name.
function Test-MsixAppInstallerPackage {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$PackagePath,
        [Parameter(Mandatory)][string]$AppInstallerPath,
        [Parameter(Mandatory)][string]$Repository,
        [string]$PublishedPackageName
    )

    Add-Type -AssemblyName System.IO.Compression
    $published = [System.IO.File]::ReadAllBytes([System.IO.Path]::GetFullPath($AppInstallerPath))
    $archive = [System.IO.Compression.ZipFile]::OpenRead([System.IO.Path]::GetFullPath($PackagePath))
    try {
        $read = {
            param([string]$Name)
            $entry = $archive.GetEntry($Name)
            if ($null -eq $entry) { throw "The package is missing $Name." }
            $source = $entry.Open()
            $buffer = [System.IO.MemoryStream]::new()
            try { $source.CopyTo($buffer); , $buffer.ToArray() } finally { $source.Dispose(); $buffer.Dispose() }
        }
        $manifestBytes = & $read 'AppxManifest.xml'
        $embedded = & $read $script:EmbeddedAppInstallerName
    } finally {
        $archive.Dispose()
    }

    [xml]$manifest = [System.Text.Encoding]::UTF8.GetString($manifestBytes).TrimStart([char]0xFEFF)
    $expected = ConvertTo-MsixAppInstaller -Manifest $manifest -Repository $Repository
    if (-not [System.Linq.Enumerable]::SequenceEqual([byte[]]$embedded, [byte[]]$expected)) {
        throw "The embedded $script:EmbeddedAppInstallerName does not match the package identity."
    }
    if (-not [System.Linq.Enumerable]::SequenceEqual([byte[]]$published, [byte[]]$expected)) {
        throw 'The published App Installer feed differs from the one embedded in the package.'
    }
    $name = if ($PublishedPackageName) { $PublishedPackageName } else { [System.IO.Path]::GetFileName($PackagePath) }
    [xml]$feed = [System.Text.Encoding]::UTF8.GetString($published)
    $uri = [uri]$feed.AppInstaller.MainPackage.Uri
    if ([uri]::UnescapeDataString($uri.Segments[-1]) -cne $name) {
        throw "The App Installer package URI '$uri' does not name the published package '$name'."
    }
    $feedName = [uri]::UnescapeDataString(([uri]$feed.AppInstaller.Uri).Segments[-1])
    if ($feedName -cne [System.IO.Path]::GetFileName($AppInstallerPath)) {
        throw "The App Installer feed URI names '$feedName', not the published feed file."
    }
}
