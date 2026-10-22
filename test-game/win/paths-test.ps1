# Windows version of ../paths-test.sh: pointer paths. Gems (a double) sits at the end of a chain
# from a static pointer (world -> room -> stats) and is only touched through shared functions,
# like GameMaker games, so save must fall back to a pointer scan. Then the paths must follow gems
# when the game moves its room to new memory, keep a limit on it across that move, and find it
# again after the game restarts (not written, the limit waits, until saving it again confirms the
# path). Then ore, saved once, must come back after a restart through the paths that start like
# gems' confirmed ones. The Linux test prints; this one checks.
#   paths-test.ps1 [target.exe|target32.exe]
param([string]$Program = 'target.exe')
. "$PSScriptRoot\common.ps1"
Invoke-Test "paths-test-$Program" {
    if ($Program -ne 'target.exe') { $script:Profile = "$script:Profiles\$Program.profile"; $script:Shapes = "$script:Profiles\$Program.shapes" }
    $g = Start-Game $Program
    Start-Live
    Live "attach $($g.Id)" | Out-Null
    Write-Host '--- gems 77'
    Live 'scan 77' -Quiet | Out-Null
    Game 'gems 5'
    Live 'next 82' -Quiet | Out-Null
    Game 'gems 4'
    Live 'next 86' -Quiet | Out-Null
    Game 'coins 3'
    Game 'gems -2'
    Live 'next 84' -Quiet | Out-Null
    Keep-RealMatch 'gems' 84 | Out-Null
    if (-not (Has (Live 'save gems') '^saved gems')) { Fail 'gems not saved' }
    if (-not ((Profile-Lines) -match '^(path|candidate) ')) { Fail 'gems saved without pointer paths' }

    Write-Host '--- the game moves gems to new memory: the paths follow it'
    Game 'newroom'
    Game 'gems 6'
    Check-Values @('gems')

    Write-Host '--- limit gems to at most 95, then go past it, move it, go past it again'
    Live 'limit gems 95' | Out-Null
    Game 'gems 20'
    Game 'show'
    Expect 'gems=95 '
    Game 'newroom'
    Game 'gems 30'
    Game 'show'
    Expect 'gems=95 '

    Write-Host '--- restart the game: the saved paths find gems again (fresh game: 77), unconfirmed: no limit'
    Stop-Game
    $g = Start-Game $Program
    Live "attach $($g.Id)" | Out-Null
    Game 'gems 1'
    Check-Values @('gems')
    Game 'gems 50'
    Game 'show'
    Expect 'gems=128 '

    Write-Host '--- find gems again and save it again: the saved paths that lead to it are kept'
    Live 'limit gems off' -Quiet | Out-Null
    Game 'gems 7'
    Live 'scan 135' -Quiet | Out-Null
    Game 'gems 1'
    Live 'next 136' -Quiet | Out-Null
    Keep-RealMatch 'gems' 136 | Out-Null
    if (-not (Has (Live 'save gems') '^saved gems')) { Fail 'gems not saved again' }

    Write-Host "--- save ore (next to gems): its unconfirmed paths that start like gems' confirmed ones are followed"
    Live 'scan 40' -Quiet | Out-Null
    Game 'ore 3'
    Live 'next 43' -Quiet | Out-Null
    Game 'ore 2'
    Live 'next 45' -Quiet | Out-Null
    Keep-RealMatch 'ore' 45 | Out-Null
    if (-not (Has (Live 'save ore') '^saved ore')) { Fail 'ore not saved' }

    Write-Host '--- restart: ore comes back without being found again'
    Stop-Game
    $g = Start-Game $Program
    Live "attach $($g.Id)" | Out-Null
    Game 'ore 5'
    Check-Values @('ore')
}
