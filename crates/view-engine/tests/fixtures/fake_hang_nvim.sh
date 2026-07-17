#!/bin/sh
# Test double for the nvim binary: ignores all arguments (including the
# --embed the Engine always passes), never touches stdin/stdout, and blocks
# forever. Used to exercise the handshake-timeout path in
# handshake_failure_reaps_child without racing a real nvim process.
exec sleep infinity
