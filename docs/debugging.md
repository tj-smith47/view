# Debugging

view writes two logs on request. Each is turned on by an environment variable
that names a file, and view appends to that file for the whole session. With
the variable unset, view writes nothing.

When view cannot open the file, it prints one line on stderr before the screen
starts and runs with that log off:

```text
view: cannot open VIEW_LOG path /no/such/dir/view.log: No such file or directory (os error 2), diagnostic logging disabled
```

## Recording what view does

Set `VIEW_LOG` to a file path, and view appends one line for each step of
startup, each engine event, each layout change and each exit step. A line holds
the milliseconds since view started, a topic and the detail:

```text
7 startup shell frame painted 7.261486ms after process start
35 engine spawned pid=2176168 stdin_relay=false
35 engine attach ok pid=2176168 channel=1 api=0.12
```

```sh
VIEW_LOG=~/view.log view src/main.rs
```

Attach this file to a bug report about startup, a crash or an engine restart.

## Recording the redraw sequence

Set `VIEW_REDRAW_LOG` to a file path, and view appends one numbered line for
each of these:

| line | records |
|---|---|
| `grid_resize` | nvim sizes a grid |
| `grid_line` | nvim writes a run of cells on one row of a grid |
| `grid_scroll` | nvim scrolls a region of a grid |
| `grid_clear` | nvim clears a grid |
| `grid_destroy` | nvim frees a grid |
| `win_pos` | nvim places a window's grid |
| `win_float_pos` | nvim places a floating window's grid |
| `win_hide` | nvim hides a window's grid |
| `win_close` | nvim closes a window |
| `flush` | nvim ends a batch of screen updates |
| `try_resize` | view asks nvim to resize the whole screen |
| `try_resize_grid` | view asks nvim to resize one grid |
| `resized` | the terminal changes size |
| `held_screen` | an engine restart holds or releases the screen |
| `drain` | view takes a batch of updates to draw |

```text
711 grid_resize grid=1 width=148 height=37
713 win_pos grid=7 win=1004 row=0 col=0 width=30 height=36
804 try_resize_grid grid=7 width=28 height=34
```

```sh
VIEW_REDRAW_LOG=~/redraw.log view src/main.rs
```

The numbers run in the order the lines were written, across every thread. When
a write to the file fails, the log stops at that line, and view names the line
on exit. Attach this file to a bug report about text drawn in the wrong place,
or left behind after a window or the terminal changed size.

The two variables can be set together.
