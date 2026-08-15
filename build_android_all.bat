@echo off
REM Android SDK/NDK paths
SET ANDROID_HOME=C:\Users\jkene\AppData\Local\Android\Sdk
SET NDK_HOME=C:\Program Files\Android\android-ndk-r27d
SET PATH=%PATH%;C:\Users\jkene\AppData\Local\Android\Sdk\platform-tools
SET JAVA_HOME=C:\Program Files\Android\jdk-17.0.12
 
echo.
echo Building PDFMaker for Android (all architectures)...
echo ANDROID_HOME=%ANDROID_HOME%
echo NDK_HOME=%NDK_HOME%
echo JAVA_HOME=%JAVA_HOME%
echo.

REM Build for each architecture
REM Note: Tauri builds each arch separately then Gradle combines them

echo.
echo === Building armeabi-v7a (armv7) ===
cargo tauri android build --target armv7
if errorlevel 1 goto :build_failed

echo.
echo === Building x86 (i686) ===
cargo tauri android build --target i686
if errorlevel 1 goto :build_failed

echo.
echo === Building x86_64 ===
cargo tauri android build --target x86_64
if errorlevel 1 goto :build_failed

echo.
echo === Building arm64-v8a (aarch64) ===
cargo tauri android build --target aarch64
if errorlevel 1 goto :build_failed

echo.
echo ============================================================
echo BUILD COMPLETE!
echo ============================================================
echo.
echo AAB file location:
echo   gen\android\app\build\outputs\bundle\release\app-release.aab
echo.
echo APK files location:
echo   gen\android\app\build\outputs\apk\release\
echo.
pause
exit /b 0

:build_failed
echo.
echo BUILD FAILED!
echo.
pause
exit /b 1
