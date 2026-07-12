# Stops the local Paper server gracefully (RCON "stop"), falling back to
# process termination only if the server does not shut down in time.
$ErrorActionPreference = "Stop"

$Root      = (Resolve-Path "$PSScriptRoot\..\..").Path
$ServerDir = Join-Path $Root ".test-servers\paper-1.21.4"
$PidFile   = Join-Path $ServerDir "server.pid"
$RconScript = Join-Path $PSScriptRoot "rcon.ps1"

if (-not (Test-Path $PidFile)) {
    Write-Host "No PID file; server is not running."
    exit 0
}
$pid = [int](Get-Content $PidFile)
$proc = Get-Process -Id $pid -ErrorAction SilentlyContinue
if (-not $proc) {
    Write-Host "Process $pid already gone; cleaning PID file."
    Remove-Item $PidFile -Force
    exit 0
}

Write-Host "Sending graceful stop via RCON..."
try {
    & $RconScript -Command "stop" | Out-Null
} catch {
    Write-Warning "RCON stop failed: $_"
}

$deadline = (Get-Date).AddSeconds(60)
while ((Get-Date) -lt $deadline) {
    if ($proc.HasExited) {
        Remove-Item $PidFile -Force
        Write-Host "Server stopped gracefully."
        exit 0
    }
    Start-Sleep -Seconds 1
}

Write-Warning "Server did not exit within 60s; terminating process $pid."
Stop-Process -Id $pid -Force
Remove-Item $PidFile -Force
