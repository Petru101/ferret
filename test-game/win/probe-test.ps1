# Windows version of ../probe-test.sh: telling the real value from copies when the screen never
# shows a test write (Lumencraft only redraws a number when it changes it itself): every
# candidate gets its own test value, the game changes the number, and the one that carried on
# from its test value is the real one. The CLI watches no screen, so the first check can't tell.
# Gold must be found and keep what was earned meanwhile.
. "$PSScriptRoot\common.ps1"
Invoke-Test 'probe-test' {
    $g = Start-Game
    Start-Live
    Live "attach $($g.Id)" -Quiet | Out-Null
    Live 'type 1000' | Select-Object -Last 1 | Out-Null
    Game 'earn 5'
    Live 'type 1005' | Select-Object -Last 1 | Out-Null
    # Gold's stack copies follow a test write, which already settles it: pair gold with a decoy
    # holding the same number that nothing follows (hp, the int before it).
    $gold = Find-Real 'gold' 1005
    if (-not $gold) { Fail 'gold not among the matches' }
    $hp = '{0:x}' -f ([Convert]::ToInt64($gold, 16) - 4)
    Live "write $hp 1005" | Out-Null
    Live "track $gold $hp" | Out-Null
    # The same places left: the check runs, and waits for the game.
    $waiting = $false
    foreach ($i in 1..4) {
        $start = Send-Live 'type 1005'
        $r = Wait-Live $start 'waiting up to|^found it|^error|candidates left'
        if (Has $r 'waiting up to') { $waiting = $true; break }
        if (Has $r '^found it|^error') { $r | ForEach-Object { Write-Host $_ }; Fail 'settled without the in-game check' }
        Wait-Done $start -Quiet | Out-Null
    }
    if (-not $waiting) { Fail 'the in-game check never waited for the game' }
    Game 'earn 3'
    $r = Wait-Done $start
    if (-not (Has $r '^found it')) { Fail 'gold not found' }
    Game 'show'
    Expect 'gold=1008 '
}
