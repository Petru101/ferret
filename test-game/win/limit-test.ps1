# Windows version of ../limit-test.sh: find gold, save it, limit it, then "respawn" (the player
# moves to a new object and the old one is freed) and check the limit finds gold again by itself.
. "$PSScriptRoot\common.ps1"
Invoke-Test 'limit-test' {
    $g = Start-Game
    Start-Live
    Live "attach $($g.Id)" | Out-Null
    Live 'scan 1000' | Out-Null
    Game 'earn 37'
    Live 'next 1037' | Out-Null
    Game 'spend 10'
    Live 'next 1027' | Out-Null
    Keep-RealMatch 'gold' 1027 | Out-Null
    if (-not (Has (Live 'save gold') '^saved gold')) { Fail 'gold not saved' }
    Live 'limit gold 1100' | Out-Null
    Game 'earn 500'
    Game 'show'
    Expect 'gold=1100 '
    Write-Host '--- respawn: new player object, old one freed'
    Game 'respawn'
    Write-Host '--- waiting for the limit to find gold again'
    Start-Sleep 8
    Live 'values' | Out-Null
    Game 'earn 500'
    Game 'show'
    Expect 'gold=1100 '
}
