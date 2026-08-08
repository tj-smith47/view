# Default keys

Every native feature is reached through a real nvim mapping, registered
after your config has run so `<leader>` is whatever you set `mapleader`
to. `:map`, `maparg()`, and which-key see these exactly as they see your
own mappings, because that is what they are.

The table below is generated from `default_maps()` in
`crates/view-core/src/native/mappings.rs`; a test fails if this page and
the keys view registers disagree.

<!-- generated from default_maps() -->
| key | feature | command |
| --- | --- | --- |
| `<leader>ff` | `picker` | `:View picker files` |
| `<leader>fb` | `picker` | `:View picker buffers` |
| `<leader>fg` | `picker` | `:View picker grep` |
| `<leader>e` | `tree` | `:View tree toggle` |
| `<leader>fm` | `notifications` | `:View notifications history` |

## Turning them off

A default key is registered only for a feature that is on, and only for
features that are on:

```toml
[native]
picker = false
```

With that line in `view.toml`, view registers none of the picker's keys
and whatever your own config mapped `<leader>ff` to keeps working. The
first time view takes a key you had mapped, it tells you so and names the
line above verbatim.

## `:View`

The command is registered whatever you have turned off, so a feature is
always reachable even with no keys at all:

```vim
:View picker files
:View picker grep
:View tree toggle
```

It completes both arguments against every entry point this build has.
