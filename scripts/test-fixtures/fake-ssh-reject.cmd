@echo off
REM A stand-in for an OpenSSH client whose connection is refused, for the
REM hosts whose fixtures must be Windows programs rather than POSIX shell
REM scripts. Same behaviour as its shell-script sibling: under BatchMode the
REM client answers a prompt it cannot ask by giving up at once, writing its
REM own diagnosis and exiting 255 -- the status OpenSSH reserves for its own
REM failures, never the exit status of anything on the far side. Nothing runs
REM on the far side here, which is the point of a refused connection.
>&2 echo view-test-host: Permission denied (publickey).
exit /b 255
