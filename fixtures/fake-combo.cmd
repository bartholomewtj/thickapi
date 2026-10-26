@echo off
setlocal
set "DIR=%~dp0"
echo %* | findstr /C:"--prompt-file" >nul
if %errorlevel%==0 (
  echo rate limit exceeded 1>&2
  exit /b 1
)
type "%DIR%pi_stream.jsonl"
