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

## `nvim --api-info` (msgpack-RPC metadata, decoded)

The brief's own mandated source, captured directly rather than recalled: `nvim
--api-info` writes the same `nvim_get_api_info` metadata `Engine::spawn`'s
handshake decodes, as msgpack on stdout. Decoded here with `python3 -m
msgpack` for a readable diff against the two claims below; the bytes
themselves are exactly what `EngineHandle`'s own msgpack-rpc reader parses at
spawn time.

```
$ nvim --api-info > api-info.mpack
$ python3 -c '
import msgpack, json
with open("api-info.mpack", "rb") as f:
    data = msgpack.unpackb(f.read(), raw=False, strict_map_key=False)
meta = data[1] if isinstance(data, list) else data
for fn in meta["functions"]:
    if fn["name"] in ("nvim_command", "nvim_ui_attach"):
        print(json.dumps(fn, indent=2))
'
{
  "parameters": [["String", "cmd"]],
  "since": 1,
  "method": false,
  "return_type": "void",
  "name": "nvim_command"
}
{
  "parameters": [["Integer", "width"], ["Integer", "height"], ["Dict", "options"]],
  "since": 1,
  "method": false,
  "return_type": "void",
  "name": "nvim_ui_attach"
}
```

Engine identity from the same capture (`meta["version"]`), matching
`.engine-pin` and the `nvim --version` capture above:

```json
{"major": 0, "minor": 12, "patch": 4, "prerelease": false, "api_level": 14, "api_compatible": 0, "api_prerelease": false, "build": null}
```

Backs the two claims this project makes elsewhere without their own committed
capture:

- `nvim_api.rs`'s `command`/`request_timeout` doc comment claims
  `nvim_command(String command) -> nil` was "verified against the pinned
  engine's own `api_info`": the capture above confirms the parameter list
  (`String cmd`), and `"return_type": "void"` is the msgpack-RPC metadata's
  own spelling of a reply whose value is `nil` -- `void` functions still
  return one reply message, just with a `nil` result, which is what every
  `request`-based caller in this codebase (`command`, `eval_str`, ...) reads.
- The `stdin_fd` UI-attach option this document's `:help` captures describe:
  `nvim_ui_attach`'s own third parameter is an opaque `Dict` named
  `options`, not individually-enumerated keys, so `--api-info` cannot name
  `stdin_fd` any more specifically than that -- confirming structurally
  that it is passed through this call's options map (exactly what
  `EngineHandle::ui_attach_with_stdin_relay` does), while the `:help
  ui-startup-stdin` and `:help ui-ext-options` captures above are what name
  and define `stdin_fd` itself, since `--api-info` documents the RPC
  surface's shape, not the semantics of an arbitrary dict key within it.

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
  Unix. Off Unix, nvim does **not** fall back to reading its own inherited
  stdin the way a plain `nvim -` invocation would: `build_command` pipes the
  child's fd 0 unconditionally as the `--embed` RPC channel, so a `-`
  combined with piped content there would have nvim read that RPC stream
  itself as buffer text -- corrupting the channel `view` talks to it over,
  not merely doing nothing. `main::deny_unsupported_stdin_relay` refuses to
  start at all in that combination instead, with a clear error naming the
  limitation.
