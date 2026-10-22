# Windows version of ../names-test.sh: named paths on the test game's Unity-style inventory (like
# Valheim's wood): logs are found, saved by name ("Inventory" -> every "$item_logs" stack), set and
# limited in every stack but not in the chest's, followed into a new inventory after "die", and
# found again by name after a restart.
. "$PSScriptRoot\common.ps1"
Invoke-Test 'names-test' {
    $g = Start-Game
    Start-Live
    Live "attach $($g.Id)" | Out-Null
    Write-Host '--- logs 33'
    Live 'scan 33' | Out-Null
    Game 'logs 4'
    Live 'next 37' | Out-Null
    Game 'logs 1'
    Live 'next 38' | Out-Null
    Keep-RealMatch 'logs' 38 | Out-Null
    $r = Live 'save logs'
    if (-not (Has $r '^saved logs \(by name: every "\$item_logs" in "Inventory"')) { Fail 'not saved by name' }

    Write-Host "--- a second stack: set writes both, not the chest's"
    Game 'stack 20'
    if (-not (Has (Live 'values') 'in 2 places')) { Fail "values doesn't show 2 places" }
    Live 'set logs 45' | Out-Null
    Game 'show'
    Expect 'logs=45,45 stone=9 chestlogs=50'

    Write-Host '--- limit logs to at most 40'
    Live 'limit logs 40' | Out-Null
    Game 'logs 10'
    Game 'show'
    Expect 'logs=40,40 stone=9 chestlogs=50'

    Write-Host '--- die: the stacks move to a new inventory, the limit follows them'
    Game 'die'
    Game 'logs 30'
    Start-Sleep 12
    Game 'show'
    Expect 'logs=40,40 stone=9 chestlogs=50'
    Live 'values' | Out-Null
    Live 'limit logs off' | Out-Null

    Write-Host '--- restart: found again by name (fresh game: one stack of 33)'
    Stop-Game
    $g = Start-Game
    if (-not (Has (Live "attach $($g.Id)") 'logs = 33 \(found by name')) { Fail 'not restored by name' }
    Live 'set logs 77' | Out-Null
    Game 'show'
    Expect 'logs=77 stone=9 chestlogs=50'
}
