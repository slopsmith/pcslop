@echo off
rem one.bat TEST [REPEAT]: runs one test REPEAT times, for profiling.
rem TEST is a bench.exe test or one of 7zip coremark scimark whetstone linpack.
set n=%2
if "%n%"=="" set n=1
cd /d "%TEMP%"
echo == %1> COM1
for %%t in (7zip coremark scimark whetstone linpack) do if /i "%1"=="%%t" goto t_%1
"%~dp0bench.exe" COM1 %1 %n%
goto done

:t_7zip
for /l %%i in (1,1,%n%) do "%~dp07za.exe" b 3 -mmt1 -md22 > COM1 2>&1
goto done

:t_coremark
for /l %%i in (1,1,%n%) do "%~dp0coremark.exe" > COM1 2>&1
goto done

:t_scimark
for /l %%i in (1,1,%n%) do "%~dp0scimark2.exe" 0.2 > COM1 2>&1
goto done

:t_whetstone
for /l %%i in (1,1,%n%) do "%~dp0whetstone.exe" 4000 > COM1 2>&1
goto done

:t_linpack
for /l %%i in (1,1,%n%) do "%~dp0linpack.exe" < "%~dp0linpack.in" > COM1 2>&1
goto done

:done
if not "%3"=="nodone" echo BENCH-DONE> COM1
