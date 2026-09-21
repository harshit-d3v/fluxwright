@echo off
set PATH=%USERPROFILE%\.cargo\bin;%PATH%
cd /d %~dp0
echo Building fluxwright-mcp (release)...
cargo install --path crates/fluxwright-mcp --force
echo.
echo Installed: %USERPROFILE%\.cargo\bin\fluxwright-mcp.exe
echo.
echo Claude Desktop:  %%APPDATA%%\Claude\claude_desktop_config.json
echo Codex:           %%USERPROFILE%%\.codex\config.toml
echo Cursor:          .cursor\mcp.json (already in this repo)
echo.
echo See crates\fluxwright-mcp\README.md
pause
