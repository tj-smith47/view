#!/usr/bin/env bash
# Where a script under scripts/ makes the files it throws away. Sourced,
# never run.
#
# Never `/tmp`: on this host it is a small tmpfs every job in parallel
# shares, and these scripts write plugin trees, XDG homes and pane dumps a
# run at a time. The job's own scratch directory where a harness set one,
# the user's cache otherwise -- both survive a full `/tmp` and neither is
# shared with a job that is about to clear it.
#
# One definition rather than the line each script used to write for itself.
# A temp file made with no template of its own lands wherever `TMPDIR`
# happened to point, which is `/tmp` on every host here;
# `check_temp_roots` in `scripts/check-style.sh` is what refuses one.

# The directory, made if it is not there. Fails rather than answering with a
# path nothing can be written to.
scratch_root() {
    local root=${CLAUDE_JOB_DIR:+$CLAUDE_JOB_DIR/tmp}
    root=${root:-$HOME/.cache/view}
    mkdir -p "$root" || return 1
    printf '%s\n' "$root"
}
