#Requires -Version 7.4

BeforeAll {
    Import-Module (Join-Path $PSScriptRoot 'JiffshotLinuxSmoke.psm1') -Force
}

Describe 'Linux package smoke window detection' {
    It 'finds the editor window for the opened image' {
        $image = 'input image with spaces.png'
        $tree = @"
xwininfo: Window id: 0x21f (the root window) (has no name)
     0x200016 (has no name): ("jiffshot" "jiffshot")  960x630+10+10
     0x20000f "$image $([char]0x2014) Jiffshot Editor": ("jiffshot" "jiffshot")  1500x960+10+10
"@
        Find-JiffshotSmokeWindowId -WindowTree $tree -ImageName $image | Should -BeExactly '0x20000f'
    }

    It 'rejects <Case> windows' -ForEach @(
        @{ Case = 'the old generic title'; Title = 'Jiffshot' }
        @{ Case = 'the initial editor title'; Title = 'Jiffshot Editor' }
        @{ Case = 'an untitled editor'; Title = 'Untitled - Jiffshot Editor' }
        @{ Case = 'a different image'; Title = 'other.png - Jiffshot Editor' }
        @{ Case = 'a longer filename'; Title = 'other input image with spaces.png - Jiffshot Editor' }
        @{ Case = 'a filename with an extra suffix'; Title = 'input image with spaces.png.bak - Jiffshot Editor' }
        @{ Case = 'the capture window'; Title = 'Jiffshot Capture' }
    ) {
        $tree = "  0x20000f `"$Title`": (`"jiffshot`" `"jiffshot`") 1500x960+10+10"
        Find-JiffshotSmokeWindowId -WindowTree $tree -ImageName 'input image with spaces.png' |
            Should -BeNullOrEmpty
    }

    It 'matches image names literally when they contain regex characters' {
        $image = 'capture [1]+.png'
        $tree = "  0xABC123 `"$image - Jiffshot Editor`": (`"jiffshot`" `"jiffshot`") 1500x960+10+10"
        Find-JiffshotSmokeWindowId -WindowTree $tree -ImageName $image | Should -BeExactly '0xABC123'
        $other = '  0xABC123 "capture 1.png - Jiffshot Editor": ("jiffshot" "jiffshot") 1500x960+10+10'
        Find-JiffshotSmokeWindowId -WindowTree $other -ImageName $image | Should -BeNullOrEmpty
    }

    It 'handles an empty window tree' {
        Find-JiffshotSmokeWindowId -WindowTree '' -ImageName 'image.png' | Should -BeNullOrEmpty
    }
}
