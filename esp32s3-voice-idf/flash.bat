@echo off
setlocal
set PORT=%1
if "%PORT%"=="" set PORT=COM4
set ROOT=%~dp0
set TARGET=D:\t\xtensa-esp32s3-espidf\release

powershell -NoProfile -ExecutionPolicy Bypass -Command ". '%ROOT%activate.ps1'; Set-Location '%ROOT%'; cargo build --release -j 1" || exit /b 1

if not exist "%ROOT%components\esp-sr\model\srmodels.bin" (
  echo [flash] packing srmodels.bin
  "D:\esp32\toolchain\espressif\idf-tools\python_env\idf5.4_py3.12_env\Scripts\python.exe" ^
    "%ROOT%components\esp-sr\model\pack_model.py" ^
    -m "%ROOT%components\esp-sr\model" -o srmodels.bin || exit /b 1
)

espflash flash --port %PORT% --baud 921600 ^
  --bootloader "%TARGET%\bootloader.bin" ^
  --partition-table "%ROOT%partitions.csv" ^
  --partition-table-offset 0x8000 ^
  "%TARGET%\esp32s3-voice-idf" || exit /b 1

espflash write-bin --port %PORT% --baud 921600 0x410000 ^
  "%ROOT%components\esp-sr\model\srmodels.bin" || exit /b 1

echo [flash] done
