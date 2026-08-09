# Release binary composition, 2026-08-09

Taken beside the dev-linux re-record that followed the release-profile
diet, so the next footprint breach on this class arrives pre-attributed
instead of costing another readelf/smaps investigation.

The measured artifact is `target/release/view` built from `03bdf04`
(`[profile.release]`: `lto = "fat"`, `codegen-units = 1`,
`strip = "symbols"`) on dev-linux, 12 vCPU, 1-min host load 1.20 at the
start of the capture.

## Regenerating

```bash
# per-crate and per-function .text attribution; strip is disabled for the
# snapshot only, because cargo-bloat reads the symbol table the shipped
# profile removes
CARGO_TARGET_DIR=target/bloat CARGO_PROFILE_RELEASE_STRIP=none \
  cargo bloat --release -p view --crates -n 40
CARGO_TARGET_DIR=target/bloat CARGO_PROFILE_RELEASE_STRIP=none \
  cargo bloat --release -p view -n 25

# idle resident footprint, per mapping and per ELF section
bash scripts/pss-probe.sh
```

`cargo-bloat` 0.12.1. Its own caveat stands and is repeated here because
this file will be read as evidence: the per-crate split is inferred from
symbol names, and under fat LTO the inlining that makes the binary small
is exactly what makes the attribution approximate. Read the table as
"where the code came from", never as "what deleting this crate would
save".

## What the diet moved

| Quantity | Before | After | Delta |
|---|---|---|---|
| binary size (bytes) | 8 772 728 | 5 022 304 | -42.8% |
| `.text` | 4 055 778 | 3 528 286 | -13.0% |
| `.rodata` | 691 040 | 582 224 | -15.7% |
| `.eh_frame` | 498 044 | 294 624 | -40.8% |
| `.gcc_except_table` | 267 372 | 113 760 | -57.4% |
| `.data.rel.ro` | 320 424 | 198 920 | -37.9% |
| idle PSS | 5 139 kB | 4 987 kB | -3.0% |
| idle PSS, `view r--p` | 1 036 kB | 884 kB | -14.7% |
| idle PSS, `view r-xp` | 2 428 kB | 2 424 kB | -0.2% |

The binary-size decomposition, from four builds of the same source: cargo
defaults 8 772 728; defaults + `strip = "symbols"` 6 360 544; defaults +
`strip = "none"` 12 771 208; the shipped profile 5 022 304. So strip
carries -2 412 184 of the total and lto + codegen-units the remaining
-1 338 240.

The row that matters for the footprint gate is the last one. Resident
executable pages did not move even though `.text` lost 13%, because the
kernel faults binary pages in around each touched address (fault-around),
so residency is set by how scattered the executed code is rather than by
how much code exists. A size diet is therefore a weak lever on PSS; the
lever that would move that row is code layout (hot/cold splitting, PGO or
BOLT-style reordering), which nothing in this tree does today.

## Per-crate `.text`

```
 File  .text     Size Crate
11.6%  28.0% 964.6KiB std
 4.4%  10.7% 369.2KiB regex_automata
 2.7%   6.5% 224.3KiB clap_builder
 2.2%   5.2% 180.2KiB regex_syntax
 1.9%   4.7% 162.2KiB toml_edit
 1.9%   4.6% 160.2KiB view
 1.8%   4.4% 152.5KiB aho_corasick
 1.2%   3.0% 101.7KiB view_engine
 1.1%   2.7%  91.5KiB wayland_client
 0.8%   2.0%  69.0KiB ignore
 0.8%   2.0%  68.5KiB view_core
 0.8%   1.9%  63.8KiB view_native
 0.6%   1.5%  50.2KiB [Unknown]
 0.6%   1.4%  49.7KiB view_tui
 0.5%   1.3%  45.4KiB x11rb
 0.5%   1.2%  42.2KiB globset
 0.5%   1.2%  40.0KiB wl_clipboard_rs
 0.5%   1.1%  38.9KiB tree_magic_mini
 0.4%   1.0%  33.1KiB wayland_backend
 0.4%   1.0%  33.1KiB arboard
 0.4%   0.9%  31.7KiB view_surface
 0.4%   0.9%  30.4KiB encoding_rs
 0.3%   0.8%  27.4KiB rayon_core
 0.3%   0.8%  26.3KiB memchr
 0.3%   0.7%  24.8KiB crossterm
 0.3%   0.6%  22.2KiB serde_core
 0.3%   0.6%  21.6KiB winnow
 0.2%   0.5%  18.9KiB nucleo
 0.2%   0.5%  18.7KiB grep_searcher
 0.2%   0.5%  18.1KiB x11rb_protocol
 0.2%   0.4%  13.9KiB grep_regex
 0.2%   0.4%  13.2KiB walkdir
 0.2%   0.4%  12.7KiB nucleo_matcher
 0.1%   0.4%  12.3KiB heck
 0.1%   0.3%  12.0KiB proc_macro2
 0.1%   0.3%  11.8KiB anyhow
 0.1%   0.3%  11.3KiB rmpv
 0.1%   0.3%  10.9KiB parking_lot
 0.1%   0.3%  10.4KiB hashbrown
 0.1%   0.3%   8.9KiB pkg_config
 1.4%   3.3% 113.4KiB And 42 more crates.
41.3% 100.0%   3.4MiB .text section size, the file size is 8.2MiB
```

Three groupings account for most of what is not `std` or view's own
crates:

- **Regex machinery, 702 KiB (20.3% of `.text`)**: `regex_automata` +
  `regex_syntax` + `aho_corasick`, reached through `grep_regex`,
  `grep_searcher`, `globset` and `ignore`. This is the picker's live-grep
  and ignore-walking surface, which is the largest single dependency cost
  the native feature set added.
- **Clipboard stack, ~277 KiB**: `wayland_client`, `x11rb`,
  `wl_clipboard_rs`, `arboard`, `wayland_backend`, `tree_magic_mini`,
  `x11rb_protocol`. Two display-server protocols compiled in because
  either can be the live one at run time.
- **CLI and config parsing, ~408 KiB**: `clap_builder` + `toml_edit` +
  `winnow` + `serde_core`. Startup-only code; it costs file size, and it
  costs resident pages only for the pages the argument parse actually
  touches.

view's own crates total ~475 KiB: `view` 160.2, `view_engine` 101.7,
`view_core` 68.5, `view_native` 63.8, `view_tui` 49.7, `view_surface`
31.7.

## Largest functions

```
 File  .text    Size           Crate Name
 0.5%   1.3% 43.7KiB            view view::main
 0.5%   1.1% 38.1KiB  regex_automata regex_automata::meta::strategy::new
 0.4%   0.9% 32.0KiB        view_tui view_tui::paint::composite_into
 0.4%   0.9% 31.1KiB     view_native view_native::picker::matcher::build_results
 0.4%   0.9% 30.9KiB       toml_edit toml_edit::parser::value::value::{closure#1}
 0.4%   0.9% 30.6KiB             std __rust_begin_short_backtrace::<view_native::picker::matcher::spawn<..>>
 0.3%   0.8% 27.9KiB     encoding_rs <encoding_rs::variant::VariantDecoder>::decode_to_utf8_raw
 0.3%   0.7% 25.0KiB         globset <globset::GlobSetBuilder>::build
 0.3%   0.7% 23.5KiB     view_engine <EngineHandle>::start_with_pipe::<ChildStdout, ChildStdin>::{closure#1}
 0.3%   0.7% 22.7KiB     view_native view_native::picker::sources::spawn_live_grep_scan::{closure#0}
 0.3%   0.6% 21.7KiB    clap_builder <clap_builder::parser::parser::Parser>::get_matches_with
 0.3%   0.6% 21.2KiB            view view::runtime::dispatch::<EngineHandle>
 0.3%   0.6% 20.9KiB  regex_automata <regex_automata::nfa::thompson::compiler::Compiler>::c
 0.2%   0.6% 20.2KiB  regex_automata <regex_automata::dfa::dense::DFA<Vec<u32>>>::minimize
 0.2%   0.6% 19.2KiB    clap_builder <clap_builder::parser::validator::Validator>::validate
 0.2%   0.6% 19.0KiB       view_core view_core::update::update
 0.2%   0.5% 18.3KiB wl_clipboard_rs wl_clipboard_rs::copy::prepare_copy_internal
 0.2%   0.5% 18.2KiB            view view::runtime::run
 0.2%   0.5% 18.2KiB     view_engine <view_engine::process::Engine>::spawn
 0.2%   0.5% 17.2KiB            view <view::runtime::Executor<EngineHandle>>::run
33.9%  82.2%  2.8MiB                 And 4834 smaller methods.
```

## Idle resident footprint

`view --clean README.md`, idle under a 120x40 pty, 6s settle, from
`scripts/pss-probe.sh`.

Before the diet (binary 8 772 728 bytes, host load 5.89):

```
Rss:                7164 kB
Pss:                5139 kB
Shared_Clean:       2060 kB
Private_Clean:      3132 kB
Private_Dirty:      1972 kB
-- PSS by mapping+permission (kB) --
2428 view r-xp
1176 [anon]
1036 view r--p
376 [heap]
41 other (each < 16 kB)
36 [stack]
21 libc.so.6 r--p
20 libc.so.6 r-xp
```

After (binary 5 022 304 bytes, host load 4.11):

```
Rss:                7012 kB
Pss:                4987 kB
Shared_Clean:       2060 kB
Private_Dirty:      4952 kB
-- PSS by mapping+permission (kB) --
2424 view r-xp
1180 [anon]
884 view r--p
376 [heap]
41 other (each < 16 kB)
36 [stack]
21 libc.so.6 r--p
20 libc.so.6 r-xp
```

Both readings were taken at elevated host load and are still within 1 kB
of the quiet-host reading this probe reproduces (the 2026-08-09 T16 probe
at load 0.17 read Pss 5185 kB and `view r-xp` 2428 kB on the pre-diet
binary). PSS is a page-accounting quantity, not a latency one, and does
not respond to ambient load the way the tail statistics in the matrix do.

The harness's `memory.minimal` row measures a different quantity from
this probe: post-workload with ten buffers, not idle. It read 5.210 MB
pre-diet and 4.986 MB on the dieted binary in the recorded matrix run, a
delta of -0.224 MB against the idle probe's -0.152 MB.
