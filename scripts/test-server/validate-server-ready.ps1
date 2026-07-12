# Verifies the local Paper server is alive, listening on 127.0.0.1:25565
# and started without fatal errors. Exits non-zero on any failure.
$ErrorActionPreference = "Stop"

$Root      = (Resolve-Path "$PSScriptRoot\..\..").Path
$ServerDir = Join-Path $Root ".test-servers\paper-1.21.4"
$PidFile   = Join-Path $ServerDir "server.pid"
$LatestLog = Join-Path $ServerDir "logs\latest.log"
$Port      = 25565

$ok = $true

if (Test-Path $PidFile) {
    $serverPid = [int](Get-Content $PidFile)
    $proc = Get-Process -Id $serverPid -ErrorAction SilentlyContinue
    if ($proc) {
        Write-Host "OK  process alive (PID $pid)"
    } else {
        Write-Host "FAIL process $serverPid not running"; $ok = $false
    }
} else {
    Write-Host "FAIL no PID file"; $ok = $false
}

$listener = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue
if ($listener) {
    $addr = $listener | Select-Object -First 1 -ExpandProperty LocalAddress
    Write-Host "OK  port $Port listening on $addr"
    if ($addr -ne "127.0.0.1") {
        Write-Host "FAIL server not bound to localhost only ($addr)"; $ok = $false
    }
} else {
    Write-Host "FAIL port $Port not listening"; $ok = $false
}

if (Test-Path $LatestLog) {
    $content = Get-Content $LatestLog -Raw
    if ($content -match 'Done \([0-9.]+s\)!') {
        Write-Host "OK  startup completed (log reports Done)"
    } else {
        Write-Host "FAIL no Done message in latest.log"; $ok = $false
    }
    if ($content -match 'Failed to start the minecraft server') {
        Write-Host "FAIL fatal startup error in latest.log"; $ok = $false
    }
} else {
    Write-Host "FAIL no latest.log"; $ok = $false
}

if (-not $ok) { exit 1 }
Write-Host "Server ready."
exit 0
