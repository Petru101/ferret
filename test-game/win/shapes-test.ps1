# Windows version of ../shapes-test.sh: the smart scan. After finding one inventory stack,
# Ferret learns the memory around it, and searches for other stacks look in places shaped like it
# first (the item records are alike): the chest's logs and a new stack are the only shaped place
# holding their number from the first scan, and a lone shaped place must follow one change
# before it counts as found. Linux picks the heap match over stack leftovers by address (0x7ff...);
# here the game is asked which match is real.
. "$PSScriptRoot\common.ps1"

# Finds `n`; when leftovers still match too, tracks the one the game reports as `name`.
function Find-N([string]$N, [string]$Name) {
    $r = Live "type $N" -Quiet
    Write-Host ($r | Select-Object -Last 1)
    if (Has $r '^found it') { return }
    $at = Find-Real $Name $N
    if (-not $at) { Fail "no match is $Name" }
    Live "track $at" -Quiet | Out-Null
    if (-not (Has (Live "type $N" -Quiet) '^found it')) { Fail "$at isn't it" }
}

Invoke-Test 'shapes-test' {
    $g = Start-Game
    Start-Live
    Live "attach $($g.Id)" -Quiet | Out-Null
    Write-Host '--- logs in the inventory: learns the shape'
    Live 'type 33' | Select-Object -Last 1 | Out-Null
    Game 'logs 4'
    Find-N 37 'logs'
    if (-not ((Test-Path $script:Shapes) -and (Get-Item $script:Shapes).Length -gt 0)) { Fail 'no shape saved' }
    Get-Content $script:Shapes | ForEach-Object { Write-Host $_ }

    Write-Host '--- logs in the chest: one shaped place from the first number, found once it follows'
    Live 'reset' -Quiet | Out-Null
    $r = Live 'type 50'
    if (-not (Has $r 'shaped like earlier finds')) { Fail "the scan didn't prefer shaped places" }
    if (-not (Has $r '^1 candidates left')) { Fail 'a lone shaped place counted as found before it followed a change' }
    Game 'chest 6'
    Find-N 56 'chestlogs'

    Write-Host '--- a new stack: in a place shaped like the others'
    Game 'stack 71'
    Live 'reset' -Quiet | Out-Null
    $r = Live 'type 71'
    if (-not (Has $r 'shaped like earlier finds')) { Fail "the scan didn't prefer shaped places" }
    if (-not (Has $r '^1 candidates left')) { Fail "expected the new stack's place alone" }

    Write-Host '--- gold, with a logs stack of 1000 in a shaped place: falls back to searching everywhere'
    Game 'stack 1000'
    # Undo, not Start Over: clearing a shaped search means it wasn't there, and scans then look
    # everywhere until the next find.
    Live 'undo' -Quiet | Out-Null
    Live 'type 1000' | Select-Object -Last 1 | Out-Null
    Game 'earn 5'
    $r = Live 'type 1005'
    if (-not (Has $r 'searching everywhere')) { Fail "didn't fall back" }
    Game 'earn 7'
    Find-N 1012 'gold'
}
