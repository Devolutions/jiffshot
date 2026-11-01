#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify sharing selected saved captures with private desktop portal mocks.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.Desktop.psm1') -Force

function Set-SearchQuery {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver, [Parameter(Mandatory)][string]$Query)
    $Driver.Click(80, 214)
    $Driver.SelectAll()
    $Driver.Key('Delete', $true)
    $Driver.Key('Delete', $false)
    foreach ($character in $Query.ToCharArray()) {
        $shifted = $character -ceq ':'
        $name = switch -CaseSensitive ([string]$character) {
            ':' { 'semicolon' }
            '.' { 'period' }
            ' ' { 'space' }
            default { [string]$character }
        }
        if ($shifted) {
            $Driver.Key('Shift_L', $true)
        }
        $Driver.Key($name, $true)
        $Driver.Key($name, $false)
        if ($shifted) {
            $Driver.Key('Shift_L', $false)
        }
    }
    $Driver.Sync()
}

function New-SharePortal {
    param([Parameter(Mandatory)][string]$Destination)
    $portal = New-MockPortal
    $portal | Add-Member -NotePropertyMembers ([ordered]@{
            Destination    = $Destination
            OpenRequests   = [System.Collections.Generic.List[object]]::new()
            EmailRequests  = [System.Collections.Generic.List[object]]::new()
            PrintRequests  = [System.Collections.Generic.List[object]]::new()
            FolderRequests = [System.Collections.Generic.List[object]]::new()
            FolderParents  = [System.Collections.Generic.List[object]]::new()
            ResponseCode   = 0
        })
    $portal.Handler = ${function:Invoke-SharePortalMessage}
    $portal
}

function Complete-SharePortalRequest {
    <# Replies with the request path and answers it 0.1 s later with the current response code. #>
    param($Portal, [JiffshotE2E.IncomingCall]$Call, $Options, [System.Collections.IDictionary]$Result = @{})
    $requestPath = Get-PortalRequestPath $Call $Options
    $answer = if ($Portal.ResponseCode -eq 0) { $Result } else { @{} }
    $null = Send-PortalResponseLater $Portal $requestPath $Portal.ResponseCode $answer
    $Call.Reply('o', @($requestPath))
    $true
}

function Invoke-SharePortalMessage {
    param($Portal, [JiffshotE2E.IncomingCall]$Call)
    $body = $Call.Body
    if ($Call.Interface -eq 'org.freedesktop.DBus.Properties' -and $Call.Member -eq 'Get' -and $body[0] -eq 'org.freedesktop.portal.FileChooser') {
        $Call.Reply('v', @((New-DBusVariant 'u' ([uint32]4))))
        return $true
    }
    if ($Call.Interface -eq 'org.freedesktop.portal.OpenURI' -and $Call.Member -eq 'OpenFile') {
        $null, $handle, $options = $body
        $Portal.OpenRequests.Add(@($options, (Read-E2EHandleBytes $handle)))
        return Complete-SharePortalRequest $Portal $Call $options
    }
    if ($Call.Interface -eq 'org.freedesktop.portal.Email' -and $Call.Member -eq 'ComposeEmail') {
        $null, $options = $body
        $attachments = [System.Collections.Generic.List[object]]::new()
        foreach ($handle in $options['attachment_fds'].Value) {
            $attachments.Add((Read-E2EHandleBytes $handle))
        }
        $Portal.EmailRequests.Add(@($options, $attachments))
        return Complete-SharePortalRequest $Portal $Call $options
    }
    if ($Call.Interface -eq 'org.freedesktop.portal.Print' -and $Call.Member -eq 'Print') {
        $null, $title, $handle, $options = $body
        $Portal.PrintRequests.Add(@($title, $options, (Read-E2EHandleBytes $handle)))
        return Complete-SharePortalRequest $Portal $Call $options
    }
    if ($Call.Interface -eq 'org.freedesktop.portal.FileChooser' -and $Call.Member -eq 'OpenFile') {
        $parent, $null, $options = $body
        $Portal.FolderRequests.Add($options)
        $Portal.FolderParents.Add($parent)
        return Complete-SharePortalRequest $Portal $Call $options @{ uris = (New-DBusVariant 'as' ([string[]]@(ConvertTo-E2EFileUri $Portal.Destination))) }
    }
    Invoke-MockPortalMessage $Portal $Call
}

function Get-StatusFrame {
    param([Parameter(Mandatory)][JiffshotE2E.X11Driver]$Driver)
    $Driver.Capture(0, 480, 800, 20)
}

function Test-SameBytes {
    param([byte[]]$Left, [byte[]]$Right)
    $null -ne $Left -and $null -ne $Right -and [System.Linq.Enumerable]::SequenceEqual($Left, $Right)
}

function Get-FileNames {
    param([Parameter(Mandatory)][string]$Directory)
    , [string[]]@(Get-ChildItem -LiteralPath $Directory -Force | ForEach-Object Name | Sort-Object -CaseSensitive)
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$app = $server = $driver = $portal = $reader = $null
$work = New-E2EWorkDirectory 'jiffshot-share-ui-'
try {
    $captures = New-Item -ItemType Directory -Path (Join-Path $work 'captures')
    $source = Join-Path $captures 'saved-capture.png'
    $image = Get-E2ESolidPng 30 120 190
    [System.IO.File]::WriteAllBytes($source, $image)
    Write-E2EJson (Join-Path $captures 'saved-capture.json') ([ordered]@{
            process = 'Browser'
            url     = 'https://docs.example.test/guide'
            tags    = @('Planning')
        })
    $oldTime = [DateTime]::UtcNow.AddSeconds(-3600)
    [System.IO.File]::SetLastAccessTimeUtc($source, $oldTime)
    [System.IO.File]::SetLastWriteTimeUtc($source, $oldTime)
    [System.IO.File]::WriteAllBytes((Join-Path $captures 'newer-decoy.png'), (Get-E2ESolidPng 240 40 40))
    Write-E2EJson (Join-Path $captures 'newer-decoy.json') ([ordered]@{
            window_title = 'Browser Planning docs.example.test'
            process      = 'Editor'
            url          = 'https://other.test/docs.example.test'
            tags         = @('Reference')
        })
    $destination = New-Item -ItemType Directory -Path (Join-Path $work 'destination')
    $existing = Join-Path $destination 'saved-capture.png'
    [System.IO.File]::WriteAllBytes($existing, [System.Text.Encoding]::ASCII.GetBytes('existing file'))
    $config = New-Item -ItemType Directory -Force -Path (Join-Path $work 'config/Jiffshot')
    Write-E2EJson (Join-Path $config 'settings.json') ([ordered]@{
            capture_directory = $captures.FullName
            theme             = 'Light'
        })
    New-E2EPrivateDirectory (Join-Path $work 'runtime') | Out-Null
    $portal = New-SharePortal $destination.FullName
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $environment = New-E2EEnvironment $server.Display $work $runtime
        $environment['GTK_USE_PORTAL'] = '1'
        $app = Start-E2EProcess @($binaryPath) $environment $work $appLog
        $driver = Connect-E2EWindow $environment
        Wait-E2ESeconds 0.5
        Invoke-E2EScroll $driver 'down'
        Wait-E2ESeconds 0.5
        Write-E2ETrace $driver 'share-before'

        $previousStatus = Get-StatusFrame $driver
        Set-SearchQuery $driver 'app:browser site:example.test tag:planning age:day'
        $driver.Click(230, 214)
        Wait-E2EUntil { -not (Get-StatusFrame $driver).SameAs($previousStatus) } 'the qualified library search'
        Wait-E2ESeconds 0.5
        $driver.Click(558, 214)
        Wait-E2ESeconds 0.5
        Invoke-E2EScroll $driver 'down'
        Wait-E2ESeconds 0.5
        Write-E2ETrace $driver 'share-controls'
        $reader = New-E2ETextReader
        $buttons = @{}
        foreach ($pair in @(
                @('open', 'Open saved capture with'),
                @('email', 'Email saved capture'),
                @('copy', 'Copy saved capture to folder'),
                @('print', 'Print saved image'),
                @('capture', 'saved-capture.png'))) {
            $position = Find-E2ELabel $reader $driver $pair[1]
            Assert-E2E ($null -ne $position) "could not find '$($pair[1])' in the library"
            $buttons[$pair[0]] = $position
        }

        $previousStatus = Get-StatusFrame $driver
        $driver.Click($buttons['open'][0], $buttons['open'][1])
        Wait-E2EUntil { -not (Get-StatusFrame $driver).SameAs($previousStatus) } 'sharing without a selected capture'
        Wait-E2EStatus $reader $driver 'Select a saved capture before sharing'
        Assert-E2E ($portal.OpenRequests.Count -eq 0) 'sharing must require a saved selection'
        $driver.Click($buttons['copy'][0], $buttons['copy'][1])
        $driver.Click($buttons['print'][0], $buttons['print'][1])
        Wait-E2ESeconds 0.3
        Assert-E2E ($portal.FolderRequests.Count -eq 0 -and $portal.PrintRequests.Count -eq 0) 'copying and printing must require a saved selection'

        $driver.Click($buttons['capture'][0], $buttons['capture'][1])
        Wait-E2ESeconds 1
        Write-E2ETrace $driver 'share-selected'
        Invoke-E2EDrawArrow $driver
        Wait-E2EUntil { $driver.RedCanvasPixels() -gt 40 } 'an unsaved annotation'

        $driver.Click($buttons['open'][0], $buttons['open'][1])
        Wait-E2EUntil { $portal.OpenRequests.Count -eq 1 } 'the open-with portal'
        $options, $contents = $portal.OpenRequests[$portal.OpenRequests.Count - 1]
        Assert-E2E ($options['ask'].Value) 'open-with must ask the desktop for an application'
        Assert-E2E (Test-SameBytes $contents $image) 'open-with must receive the saved file, not editor changes'

        $driver.Click($buttons['email'][0], $buttons['email'][1])
        Wait-E2EUntil { $portal.EmailRequests.Count -eq 1 } 'the email portal'
        $options, $attachments = $portal.EmailRequests[$portal.EmailRequests.Count - 1]
        Assert-E2E ($options['subject'].Value -ceq 'Jiffshot capture') "$($options | Out-String)"
        Assert-E2E ($attachments.Count -eq 1 -and (Test-SameBytes $attachments[0] $image)) 'email must attach the selected saved file'

        $driver.Click($buttons['print'][0], $buttons['print'][1])
        Wait-E2EUntil { $portal.PrintRequests.Count -eq 1 } 'the print portal'
        $title, $null, $output = $portal.PrintRequests[$portal.PrintRequests.Count - 1]
        Assert-E2E ($title -ceq 'Jiffshot saved capture') $title
        $outputText = [System.Text.Encoding]::Latin1.GetString($output)
        Assert-E2E ($outputText.StartsWith('%PDF-1.4', [System.StringComparison]::Ordinal) -and $outputText.EndsWith("%%EOF`n", [System.StringComparison]::Ordinal)) 'the print portal must receive a complete PDF'
        Assert-E2E ($outputText.Contains('/Width 240 /Height 140')) 'the printed page must contain the selected saved image'

        $driver.Click($buttons['copy'][0], $buttons['copy'][1])
        Wait-E2EUntil { $portal.FolderRequests.Count -eq 1 } 'the folder picker'
        Assert-E2E ($portal.FolderRequests[$portal.FolderRequests.Count - 1]['directory'].Value) 'the picker must choose a folder'
        $parent = [string]$portal.FolderParents[$portal.FolderParents.Count - 1]
        if ($parent.StartsWith('x11:', [System.StringComparison]::Ordinal)) {
            $parent = $parent.Substring(4)
        }
        Assert-E2E ([Convert]::ToInt64($parent, 16) -eq $driver.Window) "folder picker parent $parent"
        $copied = Join-Path $destination 'saved-capture (2).png'
        Wait-E2EUntil { Test-Path -LiteralPath $copied } 'the copied capture'
        Assert-E2E (Test-SameBytes ([System.IO.File]::ReadAllBytes($copied)) $image) 'the copied capture must match the saved file'
        Assert-E2E ([System.IO.File]::ReadAllText($existing) -ceq 'existing file') 'copy must not overwrite an existing file'

        $portal.ResponseCode = 1
        $driver.Click($buttons['open'][0], $buttons['open'][1])
        Wait-E2EUntil { $portal.OpenRequests.Count -eq 2 } 'a cancelled open-with request'
        Wait-E2ESeconds 0.3
        $driver.Click($buttons['copy'][0], $buttons['copy'][1])
        Wait-E2EUntil { $portal.FolderRequests.Count -eq 2 } 'a cancelled folder picker'
        Wait-E2ESeconds 0.3
        $filesAfterCancel = Get-FileNames $destination
        Assert-E2E (Test-E2EDeepEqual $filesAfterCancel @('saved-capture (2).png', 'saved-capture.png')) "cancelled folder selection created unexpected files: $($filesAfterCancel -join ', ')"
        $driver.Click($buttons['print'][0], $buttons['print'][1])
        Wait-E2EUntil { $portal.PrintRequests.Count -eq 2 } 'a cancelled print request'
        Wait-E2ESeconds 0.3
        $portal.ResponseCode = 2
        $driver.Click($buttons['email'][0], $buttons['email'][1])
        Wait-E2EUntil { $portal.EmailRequests.Count -eq 2 } 'a rejected email request'
        Wait-E2ESeconds 0.3
        $previousStatus = Get-StatusFrame $driver
        $driver.Click($buttons['copy'][0], $buttons['copy'][1])
        Wait-E2EUntil { $portal.FolderRequests.Count -eq 3 } 'a failed folder picker'
        Wait-E2EUntil { -not (Get-StatusFrame $driver).SameAs($previousStatus) } 'reporting the failed folder picker'
        Assert-E2E (Test-E2EDeepEqual (Get-FileNames $destination) $filesAfterCancel) 'a failed folder picker must not create files'
        $portal.ResponseCode = 0
        $driver.Click($buttons['copy'][0], $buttons['copy'][1])
        Wait-E2EUntil { $portal.FolderRequests.Count -eq 4 } 'a folder picker after errors'
        $copiedAfterErrors = Join-Path $destination 'saved-capture (3).png'
        Wait-E2EUntil { Test-Path -LiteralPath $copiedAfterErrors } 'a copy after picker errors'
        Assert-E2E (Test-SameBytes ([System.IO.File]::ReadAllBytes($copiedAfterErrors)) $image) 'the copy after picker errors must match the saved file'

        [System.IO.File]::WriteAllBytes($source, [System.Text.Encoding]::ASCII.GetBytes('not an image'))
        $previousStatus = Get-StatusFrame $driver
        $driver.Click($buttons['print'][0], $buttons['print'][1])
        Wait-E2EUntil { -not (Get-StatusFrame $driver).SameAs($previousStatus) } 'reporting a non-image print source'
        Assert-E2E ($portal.PrintRequests.Count -eq 2) 'invalid image data must not reach print'

        Remove-Item -LiteralPath $source
        Wait-E2ESeconds 0.3
        $previousStatus = Get-StatusFrame $driver
        $driver.Click($buttons['open'][0], $buttons['open'][1])
        Wait-E2EUntil { -not (Get-StatusFrame $driver).SameAs($previousStatus) } 'reporting a missing saved capture'
        Assert-E2E ($portal.OpenRequests.Count -eq 2) 'a missing file must not reach the portal'
        Write-E2ELine 'GUI qualified library search selected the matching saved file for sharing and printing'
    }
    catch {
        if ($null -ne $driver) {
            Write-E2ETrace $driver 'share-failure'
        }
        Write-E2ELogTail @($appLog, $xvfbLog)
        throw
    }
    finally {
        if ($null -ne $reader) {
            $reader.Dispose()
        }
        Close-E2EX11Driver $driver
        Stop-E2EProcess $app
        if ($null -ne $server) {
            Stop-E2EProcess $server.Process
        }
    }
}
finally {
    Unregister-E2EService $portal
    Remove-E2EWorkDirectory $work
}
