# Windows version of ../decimal-test.sh: numbers shown with decimals and as times. Energy (a
# float, 1.50 at start) is found from typed "1.5", "1.7" (1.75 cut off), "2.0" (2.05 rounded),
# 2.1, 2.2, then saved, set to 3.5 and kept between 1.25 and 4.5; gold (an int) typed as if shown
# in tenths ("100.0" for 1000) is saved with its decimal, shown, set and limited in tenths; a time
# "0:20" is a search for 20 seconds, which finds wood (a double, 12 at start + 8).
. "$PSScriptRoot\common.ps1"
Invoke-Test 'decimal-test' {
    $g = Start-Game
    Start-Live
    Live "attach $($g.Id)" -Quiet | Out-Null
    Write-Host '--- energy shown as 1.5, 1.7, 2.0'
    Live 'type 1.5' | Out-Null
    Game 'gain 0.25'
    Live 'type 1.7' | Out-Null
    Game 'gain 0.3'
    Live 'type 2.0' | Out-Null
    # The game's running maximum of energy follows it too: two more numbers, then the probe
    # tells them apart.
    $r = @()
    foreach ($e in '2.1', '2.2') {
        Game 'gain 0.1'
        $r = Live "type $e"
        if (Has $r 'found it') { break }
    }
    $found = $r | Where-Object { $_ -match '^found it at (0x[0-9a-f]+)' } | Select-Object -First 1
    if (-not $found) { Fail 'energy not found' }
    $loc = $matches[1]
    $energy = Game-Value 'energy'
    $peek = (Live "peek ${loc}:f32" | Select-Object -Last 1)
    if ($peek -notmatch ' = (-?[0-9.]+)' -or [Math]::Abs([double]$matches[1] - $energy) -gt 0.001) { Fail "found the wrong value (game: $energy, $peek)" }

    Write-Host '--- energy: save, set 3.5, keep between 1.25 and 4.5'
    Live 'save energy' | Select-Object -Last 1 | Out-Null
    Live 'set energy 3.5' -Quiet | Out-Null
    Game 'show'
    Expect 'energy=3\.50'
    Live 'limit energy 1.25 4.5' | Out-Null
    Game 'gain 5'
    Start-Sleep 1
    Game 'show'
    Expect 'energy=4\.50'
    if (-not (Has (Live 'values') '^energy\s+4\.5 .*kept between 1\.25 and 4\.5')) { Fail "values doesn't show energy as 4.5" }

    Write-Host '--- gold shown in tenths: 100.0 is 1000'
    Live 'reset' -Quiet | Out-Null
    Live 'type 100.0' | Out-Null
    Game 'earn 37'
    Live 'type 103.7' | Out-Null
    Game 'spend 10'
    $r = Live 'type 102.7'
    if (-not (Has $r 'found it')) { Fail 'gold not found from tenths' }
    Live 'save gold' | Out-Null
    if (-not ((Profile-Lines) -match '^decimals 1')) { Fail 'gold saved without its decimal' }
    if (-not (Has (Live 'values') '^gold\s+102\.7 ')) { Fail "values doesn't show gold as 102.7" }
    Live 'set gold 120.3' -Quiet | Out-Null
    Game 'show'
    Expect 'gold=1203 '
    if (-not (Has (Live 'set gold 1.25') 'one decimal at most')) { Fail 'gold took two decimals' }
    Live 'limit gold - 130' | Out-Null
    Game 'earn 500'
    Start-Sleep 1
    Game 'show'
    Expect 'gold=1300 '
    Live 'values' | Out-Null

    Write-Host '--- a time: 0:20 is 20 seconds'
    Live 'reset' -Quiet | Out-Null
    Game 'wood 8'
    if (-not (Has (Live 'type 0:20') 'typed 0:20')) { Fail 'time not taken' }
}
