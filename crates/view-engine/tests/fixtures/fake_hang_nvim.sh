#!/bin/sh
# Test double for the nvim binary: ignores all arguments (including the
# --embed the Engine always passes), never touches stdin/stdout, and blocks
# for a long time. Used to exercise the handshake-timeout path in
# handshake_failure_reaps_child without racing a real nvim process.
#
# `100000` (not `infinity`): BSD sleep (macOS) has no `infinity` unit and
# exits immediately with a usage error on it, while GNU sleep (Linux)
# accepts both; `100000` seconds (~27.7h) is portable to both and still far
# longer than any test's timeout. `exec` replaces the shell's process image
# in place rather than forking a child, so the resulting `sleep` keeps this
# script's own pid -- tests that kill or pgrep for the fixture are killing
# the actual blocked process, not a parent shell with an orphaned grandchild.
exec sleep 100000
