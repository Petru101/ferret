# Runs the Windows feature tests one after another (they share the stand-in's profile, so never
# at once) and sums them up. Each test's transcript: run\win-tests\<test>.log.
#   run-all.ps1 [test names...]   e.g. run-all.ps1 limit-test names-test
# Known to fail on Windows (2026-10-22), Windows' heap against Ferret's heuristics:
# - paths-test: freed memory stays readable, and most saved pointer paths (2868 of 3000, most
#   through ntdll's and the C runtime's heap bookkeeping) still lead to the freed old gems after
#   "newroom", outvoting the 125 that follow the game to the new one.
# - shapes-test: a learned shape keeps the 8 bytes after the record, the next heap block's header,
#   which Windows encodes and which changes with the neighbouring block: the newest stack's
#   doesn't match.
param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Only)
$tests = 'limit-test', 'names-test', 'copy-test', 'decimal-test', 'doubt-test', 'move-test', 'paths-test', 'probe-test', 'shapes-test', 'shared-test', 'types-test'
if ($Only) { $tests = $tests | Where-Object { $Only -contains $_ } }
$logs = Join-Path (Split-Path (Split-Path $PSScriptRoot)) 'run\win-tests'
New-Item -ItemType Directory -Force $logs | Out-Null
$results = foreach ($t in $tests) {
    $start = Get-Date
    powershell -NoProfile -ExecutionPolicy Bypass -File "$PSScriptRoot\$t.ps1" *> "$logs\$t.log"
    $code = $LASTEXITCODE
    $last = Get-Content "$logs\$t.log" -Tail 1
    [pscustomobject]@{ Test = $t; Result = if ($code -eq 0) { 'PASS' } else { 'FAIL' }; Seconds = [int]((Get-Date) - $start).TotalSeconds; Last = $last }
}
$results | Format-Table -AutoSize -Wrap
"$(@($results | Where-Object Result -eq 'PASS').Count) of $(@($results).Count) passed"
