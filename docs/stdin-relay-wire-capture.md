# Wire capture: `stdin_fd` / `ui-startup-stdin` contract

Captured live against the pinned engine per "capture, never recall." Source
of truth for the CLI's stdin relay (`ls | view -`, Task 7).

## Engine identity

```
$ nvim --version | head -3
NVIM v0.12.4
Build type: Release
LuaJIT 2.1.1785192264
```

Matches `.engine-pin` (`v0.12.4`) exactly.

## `:help ui-startup-stdin` (verbatim, from `api-ui-events.txt`)

Captured via `nvim --headless -c "help ui-startup-stdin" -c "write! <out>" -c "qa!"`.

```
						   *ui-startup-stdin*
UIs can support reading from stdin (like `command | nvim -`, see |--|) as follows:

1. The embedding process detects that the stdin fd is not a terminal.
2. It then needs to forward this fd to Nvim. Because fd=0 is already is used
   to send RPC data from embedder to Nvim, it must use some other file
   descriptor, like fd=3 or higher.
3. Then pass the fd as the `stdin_fd` parameter of `nvim_ui_attach`. Nvim will
   read it as text into buffer 1.
```

## `:help ui-ext-options` (`stdin_fd` entry, same page)

```
- `stdin_fd`		Treat this fd as if it were stdin when using |--|.
			Only from |--embed| UI on startup. |ui-startup-stdin|
```

## `:help -` (verbatim, from `starting.txt`)

Captured via `nvim --headless -c "help -" -c "write! <out>" -c "qa!"`.

```
						  *--*
`-`               Alias for stdin (standard input).
                Example: >
                        echo text | nvim - file
<                "text" is read into buffer 1, "file" is opened as buffer 2.
                In most cases (except -s, -es, |--embed|, --headless) if stdin
                is not a TTY then it is read as text, so "-" is implied: >
                        echo text | nvim file
```

## Empirical resolution (spawned pinned nvim via `Engine::spawn`)

The doc text names the mechanism (a real fd, 3 or higher, named through
`stdin_fd`); the exact wiring was proven against the pinned binary rather
than assumed, in `crates/view/tests/cli_live.rs`'s
`piped_stdin_lands_in_the_first_buffer_via_the_relay_fd`:

1. A readable fd (a regular file opened over known content, standing in for
   the read end of a shell pipe -- the dup2 mechanism does not care which)
   is duplicated onto the child's fd 3 via a `pre_exec` closure
   (`view-engine`'s `relay_stdin_fd`, `std::os::unix::process::CommandExt`).
2. `nvim_ui_attach` is called with `stdin_fd: 3` in its options map
   (`EngineHandle::ui_attach_with_stdin_relay`) alongside `-` in the child's
   own argv (`EngineConfig::extra_args`, from the CLI's passthrough).
3. `getline(1)` on the resulting session returns the fd's content verbatim.

```
getline(1) = "hello from the pipe"
```

## Conclusions for the implementation

- Child fd 0 is `--embed`'s own RPC channel and cannot double as the piped
  content's descriptor; the relay must land on a distinct fd (this
  implementation fixes it at 3, `view_engine::nvim_api::STDIN_RELAY_CHILD_FD`)
  and must still forward `-` as an ordinary passthrough argument -- the
  `stdin_fd` option alone does not imply `-` was given, and `-` alone with no
  `stdin_fd` set makes nvim read its own fd 0, which is the RPC channel here.
- `stdin_fd` is accepted only on the same `nvim_ui_attach` call that performs
  startup UI attach ("Only from `--embed` UI on startup"), which is why the
  CLI adds a second attach method (`ui_attach_with_stdin_relay`) rather than
  a follow-up call after the ordinary `ui_attach`.
- The relay is Unix-only (`std::os::unix::process::CommandExt::pre_exec`);
  `EngineConfig::stdin_relay_requested` returns `false` unconditionally off
  Unix, and `-` still reaches the engine as a literal passthrough argument
  there, unchanged from ordinary forwarding (nvim then reads its own
  inherited stdin, exactly as an ordinary `nvim -` invocation would).
