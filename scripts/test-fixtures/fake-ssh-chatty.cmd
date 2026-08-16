@echo off
REM A stand-in for an OpenSSH client whose connection is refused behind a very
REM long banner, for the hosts whose fixtures must be Windows programs rather
REM than POSIX shell scripts. Same behaviour as its shell-script sibling: a
REM banner larger than a pipe buffer, then the refusal on the last line. A
REM client captured through a pipe nobody drains blocks on its own write at
REM that size and never reaches the exit its diagnosis is read after.
setlocal
for /L %%i in (1,1,2000) do >&2 echo view-test-host: banner line %%i, padded so this fixture writes more than a pipe will hold
>&2 echo view-test-host: Permission denied (publickey).
exit /b 255
