# Windows version of ../doubt-test.sh: Ferret doesn't write through pointer paths that disagree.
# Gems is saved through pointer paths, then its profile is changed to one real path plus two
# copies of a wrong one (gems' path with the last offset +8 = ore). The vote then points at ore,
# 2 to 1: not clear. Set must refuse, and a limit must pause instead of writing ore (or gems).
# Then with the real path given three times the vote is clear again and both work. The same
# three as unconfirmed candidates of an earlier run are guesses: set must refuse.
. "$PSScriptRoot\common.ps1"

function Write-GemsProfile([string]$Kind, [string[]]$Paths) {
    $lines = @('entry gems', 'type f64', 'run 1') + ($Paths | ForEach-Object { "$Kind $_" })
    [IO.File]::WriteAllText($script:Profile, (($lines -join "`n") + "`n"))
}

Invoke-Test 'doubt-test' {
    $g = Start-Game
    Start-Live
    Live "attach $($g.Id)" -Quiet | Out-Null
    Live 'scan 77' -Quiet | Out-Null
    Game 'gems 5'
    Live 'next 82' -Quiet | Out-Null
    Game 'gems 4'
    Live 'next 86' -Quiet | Out-Null
    Keep-RealMatch 'gems' 86 | Out-Null
    if (-not (Has (Live 'save gems') '^saved gems')) { Fail 'gems not saved' }
    $real = (Profile-Lines | Where-Object { $_ -match '^candidate ' } | Select-Object -First 1) -replace '^candidate ', ''
    if (-not $real) { Fail 'no pointer path saved' }
    $parts = $real -split ' '
    $wrong = (($parts[0..($parts.Count - 2)]) + ('{0:x}' -f ([Convert]::ToInt64($parts[-1], 16) + 8))) -join ' '
    Write-Host "real path: $real   wrong path: $wrong"

    Write-Host '--- paths disagree (1 real, 2 wrong)'
    Write-GemsProfile 'path' @($real, $wrong, $wrong)
    Live 'restore' | Out-Null
    if (-not (Has (Live 'values') 'not written')) { Fail "values doesn't say it won't write" }
    if (-not (Has (Live 'set gems 500') '^error: not written: its pointer paths disagree')) { Fail 'set wrote' }
    Live 'limit gems 95' | Out-Null
    Game 'gems 20'
    if (-not (Has (Live 'values') "don't agree")) { Fail 'limit not paused' }
    Game 'show'
    Expect 'gems=106.* ore='
    if ((Last-Log) -match 'ore=95( |$)') { Fail 'the limit wrote ore' }
    Live 'limit gems off' -Quiet | Out-Null

    Write-Host '--- paths agree (3 real)'
    Write-GemsProfile 'path' @($real, $real, $real)
    Live 'restore' | Out-Null
    if (Has (Live 'set gems 90') '^error') { Fail 'set refused' }
    Game 'show'
    Expect 'gems=90'
    Live 'limit gems 95' -Quiet | Out-Null
    Game 'gems 20'
    Game 'show'
    Expect 'gems=95'
    Live 'limit gems off' -Quiet | Out-Null

    Write-Host '--- unconfirmed candidates of an earlier run (3 real): guesses, not written'
    Write-GemsProfile 'candidate' @($real, $real, $real)
    Live 'restore' | Out-Null
    if (-not (Has (Live 'set gems 90') '^error: not written: its pointer paths are guesses')) { Fail 'set wrote through guesses' }
    if (-not (Has (Live 'limit gems 50') "aren't confirmed yet")) { Fail 'limit set on guesses' }
}
