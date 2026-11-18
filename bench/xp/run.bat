@echo off
rem Runs the whole suite. Every result goes to COM1, which scripts/bench-xp
rem captures with --serial; BENCH-DONE marks the end.
cd /d "%TEMP%"
"%~dp0bench.exe" COM1
for %%t in (7zip coremark scimark whetstone linpack) do call "%~dp0one.bat" %%t 1 nodone
echo BENCH-DONE> COM1
