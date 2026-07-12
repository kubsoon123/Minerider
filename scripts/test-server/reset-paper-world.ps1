# Resets the test world: stops the server if running, deletes the world
# directories so the next start regenerates them from the fixed seed.
$ErrorActionPreference = "Stop"

$Root      = (Resolve-Path "$PSScriptRoot\..\..").Path
$ServerDir = Join-Path $Root ".test-servers\paper-1.21.4"
$StopScript = Join-Path $PSScriptRoot "stop-paper-1.21.4.ps1"

if (Test-Path (Join-Path $ServerDir "server.pid")) {
    & $StopScript
}

foreach ($world in @("world", "world_nether", "world_the_end")) {
    $path = Join-Path $ServerDir $world
    if (Test-Path $path) {
        Remove-Item $path -Recurse -Force
        Write-Host "Deleted $path"
    }
}
Write-Host "World reset. Start the server to regenerate from the fixed seed."
