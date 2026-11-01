#!/usr/bin/env pwsh
#Requires -Version 7.4
<#
.SYNOPSIS
Verify automatic capture copies through the Linux editor and desktop portals.
#>
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$RuntimeDir
)

Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.Editor.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'LinuxE2E.Desktop.psm1') -Force

$Setting = 'linux_auto_copy_directory'
$PickerTitle = 'Choose folder for automatic capture copies'
$FrozenScreenshot = 'Screenshot-1700000000-123456789.png'

function Get-ScreenshotFiles {
    param([Parameter(Mandatory)][string]$Directory)
    , [string[]]@([System.IO.Directory]::GetFiles($Directory, 'Screenshot-*.png'))
}

function Get-PngFiles {
    param([Parameter(Mandatory)][string]$Directory)
    , [string[]]@([System.IO.Directory]::GetFiles($Directory, '*.png'))
}

function Get-AddedFiles {
    param([string[]]$After, [string[]]$Before)
    , [string[]]@($After | Where-Object { $Before -cnotcontains $_ })
}

function Test-SameFileSet {
    param([string[]]$Left, [string[]]$Right)
    $leftSorted = @($Left | Sort-Object -CaseSensitive)
    $rightSorted = @($Right | Sort-Object -CaseSensitive)
    ($leftSorted -join "`n") -ceq ($rightSorted -join "`n")
}

function Test-PngComplete {
    param([Parameter(Mandatory)][string]$Path)
    if (-not [System.IO.File]::Exists($Path)) {
        return $false
    }
    $data = [System.IO.File]::ReadAllBytes($Path)
    $header = [byte[]](0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A)
    $trailer = [byte[]](0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82)
    if ($data.Length -lt $header.Length) {
        return $false
    }
    [System.Linq.Enumerable]::SequenceEqual([byte[]]$data[0..($header.Length - 1)], $header) -and (Test-E2EFileEnd $Path $trailer)
}

function Test-SameFileBytes {
    param([Parameter(Mandatory)][string]$Left, [Parameter(Mandatory)][string]$Right)
    [System.Linq.Enumerable]::SequenceEqual([System.IO.File]::ReadAllBytes($Left), [System.IO.File]::ReadAllBytes($Right))
}

function Get-FolderSnapshot {
    <# Maps each entry name in a folder to its base64 contents, like {path.name: path.read_bytes()}. #>
    param([Parameter(Mandatory)][string]$Directory)
    $snapshot = [ordered]@{}
    foreach ($path in [System.IO.Directory]::GetFileSystemEntries($Directory)) {
        $snapshot[[System.IO.Path]::GetFileName($path)] = [Convert]::ToBase64String([System.IO.File]::ReadAllBytes($path))
    }
    $snapshot
}

function Get-Preferences {
    Read-E2EJson $fixture.Settings
}

function Set-Preferences {
    param([AllowNull()][string]$Directory)
    $value = Copy-E2EMap $fixture.Original
    if ($Directory) {
        $value[$Setting] = $Directory
    }
    Write-E2EJson $fixture.Settings $value
}

function Get-ExpectedPreferences {
    param([Parameter(Mandatory)][string]$Directory)
    Copy-E2EMap $fixture.Original @{ $Setting = $Directory }
}

function Close-App {
    if ($null -ne $fixture.Driver) {
        Close-E2EX11Driver $fixture.Driver
        $fixture.Driver = $null
    }
    Stop-E2EProcess $fixture.App
    $fixture.App = $null
    Wait-E2EUntil { $null -eq (Get-E2EWindowInfo $fixture.Environment) } 'the previous editor to close'
}

function Start-App {
    param([string[]]$Arguments = @(), [System.Collections.IDictionary]$ExtraEnvironment = @{})
    if ($null -ne $fixture.App) {
        Close-App
    }
    $environment = Copy-E2EMap $fixture.Environment $ExtraEnvironment
    $fixture.App = Start-E2EProcess (@($fixture.Binary) + $Arguments) $environment $fixture.Work $fixture.AppLog
    $fixture.Driver = Connect-E2EWindow $fixture.Environment 'the editor' 15
    Wait-E2ESeconds 0.4
}

function Invoke-FindAndClick {
    param([Parameter(Mandatory)][string]$Label, [Parameter(Mandatory)][string]$Direction)
    for ($attempt = 0; $attempt -lt 14; $attempt++) {
        $position = Find-E2ELabel $fixture.Text $fixture.Driver $Label
        if ($null -ne $position) {
            $fixture.Driver.Click($position[0], $position[1])
            return
        }
        Invoke-E2EScroll $fixture.Driver $Direction 2
        Wait-E2ESeconds 0.15
    }
    Write-E2ETrace $fixture.Driver 'automatic-copy-label-not-found'
    $observed = $fixture.Text.Recognize($fixture.Driver.Capture(0, 0, 800, 270), 6, $false)
    Assert-E2E $false "could not find '$Label' in the controls; OCR saw '$observed'"
}

function Show-Organization {
    Invoke-E2EScroll $fixture.Driver 'down'
    Wait-E2ESeconds 0.4
    Invoke-FindAndClick 'Organize captures' 'down'
    Wait-E2ESeconds 0.3
    Invoke-E2EScroll $fixture.Driver 'down'
    Wait-E2ESeconds 0.3
    Write-E2ETrace $fixture.Driver 'automatic-copy-controls'
}

function Start-FolderPicker {
    param([switch]$FailMethod)
    $previous = $portal.OpenRequests.Count
    $portal.FailNextOpen = [bool]$FailMethod
    Invoke-FindAndClick 'Auto-copy new captures' 'down'
    Wait-E2EUntil { $portal.OpenRequests.Count -eq $previous + 1 } 'the automatic-copy folder chooser'
    $title, $options, $null = $portal.OpenRequests[$previous]
    Assert-E2E ($title -ceq $PickerTitle) $title
    Assert-E2E ($options['directory'].Value -eq $true) 'the chooser must select a directory'
    Assert-E2E ((ConvertFrom-E2EWindowHandle $portal.OpenParents[$previous]) -eq $fixture.Driver.Window) 'the folder chooser must be parented to the editor'
    Wait-E2EStatus $fixture.Text $fixture.Driver 'Choose a folder for automatic'
    $previous
}

function Complete-FolderPicker {
    param([Parameter(Mandatory)][int]$Index, [Parameter(Mandatory)][uint32]$Code, [string]$Uri)
    $null = Complete-EffectsPortalOpen $portal $Index $Code $Uri
}

function Select-Folder {
    param([Parameter(Mandatory)][string]$Directory)
    $index = Start-FolderPicker
    Complete-FolderPicker $index 0 (ConvertTo-E2EFileUri $Directory)
    Wait-E2EUntil { (Get-Preferences)[$Setting] -ceq $Directory } 'the selected automatic-copy preference'
}

function Assert-InteractiveScreenshot {
    param($Options)
    Assert-E2E ($Options['interactive'].Value -eq $true -and $Options['modal'].Value -eq $true) "$($Options | Out-String)"
}

function Start-Screenshot {
    $previous = $portal.ScreenshotRequests.Count
    Invoke-E2EScroll $fixture.Driver 'down'
    Wait-E2ESeconds 0.3
    Invoke-FindAndClick 'Desktop screenshot chooser' 'up'
    Wait-E2EUntil { $portal.ScreenshotRequests.Count -eq $previous + 1 } 'the desktop screenshot chooser'
    $requestPath, $options = $portal.ScreenshotRequests[$previous]
    Assert-InteractiveScreenshot $options
    Wait-E2EStatus $fixture.Text $fixture.Driver 'Choose a screenshot'
    $requestPath
}

function Start-ScreenshotApp {
    param([System.Collections.IDictionary]$ExtraEnvironment = @{})
    $previous = $portal.ScreenshotRequests.Count
    Start-App @('--capture-interactive') $ExtraEnvironment
    Wait-E2EUntil { $portal.ScreenshotRequests.Count -eq $previous + 1 } 'the launched screenshot chooser'
    $requestPath, $options = $portal.ScreenshotRequests[$previous]
    Assert-InteractiveScreenshot $options
    $requestPath
}

function Complete-Screenshot {
    param([Parameter(Mandatory)][string]$RequestPath, [uint32]$Code = 0, [string]$Uri)
    $result = if ($Code -ne 0) { @{} } else {
        @{ uri = (New-DBusVariant 's' ($Uri ? $Uri : (ConvertTo-E2EFileUri $fixture.Source))) }
    }
    $null = Send-PortalResponseLater $portal $RequestPath $Code $result
}

function Wait-SavedScreenshot {
    param([string[]]$Previous)
    Wait-E2EUntil {
        $added = Get-AddedFiles (Get-ScreenshotFiles $fixture.Captures) $Previous
        $added.Count -eq 1 -and (Test-PngComplete $added[0])
    } 'a completed original screenshot' 15
    $added = Get-AddedFiles (Get-ScreenshotFiles $fixture.Captures) $Previous
    Assert-E2E ($added.Count -eq 1) "expected one new original, got $($added -join ', ')"
    $path = $added[0]
    Assert-E2E (((Read-E2EPng $path).Pixel(150, 100) -join ',') -eq '255,0,0,255') 'the saved capture must contain the selected red screenshot'
    $path
}

function Wait-CopiedScreenshot {
    param([Parameter(Mandatory)][string]$Original, [Parameter(Mandatory)][string]$Directory, [string[]]$Previous)
    Wait-E2EUntil {
        $added = Get-AddedFiles (Get-PngFiles $Directory) $Previous
        $added.Count -eq 1 -and (Test-SameFileBytes $added[0] $Original)
    } "one byte-identical copy in $Directory" 15
    $added = Get-AddedFiles (Get-PngFiles $Directory) $Previous
    Assert-E2E ($added.Count -eq 1) "expected one new copy, got $($added -join ', ')"
    $copied = $added[0]
    Assert-E2E (Test-SameFileBytes $copied $Original) 'copy bytes must equal saved bytes'
    $copied
}

function Invoke-Capture {
    <# Captures through the editor's screenshot chooser; returns @(original, copy or $null). #>
    param([string]$Directory)
    $beforeSource = Get-ScreenshotFiles $fixture.Captures
    $beforeCopy = if ($Directory) { Get-PngFiles $Directory } else { [string[]]@() }
    $requestPath = Start-Screenshot
    Complete-Screenshot $requestPath
    $original = Wait-SavedScreenshot $beforeSource
    $copied = if ($Directory) { Wait-CopiedScreenshot $original $Directory $beforeCopy } else { $null }
    Wait-E2EStatus $fixture.Text $fixture.Driver 'Desktop screenshot captured'
    @($original, $copied)
}

function Invoke-Case {
    param([Parameter(Mandatory)][string]$Name, [Parameter(Mandatory)][scriptblock]$Body)
    & $Body
    Write-E2EPass $Name
}

$binaryPath = Resolve-E2EPath $Binary
$runtime = Resolve-E2EPath $RuntimeDir
$portal = New-EffectsPortal
$portal.ScreenshotMode = 'hold'
$server = $fixture = $reader = $null
$work = New-E2EWorkDirectory 'jiffshot-auto-copy-ui-'
try {
    New-E2EPrivateDirectory (Join-Path $work 'runtime') | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $work 'config/Jiffshot') | Out-Null
    $captures = (New-Item -ItemType Directory -Path (Join-Path $work 'captures')).FullName
    $first, $second, $collision = foreach ($name in @('first', 'second', 'collision')) {
        (New-Item -ItemType Directory -Path (Join-Path $work $name)).FullName
    }
    $clockLibrary = Join-Path $work 'frozen-realtime.so'
    & cc -shared -fPIC -Wall -Wextra -Werror (Join-Path $PSScriptRoot 'linux_frozen_realtime.c') -o $clockLibrary
    if ($LASTEXITCODE -ne 0) {
        throw "cc failed with exit code $LASTEXITCODE"
    }
    $xvfbLog = Join-Path $work 'xvfb.log'
    $appLog = Join-Path $work 'app.log'
    try {
        $server = Start-E2EXvfb $xvfbLog
        $environment = New-E2EEnvironment $server.Display $work $runtime
        $environment['GTK_USE_PORTAL'] = '1'
        $reader = New-E2ETextReader
        $fixture = [pscustomobject]@{
            Binary       = $binaryPath
            Work         = $work
            Text         = $reader
            Environment  = $environment
            AppLog       = $appLog
            App          = $null
            Driver       = $null
            Settings     = Join-Path $work 'config/Jiffshot/settings.json'
            Captures     = $captures
            Source       = Join-Path $work 'desktop.png'
            EditorSource = Join-Path $work 'editor.png'
            Original     = [ordered]@{
                theme             = 'Light'
                capture_directory = $captures
                copy_to_clipboard = $false
                other             = [ordered]@{ keep = @('unrelated', 7) }
            }
        }
        [System.IO.File]::WriteAllBytes($fixture.Source, (Get-E2ESolidPng 255 0 0))
        [System.IO.File]::WriteAllBytes($fixture.EditorSource, (Get-E2ESolidPng 30 120 190))

        Invoke-Case 'test_default_disabled_does_not_copy' {
            Set-Preferences
            $before = Get-ScreenshotFiles $fixture.Captures
            $requestPath = Start-ScreenshotApp
            Complete-Screenshot $requestPath
            $null = Wait-SavedScreenshot $before
            Wait-E2EStatus $fixture.Text $fixture.Driver 'Desktop screenshot captured'
            Assert-E2E (-not (Get-Preferences).Contains($Setting)) 'automatic copying must be absent by default'
            Assert-E2E ((Get-FolderSnapshot $first).Count -eq 0) 'a default-disabled capture must not create copies'
        }

        Invoke-Case 'test_folder_selection_persists_unrelated_preferences' {
            Set-Preferences
            Start-App @($fixture.EditorSource)
            Show-Organization
            Select-Folder $first
            Assert-E2E (Test-E2EDeepEqual (Get-Preferences) (Get-ExpectedPreferences $first)) "$(Get-Preferences | ConvertTo-Json -Compress)"
            Assert-E2E ([System.IO.Path]::IsPathRooted($first)) 'the chosen local folder must be an absolute path'
        }

        Invoke-Case 'test_screenshot_copy_is_byte_identical' {
            $original, $copied = Invoke-Capture $first
            Assert-E2E ([System.IO.Path]::GetFileName($copied) -ceq [System.IO.Path]::GetFileName($original)) 'without a collision, preserve the capture filename'
        }

        Invoke-Case 'test_live_destination_change_uses_new_folder' {
            $oldFiles = Get-FolderSnapshot $first
            Select-Folder $second
            Assert-E2E (Test-E2EDeepEqual (Get-Preferences) (Get-ExpectedPreferences $second)) "$(Get-Preferences | ConvertTo-Json -Compress)"
            $original, $copied = Invoke-Capture $second
            Assert-E2E (Test-SameFileBytes $copied $original) 'copy bytes must equal saved bytes'
            Assert-E2E (Test-E2EDeepEqual (Get-FolderSnapshot $first) $oldFiles) 'changing destinations must leave the previous folder unchanged'
        }

        Invoke-Case 'test_persistence_and_disable_apply_to_later_captures' {
            $beforeOriginals = Get-ScreenshotFiles $fixture.Captures
            $beforeSecond = Get-PngFiles $second
            $requestPath = Start-ScreenshotApp
            Complete-Screenshot $requestPath
            $original = Wait-SavedScreenshot $beforeOriginals
            $null = Wait-CopiedScreenshot $original $second $beforeSecond
            Wait-E2EStatus $fixture.Text $fixture.Driver 'Desktop screenshot captured'
            Assert-E2E (Test-E2EDeepEqual (Get-Preferences) (Get-ExpectedPreferences $second)) "$(Get-Preferences | ConvertTo-Json -Compress)"

            Show-Organization
            Invoke-FindAndClick 'Disable auto-copy' 'down'
            Wait-E2EUntil { -not (Get-Preferences).Contains($Setting) } 'disabling auto-copy'
            Assert-E2E (Test-E2EDeepEqual (Get-Preferences) $fixture.Original) "$(Get-Preferences | ConvertTo-Json -Compress)"
            $snapshots = @((Get-FolderSnapshot $first), (Get-FolderSnapshot $second))
            $null = Invoke-Capture
            Assert-E2E (Test-E2EDeepEqual @((Get-FolderSnapshot $first), (Get-FolderSnapshot $second)) $snapshots) 'disabling must immediately stop copying'

            $beforeOriginals = Get-ScreenshotFiles $fixture.Captures
            $requestPath = Start-ScreenshotApp
            Complete-Screenshot $requestPath
            $null = Wait-SavedScreenshot $beforeOriginals
            Wait-E2EStatus $fixture.Text $fixture.Driver 'Desktop screenshot captured'
            Assert-E2E (Test-E2EDeepEqual (Get-Preferences) $fixture.Original) "$(Get-Preferences | ConvertTo-Json -Compress)"
            Assert-E2E (Test-E2EDeepEqual @((Get-FolderSnapshot $first), (Get-FolderSnapshot $second)) $snapshots) 'the disabled preference must survive a restart'
        }

        Invoke-Case 'test_folder_chooser_cancellation_and_errors_preserve_setting' {
            Set-Preferences $first
            Start-App @($fixture.EditorSource)
            Show-Organization
            $expected = Get-ExpectedPreferences $first

            $index = Start-FolderPicker
            Complete-FolderPicker $index 1
            Wait-E2EStatus $fixture.Text $fixture.Driver 'cancelled'
            Assert-E2E (Test-E2EDeepEqual (Get-Preferences) $expected) 'cancellation must preserve all preferences'

            $index = Start-FolderPicker
            Complete-FolderPicker $index 2
            Wait-E2EStatus $fixture.Text $fixture.Driver 'Could not set'
            Assert-E2E (Test-E2EDeepEqual (Get-Preferences) $expected) 'a failed response must preserve preferences'

            $index = Start-FolderPicker -FailMethod
            Send-EffectsPortalOpenError $portal $index
            Wait-E2EStatus $fixture.Text $fixture.Driver 'Could not set'
            Assert-E2E (Test-E2EDeepEqual (Get-Preferences) $expected) 'a chooser method error must preserve preferences'

            $index = Start-FolderPicker
            Complete-FolderPicker $index 0 'https://example.invalid/not-a-local-folder'
            Wait-E2EStatus $fixture.Text $fixture.Driver 'Could not set'
            Assert-E2E (Test-E2EDeepEqual (Get-Preferences) $expected) 'a non-local URI must preserve preferences'
        }

        Invoke-Case 'test_stale_folder_chooser_response_cannot_reenable_after_disable' {
            Set-Preferences $first
            Start-App @($fixture.EditorSource)
            Show-Organization
            $index = Start-FolderPicker
            Invoke-FindAndClick 'Disable auto-copy' 'down'
            Wait-E2EUntil { -not (Get-Preferences).Contains($Setting) } 'disabling an open chooser'
            Wait-E2EStatus $fixture.Text $fixture.Driver 'disabled'

            $beforeFirst = Get-FolderSnapshot $first
            $beforeSecond = Get-FolderSnapshot $second
            $responsePath = $portal.OpenRequests[$index][2]
            Send-DBusSignal $portal $responsePath $PortalRequestInterface 'Response' 'ua{sv}' @([uint32]0, @{ uris = (New-DBusVariant 'as' ([string[]]@(ConvertTo-E2EFileUri $second))) })
            # Completing a later capture gives the application an observable progress
            # barrier after the stale response has been sent.
            $null = Invoke-Capture
            Assert-E2E (Test-E2EDeepEqual (Get-Preferences) $fixture.Original) 'a stale choice must not re-enable copying'
            Assert-E2E (Test-E2EDeepEqual (Get-FolderSnapshot $first) $beforeFirst) 'the stale choice changed the first folder'
            Assert-E2E (Test-E2EDeepEqual (Get-FolderSnapshot $second) $beforeSecond) 'the stale choice changed the second folder'
        }

        Invoke-Case 'test_cancelled_and_failed_screenshots_do_not_copy' {
            Set-Preferences $first
            Start-App @($fixture.EditorSource)
            $beforeSource = Get-ScreenshotFiles $fixture.Captures
            $beforeCopy = Get-FolderSnapshot $first
            foreach ($pair in @(@(1, 'cancelled'), @(2, 'failed'))) {
                $code, $expected = $pair
                $requestPath = Start-Screenshot
                Complete-Screenshot $requestPath $code
                Wait-E2EStatus $fixture.Text $fixture.Driver $expected
                Assert-E2E (Test-SameFileSet (Get-ScreenshotFiles $fixture.Captures) $beforeSource) "portal response $code must not save a capture"
                Assert-E2E (Test-E2EDeepEqual (Get-FolderSnapshot $first) $beforeCopy) "portal response $code must not create a copy"
            }
            $requestPath = Start-Screenshot
            Complete-Screenshot $requestPath -Uri 'https://example.invalid/screenshot.png'
            Wait-E2EStatus $fixture.Text $fixture.Driver 'non-local'
            Assert-E2E (Test-SameFileSet (Get-ScreenshotFiles $fixture.Captures) $beforeSource) 'a non-local screenshot must not save a capture'
            Assert-E2E (Test-E2EDeepEqual (Get-FolderSnapshot $first) $beforeCopy) 'a non-local screenshot must not create a copy'
        }

        Invoke-Case 'test_failed_screenshot_save_does_not_copy' {
            $unavailableCaptureDirectory = Join-Path $fixture.Work 'not-a-capture-directory'
            [System.IO.File]::WriteAllBytes($unavailableCaptureDirectory, [System.Text.Encoding]::ASCII.GetBytes('keep this file'))
            $settings = Copy-E2EMap $fixture.Original ([ordered]@{
                    capture_directory = $unavailableCaptureDirectory
                    $Setting          = $first
                })
            Write-E2EJson $fixture.Settings $settings
            $beforeCopy = Get-FolderSnapshot $first
            $requestPath = Start-ScreenshotApp
            Complete-Screenshot $requestPath
            Wait-E2EStatus $fixture.Text $fixture.Driver 'could not save'
            Assert-E2E ([System.IO.File]::ReadAllText($unavailableCaptureDirectory) -ceq 'keep this file') 'the file at the capture directory path must be kept'
            Assert-E2E (Test-E2EDeepEqual (Get-Preferences) $settings) "$(Get-Preferences | ConvertTo-Json -Compress)"
            Assert-E2E (Test-E2EDeepEqual (Get-FolderSnapshot $first) $beforeCopy) 'a screenshot which could not be saved must not be copied'
        }

        Invoke-Case 'test_unsaved_editor_changes_do_not_copy' {
            Set-Preferences $first
            Start-App @($fixture.EditorSource)
            $beforeSource = Get-ScreenshotFiles $fixture.Captures
            $beforeCopy = Get-FolderSnapshot $first
            Wait-E2EUntil { (Measure-E2ECanvasColor $fixture.Driver 30 120 190) -gt 5000 } 'the blue editor document'
            $redBefore = $fixture.Driver.RedCanvasPixels()
            Invoke-E2EDrawArrow $fixture.Driver
            Wait-E2EUntil { $fixture.Driver.RedCanvasPixels() -gt $redBefore + 40 } 'an unsaved red annotation'
            Wait-E2EStatus $fixture.Text $fixture.Driver 'Document changed'
            Assert-E2E (Test-SameFileSet (Get-ScreenshotFiles $fixture.Captures) $beforeSource) 'editing a document must not save a capture'
            Assert-E2E (Test-E2EDeepEqual (Get-FolderSnapshot $first) $beforeCopy) 'editing a document must not copy an unsaved change'
            $saved, $copied = Invoke-Capture $first
            $added = @([System.IO.Directory]::GetFileSystemEntries($first) | Where-Object { -not $beforeCopy.Contains([System.IO.Path]::GetFileName($_)) })
            Assert-E2E ($added.Count -eq 1 -and $added[0] -ceq $copied -and (Test-SameFileBytes $copied $saved)) 'only the subsequently saved screenshot may be copied'
        }

        Invoke-Case 'test_collision_never_overwrites_existing_files' {
            Set-Preferences $collision
            $sentinel = Join-Path $collision $FrozenScreenshot
            $secondCopy = Join-Path $collision "$([System.IO.Path]::GetFileNameWithoutExtension($FrozenScreenshot)) (2)$([System.IO.Path]::GetExtension($FrozenScreenshot))"
            [System.IO.File]::WriteAllBytes($sentinel, [System.Text.Encoding]::ASCII.GetBytes('first existing file'))
            [System.IO.File]::WriteAllBytes($secondCopy, [System.Text.Encoding]::ASCII.GetBytes('second existing file'))
            $beforeOriginals = Get-ScreenshotFiles $fixture.Captures
            $beforeCopies = Get-PngFiles $collision
            $requestPath = Start-ScreenshotApp @{ LD_PRELOAD = $clockLibrary }
            Complete-Screenshot $requestPath
            $original = Wait-SavedScreenshot $beforeOriginals
            Assert-E2E ([System.IO.Path]::GetFileName($original) -ceq $FrozenScreenshot) "the test clock must make the pre-existing names actual collisions: $([System.IO.Path]::GetFileName($original))"
            $copied = Wait-CopiedScreenshot $original $collision $beforeCopies
            Wait-E2EStatus $fixture.Text $fixture.Driver 'Desktop screenshot captured'
            Assert-E2E ($copied -cne $sentinel -and $copied -cne $secondCopy) "the copy reused an existing name: $copied"
            Assert-E2E ([System.IO.File]::ReadAllText($sentinel) -ceq 'first existing file') 'the first existing file was overwritten'
            Assert-E2E ([System.IO.File]::ReadAllText($secondCopy) -ceq 'second existing file') 'the second existing file was overwritten'
        }

        Invoke-Case 'test_missing_folder_reports_error_and_preserves_original' {
            $destination = Join-Path $fixture.Work 'removed-destination'
            $null = [System.IO.Directory]::CreateDirectory($destination)
            Set-Preferences $destination
            [System.IO.Directory]::Delete($destination)
            $before = Get-ScreenshotFiles $fixture.Captures
            $requestPath = Start-ScreenshotApp
            Complete-Screenshot $requestPath
            $original = Wait-SavedScreenshot $before
            Wait-E2EStatus $fixture.Text $fixture.Driver 'automatic copy failed'
            Assert-E2E (Test-PngComplete $original) 'an automatic-copy error must not delete the original'
            Assert-E2E (-not (Test-Path -LiteralPath $destination)) 'a missing destination must not be silently recreated'
        }

        Invoke-Case 'test_capture_folder_destination_reports_error_without_duplicate' {
            Set-Preferences $fixture.Captures
            $before = Get-ScreenshotFiles $fixture.Captures
            $requestPath = Start-ScreenshotApp
            Complete-Screenshot $requestPath
            $original = Wait-SavedScreenshot $before
            Wait-E2EStatus $fixture.Text $fixture.Driver 'automatic copy failed'
            Assert-E2E (Test-PngComplete $original) 'an automatic-copy error must not delete the original'
            Assert-E2E (Test-SameFileSet (Get-AddedFiles (Get-ScreenshotFiles $fixture.Captures) $before) @($original)) 'using the capture folder as destination must not create a duplicate'
        }

        Invoke-Case 'test_invalid_auto_copy_setting_reports_error_and_preserves_original' {
            Write-E2EJson $fixture.Settings (Copy-E2EMap $fixture.Original @{ $Setting = 42 })
            $expected = Get-Preferences
            $beforeSource = Get-ScreenshotFiles $fixture.Captures
            $beforeCopy = Get-FolderSnapshot $first
            $requestPath = Start-ScreenshotApp
            Complete-Screenshot $requestPath
            $original = Wait-SavedScreenshot $beforeSource
            Wait-E2EStatus $fixture.Text $fixture.Driver 'automatic copy failed'
            Assert-E2E (Test-PngComplete $original) 'an invalid preference must not delete the original'
            Assert-E2E (Test-E2EDeepEqual (Get-Preferences) $expected) 'capturing must not silently rewrite invalid settings'
            Assert-E2E (Test-E2EDeepEqual (Get-FolderSnapshot $first) $beforeCopy) 'an invalid preference must not create a copy'
        }

        Invoke-Case 'test_capture_folder_alias_reports_error_without_duplicate' {
            $alias = Join-Path $fixture.Work 'captures-alias'
            $null = [System.IO.Directory]::CreateSymbolicLink($alias, $fixture.Captures)
            Set-Preferences $alias
            $before = Get-ScreenshotFiles $fixture.Captures
            $requestPath = Start-ScreenshotApp
            Complete-Screenshot $requestPath
            $original = Wait-SavedScreenshot $before
            Wait-E2EStatus $fixture.Text $fixture.Driver 'automatic copy failed'
            Assert-E2E (Test-PngComplete $original) 'an automatic-copy error must not delete the original'
            Assert-E2E (Test-SameFileSet (Get-AddedFiles (Get-ScreenshotFiles $fixture.Captures) $before) @($original)) 'an alias of the capture folder must not create a duplicate'
        }
    }
    catch {
        if ($null -ne $fixture -and $null -ne $fixture.Driver) {
            Write-E2ETrace $fixture.Driver 'automatic-copy-failure'
        }
        Write-E2ELogTail @($appLog, $xvfbLog)
        throw
    }
    finally {
        if ($null -ne $fixture) {
            Close-E2EX11Driver $fixture.Driver
            Stop-E2EProcess $fixture.App
        }
        if ($null -ne $reader) {
            $reader.Dispose()
        }
        if ($null -ne $server) {
            Stop-E2EProcess $server.Process
        }
    }
}
finally {
    Unregister-E2EService $portal
    Remove-E2EWorkDirectory $work
}
