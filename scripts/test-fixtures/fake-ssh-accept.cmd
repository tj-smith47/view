@echo off
REM A stand-in for an OpenSSH client that connects, authenticates, and runs
REM what it was given, for the hosts whose fixtures must be Windows programs.
REM It reproduces only the exit status a caller diagnosing a failed session
REM reads: the far side ran the command and it succeeded, so whatever failed
REM was not the connection. The POSIX sibling (fake-ssh) really re-parses and
REM runs the command string; nothing here needs it to.
exit /b 0
