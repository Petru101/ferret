# What the Windows feature tests share (the PowerShell side of ../live.sh and the helpers each
# Linux test defines): the stand-in game, one `ferret.exe --cli` session per test, and checks.
# A test dot-sources this and runs its steps in `Invoke-Test <name> { ... }`: PASS or FAIL, and
# everything cleaned up either way. Each test runs in run\win-tests\<name>; the player's own
# target.exe profile and shapes are kept aside meanwhile and put back after.
#   FERRET_EXE   ferret.exe to test (default: target\release\ferret.exe, with MSYS2's GTK)

[Console]::InputEncoding = New-Object System.Text.UTF8Encoding $false
$script:Repo = Split-Path (Split-Path $PSScriptRoot)
$script:Exe = if ($env:FERRET_EXE) { $env:FERRET_EXE } else { Join-Path $script:Repo 'target\release\ferret.exe' }
if (-not $env:FERRET_EXE -and (Test-Path 'C:\msys64\ucrt64\bin')) { $env:PATH = "C:\msys64\ucrt64\bin;$env:PATH" }
$script:Profiles = "$env:LOCALAPPDATA\Ferret\profiles"

function Fail([string]$why) { throw $why }

function Start-Test([string]$Name) {
    $script:Dir = Join-Path $script:Repo "run\win-tests\$Name"
    Remove-Item -Recurse -Force $script:Dir -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force "$script:Dir\kept" | Out-Null
    $script:Cmd = "$script:Dir\cmd"
    $script:Log = "$script:Dir\log"
    $script:Profile = "$script:Profiles\target.exe.profile"
    $script:Shapes = "$script:Profiles\target.exe.shapes"
    # Shapes learned by one test would steer the next one's scans; the player's own wait aside.
    Get-Item $script:Profile, $script:Shapes -ErrorAction SilentlyContinue | Move-Item -Destination "$script:Dir\kept" -Force
    $script:Games = @()
}

function Stop-Test {
    foreach ($g in $script:Games) {
        if (-not $g.HasExited) { [IO.File]::WriteAllText($script:Cmd, "quit 1`n"); $g.WaitForExit(3000) | Out-Null }
        if (-not $g.HasExited) { $g.Kill() }
    }
    Stop-Live
    Remove-Item $script:Profile, $script:Shapes -ErrorAction SilentlyContinue
    Get-ChildItem "$script:Dir\kept" -ErrorAction SilentlyContinue | Move-Item -Destination $script:Profiles -Force
}

function Invoke-Test([string]$Name, [scriptblock]$Body) {
    Start-Test $Name
    $code = 0
    try {
        & $Body
        Write-Host 'PASS'
    } catch {
        Write-Host "FAIL: $($_.Exception.Message)"
        $code = 1
    } finally {
        Stop-Test
    }
    exit $code
}

# --- the stand-in game

function Start-Game([string]$Program = 'target.exe') {
    Remove-Item $script:Log, $script:Cmd -ErrorAction SilentlyContinue
    $g = Start-Process -FilePath (Join-Path $script:Repo "test-game\$Program") -ArgumentList $script:Cmd, $script:Log -WindowStyle Hidden -PassThru
    $t = Get-Date
    while (-not ((Test-Path $script:Log) -and (Get-Item $script:Log).Length -gt 0)) {
        if (((Get-Date) - $t).TotalSeconds -gt 15) { Fail 'the game did not start' }
        Start-Sleep -Milliseconds 200
    }
    $script:Games += $g
    $script:Game = $g
    $g
}

function Stop-Game {
    [IO.File]::WriteAllText($script:Cmd, "quit 1`n")
    if (-not $script:Game.WaitForExit(10000)) { Fail "the game didn't quit" }
}

function Last-Log { Get-Content $script:Log -Tail 1 }

# A command to the game (written whole, as the Linux tests' mv does), then its last log line.
function Game([string]$c, [switch]$Quiet) {
    [IO.File]::WriteAllText("$script:Cmd.tmp", "$c`n")
    Move-Item -Force "$script:Cmd.tmp" $script:Cmd
    Start-Sleep 1
    if (-not $Quiet) { Write-Host "   [game] $c -> $(Last-Log)" }
}

# The game's last log line must match `pattern` (a regex).
function Expect([string]$Pattern) {
    $l = Last-Log
    if ($l -notmatch $Pattern) { Fail "expected /$Pattern/ in: $l" }
}

# --- the Ferret CLI session

# Ferret's output, read on a thread of its own: PowerShell's OutputDataReceived events came out
# of order now and then, and the end-of-command marker arrived before a reply's last line.
if (-not ('FerretLines' -as [type])) {
    Add-Type -TypeDefinition @'
using System.Collections.Generic;
using System.IO;
using System.Threading;
public class FerretLines {
    readonly List<string> lines = new List<string>();
    public FerretLines(StreamReader reader, string file) {
        var t = new Thread(() => {
            string l;
            while ((l = reader.ReadLine()) != null) {
                lock (lines) lines.Add(l);
                File.AppendAllText(file, l + "\n");
            }
        });
        t.IsBackground = true;
        t.Start();
    }
    public int Count { get { lock (lines) return lines.Count; } }
    public string[] From(int start) { lock (lines) return lines.GetRange(start, lines.Count - start).ToArray(); }
}
'@
}

function Start-Live {
    $psi = New-Object Diagnostics.ProcessStartInfo $script:Exe, '--cli'
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.StandardOutputEncoding = New-Object Text.UTF8Encoding $false
    $f = [Diagnostics.Process]::Start($psi)
    $script:Out = New-Object FerretLines $f.StandardOutput, "$script:Dir\out"
    $script:Live = $f
}

function Stop-Live {
    if (-not $script:Live) { return }
    try { $script:Live.StandardInput.Close() } catch { }
    if (-not $script:Live.WaitForExit(10000)) { $script:Live.Kill() }
    $script:Live = $null
}

# Lines `sandbox` prints: the end-of-command marker's own output, left out.
$script:MarkerLines = '^(frontend in flatpak|frontend pid namespace|processes visible to frontend)'

# Sends a command and a `sandbox` marker after it, without waiting: where its output starts.
function Send-Live([string]$Line) {
    $start = $script:Out.Count
    Write-Host "> $Line"
    $script:Live.StandardInput.WriteLine($Line)
    $script:Live.StandardInput.WriteLine('sandbox')
    $script:Live.StandardInput.Flush()
    $start
}

# Waits until the output from `start` on matches `pattern`; its lines.
function Wait-Live([int]$Start, [string]$Pattern, [int]$Secs = 300) {
    $t = Get-Date
    while (((Get-Date) - $t).TotalSeconds -lt $Secs) {
        $new = @($script:Out.From($Start))
        if ($new | Where-Object { $_ -match $Pattern }) { return $new }
        if ($script:Live.HasExited) { Fail "ferret.exe ended (code $($script:Live.ExitCode))" }
        Start-Sleep -Milliseconds 200
    }
    Fail "no /$Pattern/ in $Secs s"
}

# Waits for the marker after the command sent at `start`; the command's reply lines.
function Wait-Done([int]$Start, [int]$Secs = 600, [switch]$Quiet) {
    $all = Wait-Live $Start '^> sandbox$' $Secs
    $end = [Array]::IndexOf($all, '> sandbox')
    $reply = @($all[0..([Math]::Max(0, $end - 1))] | Where-Object { $_ -notmatch $script:MarkerLines -and $_ -notmatch '^> ' })
    if ($end -eq 0) { $reply = @() }
    if (-not $Quiet) { $reply | ForEach-Object { Write-Host $_ } }
    $reply
}

# A command and its reply lines (printed unless -Quiet).
function Live([string]$Line, [switch]$Quiet, [int]$Secs = 600) {
    Wait-Done (Send-Live $Line) $Secs -Quiet:$Quiet
}

# Any reply line matching `pattern`?
function Has([object[]]$Reply, [string]$Pattern) { [bool]($Reply | Where-Object { $_ -match $Pattern }) }

# The matches listed now ("0x...:type").
function Match-List { @(Live 'list' -Quiet | Where-Object { $_ -match '^0x' } | ForEach-Object { ($_ -split ' ')[0] }) }

# The listed match the game reports as `name`: a marker written to each, the game asked, the
# number put back (others are copies: on the stack, or for the HUD). Its address, or nothing.
function Find-Real([string]$Name, [string]$Value) {
    foreach ($loc in Match-List) {
        Live "write $loc 4242" -Quiet | Out-Null
        Game 'show' -Quiet
        Live "write $loc $Value" -Quiet | Out-Null
        if ((Last-Log) -match "(^| )$Name=4242(\.0+)?( |$|,)") { return ($loc -split ':')[0] }
    }
}

# Find-Real, kept as the only match.
function Keep-RealMatch([string]$Name, [string]$Value) {
    $at = Find-Real $Name $Value
    if (-not $at) { Fail "no match is the real $Name" }
    Live "keep $at" | Out-Null
    $at
}

# The saved profile's lines.
function Profile-Lines { if (Test-Path $script:Profile) { @(Get-Content $script:Profile) } else { @() } }

# What the game's log says `name` is now (the first number after "name=").
function Game-Value([string]$Name) {
    if ((Last-Log) -match "(^| )$Name=(-?[0-9.]+)") { [double]$matches[2] } else { Fail "no $Name in: $(Last-Log)" }
}

# Ferret's `values` must read what the game says, for each name (within `tolerance`).
function Check-Values([string[]]$Names, [double]$Tolerance = 0.01) {
    Game 'show' -Quiet
    $v = Live 'values'
    foreach ($n in $Names) {
        $line = $v | Where-Object { $_ -match "^$n\s" } | Select-Object -First 1
        if (-not $line -or $line -notmatch "^$n\s+(-?[0-9.]+)") { Fail "values doesn't read $n" }
        $ferret = [double]$matches[1]
        $game = Game-Value $n
        if ([Math]::Abs($ferret - $game) -gt $Tolerance) { Fail "values says $n = $ferret, the game $game" }
    }
}
