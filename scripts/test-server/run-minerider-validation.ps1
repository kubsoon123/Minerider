# Runs MineRider against the local Paper server for a fixed duration with
# full trace capture, then stops the client. Artifacts land in
# .test-servers/traces/ (git-ignored).
param(
    [int]$DurationSeconds = 60,
    [string]$Username = "MineRiderTest"
)
$ErrorActionPreference = "Stop"

$Root      = (Resolve-Path "$PSScriptRoot\..\..").Path
$TracesDir = Join-Path $Root ".test-servers\traces"
$ReadyScript = Join-Path $PSScriptRoot "validate-server-ready.ps1"

& $ReadyScript
if ($LASTEXITCODE -ne 0) { throw "Server is not ready." }

New-Item -ItemType Directory -Force -Path $TracesDir | Out-Null
$stamp = (Get-Date).ToUniversalTime().ToString("yyyyMMddTHHmmssZ")
$tracePath = Join-Path $TracesDir "minerider-$stamp.jsonl"
$clientLog = Join-Path $TracesDir "minerider-$stamp.log"

Write-Host "Building MineRider (release)..."
Push-Location $Root
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
} finally {
    Pop-Location
}

$exe = Join-Path $Root "target\release\minerider.exe"
Write-Host "Connecting $Username to 127.0.0.1:25565 for $DurationSeconds s, trace -> $tracePath"
$env:MINERIDER_TRACE = $tracePath
$env:MINERIDER_TRACE_SCENARIO = "paper_real_server"
$env:RUST_LOG = "debug"
$proc = Start-Process -FilePath $exe `
    -ArgumentList "127.0.0.1","25565",$Username `
    -RedirectStandardOutput $clientLog -RedirectStandardError "$clientLog.err" `
    -WindowStyle Hidden -PassThru

$disconnected = $proc.WaitForExit($DurationSeconds * 1000)
if (-not $disconnected) {
    Write-Host "Client still connected after $DurationSeconds s; stopping it."
    Stop-Process -Id $proc.Id -Force
    $proc.WaitForExit()
}

Write-Host ""
Write-Host "--- client stdout ---"
Get-Content $clientLog -ErrorAction SilentlyContinue
Write-Host "--- client stderr (last 20 lines) ---"
Get-Content "$clientLog.err" -Tail 20 -ErrorAction SilentlyContinue
Write-Host ""
Write-Host "Trace: $tracePath"
Write-Host "Log:   $clientLog"
