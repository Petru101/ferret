# Windows version of ../types-test.sh: value types besides plain integers. Energy (a float that
# grows by fractions and shows cut off to a whole number) and scrap (an XOR-encoded integer) are
# found, saved and limited; after a respawn the limits must find both again. Then shield (a
# double) is found and set. The Linux test prints; this one checks.
#   types-test.ps1 [target.exe|target32.exe]
param([string]$Program = 'target.exe')
. "$PSScriptRoot\common.ps1"
Invoke-Test "types-test-$Program" {
    if ($Program -ne 'target.exe') { $script:Profile = "$script:Profiles\$Program.profile"; $script:Shapes = "$script:Profiles\$Program.shapes" }
    $g = Start-Game $Program
    Start-Live
    Live "attach $($g.Id)" | Out-Null
    Write-Host '--- energy 1.5 shows as 1'
    Live 'scan 1' -Quiet | Out-Null
    Game 'gain 0.9'
    Live 'next 2' -Quiet | Out-Null
    Game 'gain 1.3'
    Live 'next 3' -Quiet | Out-Null
    Game 'gain 0.6'
    Live 'next 4' -Quiet | Out-Null
    Game 'gain 2.2'
    Live 'next 6' -Quiet | Out-Null
    Write-Host "--- going down leaves the game's highest-energy copy behind"
    Game 'gain -1.4'
    Live 'next 5' | Out-Null
    Live 'list' | Out-Null
    if (-not (Has (Live 'save energy') '^saved energy')) { Fail 'energy not saved' }
    Live 'limit energy 7' | Out-Null
    Game 'gain 3.1'
    Game 'show'
    Expect 'energy=7\.00 '
    Write-Host '--- scrap 50, XOR-encoded'
    Live 'scan 50' -Quiet | Out-Null
    Game 'scrap 17'
    Live 'next 67' -Quiet | Out-Null
    Game 'scrap -30'
    Live 'next 37' | Out-Null
    Live 'list' | Out-Null
    if (-not (Has (Live 'save scrap') '^saved scrap')) { Fail 'scrap not saved' }
    Live 'limit scrap 40' | Out-Null
    Game 'scrap 100'
    Game 'show'
    Expect 'scrap=40 '
    Live 'values' | Out-Null
    Write-Host '--- respawn: new player object, old one freed'
    Game 'respawn'
    Write-Host '--- waiting for the limits to find energy and scrap again'
    Start-Sleep 8
    Game 'gain 9'
    Game 'scrap 100'
    Game 'show'
    Expect 'energy=7\.00 '
    Expect 'scrap=40 '
    Live 'values' | Out-Null
    Write-Host '--- shield 250.25 (a double)'
    Live 'scan 250' -Quiet | Out-Null
    Game 'shield 11.5'
    Live 'next 261' -Quiet | Out-Null
    Game 'shield -30.2'
    Live 'next 231' | Out-Null
    Live 'list' | Out-Null
    Live 'set 5000' | Out-Null
    Game 'show'
    Expect 'shield=5000\.00 '
}
