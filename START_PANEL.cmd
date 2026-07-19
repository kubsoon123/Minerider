@echo off
cd /d "%~dp0"
if not exist "target\release\minerider-lua.exe" (
  echo.
  echo Nie znaleziono target\release\minerider-lua.exe.
  echo Najpierw wykonaj INSTALUJ.cmd.
  pause
  exit /b 1
)
if not exist "apps\anarchia-panel\node_modules\express" (
  cd /d "%~dp0apps\anarchia-panel"
  call npm install
  if errorlevel 1 exit /b 1
)
echo.
set /p "BOTPASS=Podaj haslo botow: "
if "%BOTPASS%"=="" (
  echo Haslo nie moze byc puste.
  pause
  exit /b 1
)
set "MINERIDER_BOT_PASSWORD=%BOTPASS%"
set "BOTPASS="
cd /d "%~dp0apps\anarchia-panel"
echo.
echo Panel: http://127.0.0.1:3000
echo Zamkniecie tego okna zatrzyma panel i boty.
echo.
call npm start
set "MINERIDER_BOT_PASSWORD="
if errorlevel 1 (
  echo.
  echo Panel zakonczyl prace z bledem.
  pause
)
