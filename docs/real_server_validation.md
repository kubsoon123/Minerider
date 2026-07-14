# Real-server validation environment

Record of the local Minecraft 1.21.4 test environment used to validate
MineRider against a real server. All server artifacts live under
`.test-servers/` (git-ignored); only this document, the scripts and
sanitized fixtures are committed.

## Environment

| Item | Value |
|---|---|
| OS | Windows (Git Bash + Windows PowerShell 5.1) |
| Rust | 1.97.0 (2026-07-07) |
| System Java | 1.8.0_491 — **too old for MC 1.21.4 (needs 21)** |
| Test JDK | Eclipse Temurin 21 JRE, downloaded from api.adoptium.net into `.test-servers/jdk-21/` (local only, no global install) |
| Server | Paper 1.21.4 (build recorded in `.test-servers/paper-1.21.4/server-metadata.json`) |
| Minecraft version | 1.21.4 |
| Protocol version | 769 |
| Validation date | 2026-07-12 |

## EULA

Running a Minecraft server requires accepting the Minecraft EULA
(<https://aka.ms/MinecraftEULA>). `scripts/test-server/setup-paper-1.21.4.ps1`
writes `eula.txt` with `eula=true` for the **local, offline-mode,
localhost-only** test server. Run setup only after accepting that EULA;
otherwise, do not run either server.

## Network safety

- The server binds to `127.0.0.1` only (`server-ip=127.0.0.1`).
- RCON (used to drive the teleport scenario) also binds localhost only;
  the committed default is a test-only password and must not be reused.
- No firewall changes, no port forwarding, offline mode, no real accounts.

## Layout

```
.test-servers/
  jdk-21/                 downloaded Temurin JRE (not committed)
  paper-1.21.4/           Paper server dir (not committed)
    server-metadata.json  build + sha256 + download timestamp
  vanilla-1.21.4/         official Mojang server dir (not committed)
    server-metadata.json  source + sha1 + download timestamp
  traces/                 captured MineRider traces (not committed)
scripts/test-server/      management scripts (committed)
```

## Paper reproduction

```powershell
scripts\test-server\setup-paper-1.21.4.ps1
scripts\test-server\start-paper-1.21.4.ps1
scripts\test-server\validate-server-ready.ps1
scripts\test-server\run-minerider-validation.ps1 -DurationSeconds 60
scripts\test-server\stop-paper-1.21.4.ps1
```

The setup script resolves the Paper 1.21.4 build through PaperMC's official
API, verifies its SHA-256, and records the build and source in ignored
metadata. It installs Temurin 21 only under `.test-servers/jdk-21/` and does
not change the system Java installation.

## Official vanilla server reproduction

The optional comparison server uses only Mojang's official version manifest
and server download. Run this block from any directory inside the repository.
It writes only below `.test-servers/vanilla-1.21.4/`.

```powershell
$ErrorActionPreference = "Stop"
$RepoRoot = (git rev-parse --show-toplevel).Trim()
if ($LASTEXITCODE -ne 0) { throw "Run this inside the MineRider repository." }
$Root = [IO.Path]::GetFullPath($RepoRoot)
$ServerDir = Join-Path $Root ".test-servers\vanilla-1.21.4"
$Jar = Join-Path $ServerDir "server.jar"
New-Item -ItemType Directory -Force -Path $ServerDir | Out-Null

$ManifestUrl = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json"
$Manifest = Invoke-RestMethod -Uri $ManifestUrl
$Version = $Manifest.versions | Where-Object { $_.id -eq "1.21.4" } |
    Select-Object -First 1
if (-not $Version) { throw "Minecraft 1.21.4 is absent from Mojang's manifest." }
$VersionMetadata = Invoke-RestMethod -Uri $Version.url
$Download = $VersionMetadata.downloads.server
Invoke-WebRequest -UseBasicParsing -Uri $Download.url -OutFile $Jar
$ActualSha1 = (Get-FileHash -Algorithm SHA1 -LiteralPath $Jar).Hash.ToLowerInvariant()
if ($ActualSha1 -ne $Download.sha1.ToLowerInvariant()) {
    Remove-Item -LiteralPath $Jar -Force
    throw "Vanilla server SHA-1 mismatch. The downloaded JAR was removed."
}

[ordered]@{
    minecraft_version = "1.21.4"
    server_type = "vanilla"
    jar_name = "server.jar"
    sha1 = $ActualSha1
    download_source = $Download.url
    downloaded_at = (Get-Date).ToUniversalTime().ToString("o")
} | ConvertTo-Json | Set-Content -Encoding UTF8 `
    (Join-Path $ServerDir "server-metadata.json")

# Execute this line only after reading and accepting https://aka.ms/MinecraftEULA.
Set-Content -Encoding ASCII (Join-Path $ServerDir "eula.txt") "eula=true"
@'
server-ip=127.0.0.1
server-port=25566
online-mode=false
enable-rcon=true
rcon.port=25576
rcon.password=minerider-local-test
level-seed=1234567890123
gamemode=creative
difficulty=peaceful
spawn-monsters=false
spawn-animals=false
view-distance=6
simulation-distance=4
network-compression-threshold=256
'@ | Set-Content -Encoding ASCII (Join-Path $ServerDir "server.properties")
```

The validated artifact had SHA-1
`4707d00eb834b446575d89a61a11b5d548d8c001`. The manifest-derived checksum
comparison above remains the authority for a fresh download.

Start the server with the repository-local Java runtime. This refuses a
duplicate listener and waits for a fresh startup-complete message:

```powershell
$ErrorActionPreference = "Stop"
$RepoRoot = (git rev-parse --show-toplevel).Trim()
if ($LASTEXITCODE -ne 0) { throw "Run this inside the MineRider repository." }
$Root = [IO.Path]::GetFullPath($RepoRoot)
$ServerDir = Join-Path $Root ".test-servers\vanilla-1.21.4"
$Java = Join-Path $Root ".test-servers\jdk-21\bin\java.exe"
$PortInUse = Get-NetTCPConnection -LocalPort 25566 -State Listen `
    -ErrorAction SilentlyContinue
if ($PortInUse) { throw "Port 25566 is already in use." }
$Logs = Join-Path $ServerDir "logs"
New-Item -ItemType Directory -Force -Path $Logs | Out-Null
$Stdin = Join-Path $Logs "stdin.empty"
if (-not (Test-Path $Stdin)) { New-Item -ItemType File -Path $Stdin | Out-Null }
$LaunchTimeUtc = (Get-Date).ToUniversalTime()
$Vanilla = Start-Process -FilePath $Java `
    -ArgumentList "-Xms512M","-Xmx1G","-jar","server.jar","nogui" `
    -WorkingDirectory $ServerDir -RedirectStandardInput $Stdin `
    -RedirectStandardOutput (Join-Path $Logs "server-stdout.log") `
    -RedirectStandardError (Join-Path $Logs "server-stderr.log") `
    -WindowStyle Hidden -PassThru
Set-Content -Encoding ASCII (Join-Path $ServerDir "server.pid") $Vanilla.Id
$LatestLog = Join-Path $Logs "latest.log"
$Deadline = (Get-Date).AddMinutes(3)
$Ready = $false
while ((Get-Date) -lt $Deadline) {
    if ($Vanilla.HasExited) { throw "Vanilla server exited during startup." }
    if (Test-Path $LatestLog) {
        $LogFile = Get-Item $LatestLog
        if ($LogFile.LastWriteTimeUtc -ge $LaunchTimeUtc -and
            (Get-Content $LatestLog -Raw) -match 'Done \([0-9.]+s\)!') {
            $Ready = $true
            break
        }
    }
    Start-Sleep -Seconds 1
}
if (-not $Ready) { throw "Vanilla server did not become ready within 3 minutes." }
Write-Host "Vanilla 1.21.4 ready on 127.0.0.1:25566 (PID $($Vanilla.Id))."
```

After validation, stop it gracefully through the localhost-only RCON client:

```powershell
$ErrorActionPreference = "Stop"
$RepoRoot = (git rev-parse --show-toplevel).Trim()
if ($LASTEXITCODE -ne 0) { throw "Run this inside the MineRider repository." }
$Root = [IO.Path]::GetFullPath($RepoRoot)
$PidFile = Join-Path $Root ".test-servers\vanilla-1.21.4\server.pid"
$VanillaPid = [int](Get-Content -LiteralPath $PidFile)
$Vanilla = Get-Process -Id $VanillaPid -ErrorAction Stop
& (Join-Path $Root "scripts\test-server\rcon.ps1") -Port 25576 -Command "stop"
$Vanilla.WaitForExit(60000) | Out-Null
if (-not $Vanilla.HasExited) { throw "Vanilla server did not stop within 60 s." }
Remove-Item -LiteralPath $PidFile -Force
```

Paper and vanilla results must remain separate. Paper uses port 25565;
vanilla uses 25566.

## Manual vanilla-client reference capture

This procedure is intentionally manual and remains **not performed**. It
requires explicit permission to use an official test account. Never read,
copy, log, proxy, or inspect Microsoft/launcher credentials or tokens.

1. Start Paper or the official vanilla server using the commands above.
2. After authenticating in the official launcher outside this workflow,
   launch exactly Minecraft Java 1.21.4 and direct-connect to
   `127.0.0.1:25566` (vanilla) or `127.0.0.1:25565` (Paper).
3. In a separate PowerShell, begin a server-visible reference log:

```powershell
$RepoRoot = (git rev-parse --show-toplevel).Trim()
if ($LASTEXITCODE -ne 0) { throw "Run this inside the MineRider repository." }
$Root = [IO.Path]::GetFullPath($RepoRoot)
$Stamp = (Get-Date).ToUniversalTime().ToString("yyyyMMddTHHmmssZ")
$Source = Join-Path $Root ".test-servers\vanilla-1.21.4\logs\latest.log"
$Capture = Join-Path $Root ".test-servers\traces\vanilla-client-$Stamp.log"
New-Item -ItemType Directory -Force (Split-Path $Capture) | Out-Null
Get-Content -LiteralPath $Source -Wait -Tail 0 | Tee-Object -FilePath $Capture
```

4. Join, idle for at least 60 seconds, and run deterministic teleport steps
   from another PowerShell (replace `TestPlayer` with the visible test name):

```powershell
scripts\test-server\rcon.ps1 -Port 25576 `
    -Command "tp TestPlayer 10.5 100 10.5"
scripts\test-server\rcon.ps1 -Port 25576 `
    -Command "tp TestPlayer 5000.5 100 5000.5"
```

5. Disconnect the client, then press Ctrl+C in the log-capture PowerShell.
   Stop the server through RCON.
6. Before committing any fixture, replace usernames, UUIDs, local paths,
   addresses, and timestamps with stable placeholders. Never commit the raw
   log or anything from the launcher profile.

The log records server-visible join, teleport, timeout, kick, and leave
behavior; it is not a packet-level vanilla reference. Packet parity remains
`PARTIAL` until an already-installed trusted capture method is explicitly
authorized and restricted to loopback TCP port 25565 or 25566. Such capture
must start only after launcher authentication and stop immediately after the
local server disconnect, so authentication traffic is never in scope.
