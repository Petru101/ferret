# Windows version of ../move-test.sh: saved values follow the game when it replaces their
# objects but keeps the old ones readable (like Particle Fleet on a new map): crystals (read from
# a static pointer) must follow at once, gold (no static pointer) within the 10 s re-check, also
# with limits. The Linux test prints; this one checks values against the game.
#   move-test.ps1 [target.exe|target32.exe]
param([string]$Program = 'target.exe')
. "$PSScriptRoot\common.ps1"
Invoke-Test "move-test-$Program" {
    if ($Program -ne 'target.exe') { $script:Profile = "$script:Profiles\$Program.profile"; $script:Shapes = "$script:Profiles\$Program.shapes" }
    $g = Start-Game $Program
    Start-Live
    Live "attach $($g.Id)" | Out-Null
    Live 'scan 300' -Quiet | Out-Null
    Game 'crystals 7'
    Live 'next 307' -Quiet | Out-Null
    Game 'crystals -5'
    Live 'next 302' -Quiet | Out-Null
    Keep-RealMatch 'crystals' 302 | Out-Null
    Live 'save crystals' | Out-Null
    Live 'scan 1000' -Quiet | Out-Null
    Game 'earn 37'
    Live 'next 1037' -Quiet | Out-Null
    Game 'spend 10'
    Live 'next 1027' -Quiet | Out-Null
    Keep-RealMatch 'gold' 1027 | Out-Null
    Live 'save gold' | Out-Null
    Write-Host '--- attach again (as after a restart): restore'
    Live "attach $($g.Id)" | Out-Null
    Check-Values @('crystals', 'gold')
    Write-Host '--- newmap: new objects, old ones stay readable: crystals follows at once'
    Game 'newmap'
    Game 'crystals 4'
    Game 'earn 3'
    Check-Values @('crystals')
    Write-Host '--- after the 10 s re-check, gold too'
    Start-Sleep 11
    Check-Values @('crystals', 'gold')
    Write-Host '--- limits across another newmap'
    Live 'limit crystals - 320' | Out-Null
    Live 'limit gold - 1100' | Out-Null
    Game 'newmap'
    Game 'crystals 50'
    Start-Sleep 11
    Game 'earn 500'
    Game 'show'
    Expect 'gold=1100 '
    Expect 'crystals=320( |$)'
    Live 'values' | Out-Null
}
