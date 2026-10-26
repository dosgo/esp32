@echo off
chcp 65001 >nul
setlocal enabledelayedexpansion

rem ===========================================================================
rem  一键部署：装 APK + 推模型到手机
rem
rem  用法：
rem     deploy.bat            装 APK 并推模型（常用）
rem     deploy.bat models     只推模型（换了模型时用）
rem     deploy.bat apk        只装 APK（改了代码时用）
rem ===========================================================================

set "HERE=%~dp0"
set "SDK=C:\Users\DosGo\AppData\Local\Android\Sdk"
set "ADB=%SDK%\platform-tools\adb.exe"
set "PKG=com.dosgo.voicegw"
set "EXT=/sdcard/Android/data/%PKG%/files"

set "APK=%HERE%app\build\outputs\apk\debug\app-debug.apk"
set "ASR_SRC=%HERE%..\server\models\asr\sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17"
set "TTS_SRC=%HERE%models\vits-icefall-zh-aishell3"

if not exist "%ADB%" (
  echo [x] 找不到 adb：%ADB%
  echo     请确认 Android SDK 路径，或改脚本里的 SDK 变量
  exit /b 1
)

echo === 检查设备 ===
"%ADB%" devices
for /f "skip=1 tokens=1,2" %%a in ('"%ADB%" devices') do (
  if "%%b"=="device" set "FOUND=%%a"
)
if not defined FOUND (
  echo.
  echo [x] 没有已授权的设备
  echo     1. 手机用数据线连电脑
  echo     2. 手机上打开「开发者选项 - USB 调试」
  echo     3. 手机上点「允许调试」
  exit /b 1
)
echo [ok] 设备 %FOUND%

set "WHAT=%~1"
if "%WHAT%"=="" set "WHAT=all"

rem ---------------------------------------------------------------- 装 APK
if "%WHAT%"=="all" goto :do_apk
if "%WHAT%"=="apk" goto :do_apk
goto :do_models

:do_apk
if not exist "%APK%" (
  echo [x] APK 不存在，先编译：gradlew.bat assembleDebug
  exit /b 1
)
echo.
echo === 安装 APK ===
"%ADB%" install -r "%APK%"
if errorlevel 1 (
  echo [x] 安装失败。如果是签名冲突，先卸载：adb uninstall %PKG%
  exit /b 1
)
echo [ok] 已安装

if "%WHAT%"=="apk" goto :done

:do_models
echo.
echo === 推模型 ===
if not exist "%ASR_SRC%\model.int8.onnx" (
  echo [x] 缺 ASR 模型：%ASR_SRC%\model.int8.onnx
  exit /b 1
)
if not exist "%TTS_SRC%\model.onnx" (
  echo [x] 缺 TTS 模型：%TTS_SRC%\model.onnx
  exit /b 1
)

rem App 必须至少运行过一次，Android 才会创建它的私有目录
"%ADB%" shell mkdir -p %EXT%/asr
"%ADB%" shell mkdir -p %EXT%/tts

echo   ASR model.int8.onnx  ~229MB，要等一会儿...
"%ADB%" push "%ASR_SRC%\model.int8.onnx" %EXT%/asr/model.int8.onnx
"%ADB%" push "%ASR_SRC%\tokens.txt"      %EXT%/asr/tokens.txt

echo   TTS model.onnx / lexicon.txt / tokens.txt ...
"%ADB%" push "%TTS_SRC%\model.onnx"   %EXT%/tts/model.onnx
"%ADB%" push "%TTS_SRC%\tokens.txt"   %EXT%/tts/tokens.txt
"%ADB%" push "%TTS_SRC%\lexicon.txt"  %EXT%/tts/lexicon.txt

echo   TTS 文本规则（数字/日期/电话读法，共 230KB）...
"%ADB%" push "%TTS_SRC%\phone.fst"          %EXT%/tts/phone.fst
"%ADB%" push "%TTS_SRC%\date.fst"           %EXT%/tts/date.fst
"%ADB%" push "%TTS_SRC%\number.fst"         %EXT%/tts/number.fst
"%ADB%" push "%TTS_SRC%\new_heteronym.fst"  %EXT%/tts/new_heteronym.fst

echo.
echo === 手机上现有的模型 ===
"%ADB%" shell ls -l %EXT%/asr %EXT%/tts

:done
echo.
echo 完成。打开 App 点「启动网关」，日志里会出现 ws://手机IP:8765
endlocal
