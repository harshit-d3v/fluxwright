@echo off
set PATH=%USERPROFILE%\.cargo\bin;%PATH%
cd /d %~dp0
echo Opening Chrome and searching Amazon for iPhone 13-18...
cargo run -p fluxwright --example amazon_iphones
echo.
pause
