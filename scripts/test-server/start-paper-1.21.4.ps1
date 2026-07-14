# Starts the local Paper 1.21.4 server in the background, saves the PID,
# waits until the server reports ready. Refuses to start a duplicate.
$ErrorActionPreference = "Stop"

$Root      = (Resolve-Path "$PSScriptRoot\..\..").Path
$ServerDir = Join-Path $Root ".test-servers\paper-1.21.4"
$JavaExe   = Join-Path $Root ".test-servers\jdk-21\bin\java.exe"
$JarPath   = Join-Path $ServerDir "paper.jar"
$PidFile   = Join-Path $ServerDir "server.pid"
$LogDir    = Join-Path $ServerDir "logs"
$Port      = 25565

if (-not (Test-Path $JavaExe) -or -not (Test-Path $JarPath)) {
    throw "Server not set up. Run scripts\test-server\setup-paper-1.21.4.ps1 first."
}

# Refuse duplicates: live PID file or a listening port.
if (Test-Path $PidFile) {
    $oldPid = [int](Get-Content $PidFile)
    $old = Get-Process -Id $oldPid -ErrorAction SilentlyContinue
    if ($old) { throw "Server already running (PID $oldPid). Stop it first." }
    Remove-Item $PidFile -Force
}
$listener = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue
if ($listener) { throw "Port $Port already in use (PID $($listener.OwningProcess))." }

New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
$stdout = Join-Path $LogDir "server-stdout.log"
$stderr = Join-Path $LogDir "server-stderr.log"
# Redirect stdin too: without it the java process inherits the caller's
# handles, which makes callers (e.g. Git Bash) hang until the server exits.
$stdinFile = Join-Path $LogDir "stdin.empty"
if (-not (Test-Path $stdinFile)) { New-Item -ItemType File -Path $stdinFile | Out-Null }

$launchTimeUtc = (Get-Date).ToUniversalTime()
$proc = Start-Process -FilePath $JavaExe `
    -ArgumentList "-Xms512M","-Xmx1G","-jar","paper.jar","--nogui" `
    -WorkingDirectory $ServerDir `
    -RedirectStandardInput $stdinFile `
    -RedirectStandardOutput $stdout -RedirectStandardError $stderr `
    -WindowStyle Hidden -PassThru
Set-Content -Path $PidFile -Value $proc.Id -Encoding ASCII
Write-Host "Server starting (PID $($proc.Id)), waiting for ready..."

$deadline = (Get-Date).AddMinutes(3)
$latestLog = Join-Path $LogDir "latest.log"
while ((Get-Date) -lt $deadline) {
    if ($proc.HasExited) {
        throw "Server exited during startup (code $($proc.ExitCode)). See $stdout and $stderr"
    }
    if (Test-Path $latestLog) {
        $logFile = Get-Item $latestLog
        if ($logFile.LastWriteTimeUtc -lt $launchTimeUtc) {
            Start-Sleep -Seconds 1
            continue
        }
        $content = Get-Content $logFile.FullName -Raw -ErrorAction SilentlyContinue
        if ($content -match 'Done \([0-9.]+s\)!') {
            Write-Host "Server is ready (log reports Done)."
            exit 0
        }
        if ($content -match 'Failed to start the minecraft server') {
            throw "Server failed to start. See $latestLog"
        }
    }
    Start-Sleep -Seconds 1
}
throw "Server did not become ready within 3 minutes. See $latestLog"
