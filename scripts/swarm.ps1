# Launch N bots against the test server, half on each team.
# PowerShell counterpart to scripts/swarm.sh (which needs Git Bash / MSYS to
# resolve the native exe's file paths; this one is shell-independent).
#
#   powershell -File scripts/swarm.ps1 -N 30 -Secs 900
param(
    [int]$N = 2,
    [int]$Secs = 120,
    [string]$Addr = "127.0.0.1:27015"
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
$Exe = Join-Path $Root "target\debug\examples\capture_running.exe"
if (-not (Test-Path $Exe)) {
    Write-Error "build it first:  cargo build -p client --example capture_running"
    exit 1
}
$Out = Join-Path $Root "captures\swarm"
New-Item -ItemType Directory -Force -Path $Out | Out-Null
Get-ChildItem "$Out\bot*.log" -ErrorAction SilentlyContinue | Remove-Item -Force

$procs = @()
for ($i = 1; $i -le $N; $i++) {
    $team = if ($i % 2 -eq 1) { 1 } else { 2 }
    $key = "AIPLAYERBOT{0:D4}" -f $i
    $name = "Bot{0:D2}" -f $i
    # Staged exits so the fleet leaves spread out (see swarm.sh for the
    # ReAuthCheck 60-minute ban on 7 disconnects in 15 s).
    # Integer! `$Secs + 5*($i-1)/2` is a DOUBLE for odd i (907.5), and the
    # bot's `secs.parse::<u64>()` falls back to 15 on "907.5" -- which made
    # every even-numbered bot exit after ~15 s.
    $life = [int]($Secs + 5 * ($i - 1) / 2)
    $outFile = Join-Path $Out "$name.bin"
    $logFile = Join-Path $Out "bot$i.log"

    $env:AIPLAYERS_NAME = $name
    $env:AIPLAYERS_KEY = $key
    $env:AIPLAYERS_TEAM = [string]$team
    # Start-Process redirects stderr (the telemetry) to the log file at spawn,
    # so it streams while the bot runs. stdout (quiet) goes nowhere useful.
    $p = Start-Process -FilePath $Exe -ArgumentList @($Addr, "$life", "`"$outFile`"") `
        -RedirectStandardError $logFile -RedirectStandardOutput "$logFile.out" `
        -PassThru -WindowStyle Hidden
    $procs += $p
    Write-Host "  $name  team $team  key $key  pid $($p.Id)"
    Start-Sleep -Milliseconds 1500
}

Write-Host "waiting for $N bots (${Secs}s)..."
foreach ($p in $procs) { $p.WaitForExit() }
Write-Host "done -- logs in $Out"
