@echo off
setlocal
set PORT=%1
if "%PORT%"=="" set PORT=COM4
set ROOT=%~dp0
set TARGET=D:\t\xtensa-esp32s3-espidf\release
set SRMODEL_OFFSET=0x410000

powershell -NoProfile -ExecutionPolicy Bypass -Command ". '%ROOT%activate.ps1'; Set-Location '%ROOT%'; cargo build --release -j 1" || exit /b 1

rem esp-sr >= 2.6 packs the Kconfig-selected models (CONFIG_SR_WN_*) during the CMake build.
set SRMODELS=
for /d %%D in ("%TARGET%\build\esp-idf-sys-*") do if exist "%%D\out\build\srmodels\srmodels.bin" set SRMODELS=%%D\out\build\srmodels\srmodels.bin
if not exist "%SRMODELS%" (
  echo [flash] srmodels.bin not found - build the project first / check CONFIG_SR_WN_* in sdkconfig
  exit /b 1
)

espflash flash --port %PORT% --baud 921600 ^
  --bootloader "%TARGET%\bootloader.bin" ^
  --partition-table "%ROOT%partitions.csv" ^
  --partition-table-offset 0x8000 ^
  "%TARGET%\esp32s3-voice-idf" || exit /b 1

espflash write-bin --port %PORT% --baud 921600 %SRMODEL_OFFSET% "%SRMODELS%" || exit /b 1

echo [flash] done
