# Windows version of ../copy-test.sh: a search ending on a display copy isn't taken as found (it
# stays listed for the player to try, with the reason). Food is a 16-bit short (left out of the
# search here with `types`) with a float copy for the HUD that the game refreshes every tick, so
# typed numbers for food end on the copy, which a test write can't change. Gold (a plain int) must
# still be found, and the test write on it undone. Linux keeps the heap match (its stack is at
# 0x7ff...); here the HUD copy is the one float match, the stack leftovers whole numbers.
. "$PSScriptRoot\common.ps1"
Invoke-Test 'copy-test' {
    $g = Start-Game
    Start-Live
    Live "attach $($g.Id)" -Quiet | Out-Null
    Live 'types i32,f32,f64,xor' -Quiet | Out-Null
    Write-Host '--- food: only its display copy can be found'
    $food = 500
    $r = Live 'type 500'
    if (-not (Has $r 'put a test value back.*Kept: 1 candidate left')) {
        Game 'eat 7'
        Live 'type 507' | Out-Null
        $copy = Match-List | Where-Object { $_ -match ':f32$' } | Select-Object -First 1
        if (-not $copy) { Fail 'no float match for the HUD copy' }
        Live "keep $(($copy -split ':')[0])" | Out-Null
        Game 'eat 3'
        $food = 510
        $r = Live 'type 510'
    }
    if (-not (Has $r 'put a test value back.*Kept: 1 candidate left')) { Fail 'the copy was accepted' }
    Game 'show'
    Expect "food=$food "
    Write-Host '--- gold: a real value is still found'
    Live 'reset' -Quiet | Out-Null
    Live 'types i32,f32,f64,xor,u16' -Quiet | Out-Null
    Live 'type 1000' | Out-Null
    Game 'earn 5'
    Live 'type 1005' | Out-Null
    Keep-RealMatch 'gold' 1005 | Out-Null
    Game 'earn 7'
    $r = Live 'type 1012'
    if (-not (Has $r 'found it')) { Fail 'gold not found' }
    Game 'show'
    Expect 'gold=1012 '
}
