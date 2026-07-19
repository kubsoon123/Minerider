$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $MyInvocation.MyCommand.Path
$Panel = Join-Path $Root 'apps\anarchia-panel'

function Refresh-Path {
    $MachinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $CargoPath = Join-Path $env:USERPROFILE '.cargo\bin'
    $env:Path = $MachinePath + ';' + $UserPath + ';' + $CargoPath
}

function Install-WithWinget {
    param([string]$Command, [string]$Package)

    if (Get-Command $Command -ErrorAction SilentlyContinue) {
        return
    }

    if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
        throw ('Missing ' + $Command + ' and winget. Install ' + $Package + ' manually.')
    }

    Write-Host ('Installing ' + $Package + '...') -ForegroundColor Cyan
    winget install --exact --id $Package --accept-package-agreements --accept-source-agreements
    Refresh-Path

    if (-not (Get-Command $Command -ErrorAction SilentlyContinue)) {
        throw ('Installed ' + $Package + ', but ' + $Command + ' is not visible yet. Close this window and run INSTALUJ.cmd again.')
    }
}

Write-Host ''
Write-Host 'MineRider + Lua panel installer' -ForegroundColor Green
Write-Host 'The first build can take several minutes.'
Write-Host ''

Install-WithWinget -Command 'node' -Package 'OpenJS.NodeJS.LTS'
Install-WithWinget -Command 'cargo' -Package 'Rustlang.Rustup'

$ProgramFilesX86 = [Environment]::GetFolderPath('ProgramFilesX86')
$VsWhere = Join-Path $ProgramFilesX86 'Microsoft Visual Studio\Installer\vswhere.exe'
if (-not (Test-Path $VsWhere)) {
    if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
        throw 'Microsoft C++ Build Tools are missing.'
    }

    Write-Host 'Installing Microsoft C++ Build Tools...' -ForegroundColor Cyan
    winget install --exact --id Microsoft.VisualStudio.2022.BuildTools --accept-package-agreements --accept-source-agreements --override '--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended'
}

Refresh-Path
Set-Location $Root
Write-Host ''
Write-Host 'Building MineRider with Lua...' -ForegroundColor Cyan
cargo build --release --features lua

Set-Location $Panel
if (-not (Test-Path (Join-Path $Panel 'node_modules\express'))) {
    Write-Host 'Installing panel dependencies...' -ForegroundColor Cyan
    npm install
}

Write-Host ''
Write-Host 'Installation complete. Run START_PANEL.cmd.' -ForegroundColor Green
Read-Host 'Press Enter to close'
