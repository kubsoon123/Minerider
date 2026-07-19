$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $MyInvocation.MyCommand.Path
$Panel = Join-Path $Root 'apps\anarchia-panel'
$Binary = Join-Path $Root 'target\release\minerider-lua.exe'

if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
    throw 'Node.js not found. Run INSTALUJ.cmd first.'
}

if (-not (Test-Path $Binary)) {
    throw 'minerider-lua.exe not found. Run INSTALUJ.cmd first.'
}

if (-not (Test-Path (Join-Path $Panel 'node_modules\express'))) {
    Set-Location $Panel
    npm install
}

$SecurePassword = Read-Host 'Bot password (not saved)' -AsSecureString
if ($SecurePassword.Length -eq 0) {
    throw 'Password cannot be empty.'
}

$Pointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($SecurePassword)
try {
    $env:MINERIDER_BOT_PASSWORD = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($Pointer)
    Set-Location $Panel
    Write-Host ''
    Write-Host 'Panel: http://127.0.0.1:3000' -ForegroundColor Green
    Write-Host 'Closing this window stops the panel and bots.'
    Write-Host ''
    npm start
}
finally {
    $env:MINERIDER_BOT_PASSWORD = $null
    [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($Pointer)
}
