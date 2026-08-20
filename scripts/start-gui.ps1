# Start the AI Players Radar as a fully DETACHED Windows process so it does
# not die when the agent shell / job object is cleaned up.
#
#   powershell -File scripts/start-gui.ps1
#   powershell -File scripts/start-gui.ps1 -Watchdog
#
# NOTE: Do NOT use Start-Process -PassThru against this exe from a redirected
# agent shell — Windows returns 0x800700E8 (ERROR_NO_DATA / pipe closed).
# Launch via `cmd /c start` (UseShellExecute) so std handles are not inherited.
param(
    [switch]$Watchdog,
    [int]$Port = 27016
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
$Exe = Join-Path $Root "target\debug\gui.exe"

if (-not (Test-Path $Exe)) {
    Write-Host "building gui..."
    Push-Location $Root
    rustup run stable-x86_64-pc-windows-msvc cargo build -p gui
    Pop-Location
}
if (-not (Test-Path $Exe)) {
    Write-Error "gui.exe missing at $Exe"
    exit 1
}

# Stop previous radar from this project only.
Get-Process -Name gui -ErrorAction SilentlyContinue | ForEach-Object {
    try {
        $path = $null
        try { $path = $_.Path } catch { }
        if ($path -and ($path -ieq $Exe)) {
            Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
        } elseif (-not $path) {
            # Path access denied for some sessions — still kill by name if
            # working dir is ours is unknown; prefer only killing if single instance.
            Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
        }
    } catch {
        # ignore
    }
}
Start-Sleep -Milliseconds 500

function Start-Radar {
    # Fully detach: `start` with empty title + UseShellExecute so the agent
    # shell's closed pipes cannot produce 0x800700E8 or kill the window.
    $env:REB_TELEMETRY_PORT = "$Port"
    $env:AIPLAYERS_TELEMETRY_PORT = "$Port"
    $arg = "/c set REB_TELEMETRY_PORT=$Port&& set AIPLAYERS_TELEMETRY_PORT=$Port&& start `"reBots Radar`" /D `"$Root`" `"$Exe`""
    $p = Start-Process -FilePath "cmd.exe" -ArgumentList $arg -WindowStyle Hidden -PassThru
    # cmd /c returns quickly; wait for gui.exe to appear.
    $gui = $null
    for ($i = 0; $i -lt 20; $i++) {
        Start-Sleep -Milliseconds 200
        $gui = Get-Process -Name gui -ErrorAction SilentlyContinue |
            Sort-Object StartTime -Descending |
            Select-Object -First 1
        if ($gui) { break }
    }
    if ($gui) {
        Write-Host "reBots Radar started  pid=$($gui.Id)  port=$Port"
    } else {
        Write-Host "reBots Radar launch issued (cmd pid=$($p.Id)) but gui.exe not seen yet"
        Write-Host "  check: $(Join-Path (Split-Path $Exe) 'gui.log')"
    }
    Write-Host "  log file: $(Join-Path (Split-Path $Exe) 'gui.log')"
    return $gui
}

$proc = Start-Radar
if (-not $Watchdog) {
    exit 0
}

Write-Host "watchdog on - will restart if the window closes"
while ($true) {
    Start-Sleep -Seconds 3
    $alive = Get-Process -Name gui -ErrorAction SilentlyContinue
    if (-not $alive) {
        Write-Host "radar exited - restarting..."
        Start-Sleep -Seconds 1
        $proc = Start-Radar
    }
}
