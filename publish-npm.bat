@echo off
set PATH=%USERPROFILE%\.cargo\bin;%PATH%
cd /d %~dp0bindings\node
echo Packing fluxwright for npm (dry run first)...
call npm pack --dry-run
echo.
echo To publish (you must be logged in: npm login):
echo   npm publish --access public
echo.
pause
