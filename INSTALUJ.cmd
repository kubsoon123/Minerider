@echo off
cd /d "%~dp0"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0INSTALUJ.ps1"
if errorlevel 1 (
  echo.
  echo Instalacja nie powiodla sie. Przeczytaj komunikat powyzej.
  pause
)
