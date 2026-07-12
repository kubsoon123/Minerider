# Downloads a local JDK 21 (Temurin) and Paper 1.21.4 into .test-servers,
# verifies checksums, writes server config. Idempotent. Never touches
# anything outside <repo>/.test-servers.
$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Root        = (Resolve-Path "$PSScriptRoot\..\..").Path
$TestServers = Join-Path $Root ".test-servers"
$ServerDir   = Join-Path $TestServers "paper-1.21.4"
$JdkDir      = Join-Path $TestServers "jdk-21"
$JavaExe     = Join-Path $JdkDir "bin\java.exe"
$JarPath     = Join-Path $ServerDir "paper.jar"
$MetaPath    = Join-Path $ServerDir "server-metadata.json"
$McVersion   = "1.21.4"
$RconPassword = "minerider-local-test"

New-Item -ItemType Directory -Force -Path $ServerDir | Out-Null

# --- JDK 21 (Eclipse Temurin, Adoptium API) ------------------------------
if (-not (Test-Path $JavaExe)) {
    $jdkUrl = "https://api.adoptium.net/v3/binary/latest/21/ga/windows/x64/jre/hotspot/normal/eclipse"
    $zipPath = Join-Path $TestServers "temurin21-jre.zip"
    Write-Host "Downloading Temurin 21 JRE from Adoptium API..."
    Invoke-WebRequest -Uri $jdkUrl -OutFile $zipPath -UseBasicParsing
    $info = Get-Item $zipPath
    if ($info.Length -lt 20MB) { throw "JDK download suspiciously small ($($info.Length) bytes)" }
    $bytes = [System.IO.File]::ReadAllBytes($zipPath)[0..1]
    if ($bytes[0] -ne 0x50 -or $bytes[1] -ne 0x4B) { throw "JDK download is not a ZIP archive" }
    $extractDir = Join-Path $TestServers "jdk-extract"
    if (Test-Path $extractDir) { Remove-Item $extractDir -Recurse -Force }
    Expand-Archive -Path $zipPath -DestinationPath $extractDir
    $inner = Get-ChildItem $extractDir -Directory | Select-Object -First 1
    if (-not $inner) { throw "JDK archive had no top-level directory" }
    if (Test-Path $JdkDir) { Remove-Item $JdkDir -Recurse -Force }
    Move-Item $inner.FullName $JdkDir
    Remove-Item $extractDir -Recurse -Force
    Remove-Item $zipPath -Force
    Write-Host "JDK installed at $JdkDir"
} else {
    Write-Host "JDK already present: $JavaExe"
}

# --- Paper 1.21.4 (official PaperMC "Fill" download service, v3) -------
$buildsUrl = "https://fill.papermc.io/v3/projects/paper/versions/$McVersion/builds/latest"
$latest = Invoke-RestMethod -Uri $buildsUrl
$build = $latest.id
$download = $latest.downloads.'server:default'
$jarName = $download.name
$expectedSha = $download.checksums.sha256
if (-not $jarName -or -not $expectedSha) { throw "PaperMC API response missing download metadata" }

$downloadOk = $false
if (Test-Path $JarPath) {
    $actualSha = (Get-FileHash $JarPath -Algorithm SHA256).Hash.ToLower()
    $downloadOk = ($actualSha -eq $expectedSha)
    if ($downloadOk) { Write-Host "Paper build $build already present, checksum OK" }
}
if (-not $downloadOk) {
    $jarUrl = $download.url
    Write-Host "Downloading Paper $McVersion build $build from fill.papermc.io..."
    Invoke-WebRequest -Uri $jarUrl -OutFile $JarPath -UseBasicParsing
    $actualSha = (Get-FileHash $JarPath -Algorithm SHA256).Hash.ToLower()
    if ($actualSha -ne $expectedSha) {
        Remove-Item $JarPath -Force
        throw "SHA-256 mismatch: expected $expectedSha, got $actualSha"
    }
    Write-Host "Checksum verified: $actualSha"
}

@{
    minecraft_version = $McVersion
    server_type       = "paper"
    build             = $build
    jar_name          = $jarName
    sha256            = $expectedSha
    download_source   = "https://fill.papermc.io/v3/projects/paper"
    downloaded_at     = (Get-Date).ToUniversalTime().ToString("o")
} | ConvertTo-Json | Set-Content -Path $MetaPath -Encoding UTF8
Write-Host "Wrote $MetaPath"

# --- EULA (documented acceptance for local test server) ------------------
$eulaPath = Join-Path $ServerDir "eula.txt"
@"
# Minecraft EULA accepted for a LOCAL, offline-mode, localhost-only test
# server used for MineRider protocol validation. See
# docs/real_server_validation.md. https://aka.ms/MinecraftEULA
eula=true
"@ | Set-Content -Path $eulaPath -Encoding ASCII

# --- server.properties (localhost-only, deterministic) -------------------
$propsPath = Join-Path $ServerDir "server.properties"
@"
server-ip=127.0.0.1
server-port=25565
online-mode=false
enforce-secure-profile=false
white-list=false
spawn-protection=0
gamemode=creative
force-gamemode=true
difficulty=peaceful
hardcore=false
pvp=true
enable-command-block=true
view-distance=6
simulation-distance=4
max-players=20
motd=MineRider Local Conformance Server
enable-status=true
network-compression-threshold=256
level-seed=1234567890123
spawn-monsters=false
spawn-animals=false
spawn-npcs=false
generate-structures=false
enable-rcon=true
rcon.port=25575
rcon.password=$RconPassword
enable-query=false
"@ | Set-Content -Path $propsPath -Encoding ASCII

Write-Host ""
Write-Host "Setup complete. RCON binds to 127.0.0.1:25575 (same server-ip binding)."
Write-Host "Start with: scripts\test-server\start-paper-1.21.4.ps1"
