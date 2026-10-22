# Windows version of ../shared-test.sh: saving a value that is only in its own heap block and
# read through shared code, like GameMaker games read every variable: coins (a double) is read by
# one function that also reads wood, and directly by the main loop. Save must skip the shared
# function, keep the direct read, and the saved pattern must find coins again. The Linux test
# prints; this one checks.
#   shared-test.ps1 [target.exe|target32.exe]
param([string]$Program = 'target.exe')
. "$PSScriptRoot\common.ps1"
Invoke-Test "shared-test-$Program" {
    if ($Program -ne 'target.exe') { $script:Profile = "$script:Profiles\$Program.profile"; $script:Shapes = "$script:Profiles\$Program.shapes" }
    $g = Start-Game $Program
    Start-Live
    Live "attach $($g.Id)" | Out-Null
    Write-Host '--- coins 30'
    Live 'scan 30' -Quiet | Out-Null
    Game 'coins 7'
    Live 'next 37' -Quiet | Out-Null
    Game 'coins 5'
    Live 'next 42' -Quiet | Out-Null
    Game 'wood 30'
    Game 'coins -4'
    Live 'next 38' -Quiet | Out-Null
    Live 'list' | Out-Null
    # With no screen to read, the probe waits for the game to change the number.
    $start = Send-Live 'probe 38'
    Start-Sleep 6
    Game 'coins 2'
    Wait-Done $start | Out-Null
    $r = Live 'save coins'
    if (-not (Has $r '^saved coins')) { Fail 'coins not saved' }
    Write-Host '--- the saved pattern finds coins again'
    Live 'restore' | Out-Null
    Check-Values @('coins')
}
