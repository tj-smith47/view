# Recording the README pictures

Each animation and the screenshot on the README is recorded by one script under
`scripts/dogfood/tapes/`. The script runs view under your own config inside
tmux and records the pane with vhs.

## What a recording needs

| tool | version |
|---|---|
| vhs | `v0.11.0-view1`, built by `scripts/dogfood/vhs/build.sh` |
| tmux | 3.7 or later |
| font | JetBrainsMono Nerd Font |

`scripts/dogfood/vhs/build.sh` downloads vhs v0.11.0, applies
`scripts/dogfood/vhs/view-capture.patch` and installs the result as
`~/.local/bin/vhs`. Stock vhs reads the cursor and the text of each frame in two
separate calls, and a frame drawn between them records the previous cursor over
the new text, so the patch reads both in one call.

```sh
scripts/dogfood/vhs/build.sh
```

`~/.local/bin` goes ahead of any other vhs on your `PATH`. A tape stops before
it records when its vhs reports another version or its tmux is older than 3.7,
and it names the binary and the version it found.

## Recording one

```sh
task build
scripts/dogfood/tapes/tiled-panes.sh
```

A tape writes over the file the README shows, or to the path given as its first
argument.
