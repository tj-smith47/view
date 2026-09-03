# anodizer release capture

What the installed anodizer actually does, captured from the tool before
`.anodizer.yaml` was written. A release config written from recall is the
same defect class as a hand-written download URL that re-derives names the
tool already knows: it agrees with reality until the tool moves.

Captured 2026-09-03 on dev-linux. Every fenced block below is the tool's own
output, with one substitution: the U+2014 dash anodizer prints in its status
lines is rendered `--`, because `scripts/check-style.sh` bans that character
from `docs/` and `README.md`.

## Version

```
$ anodizer --version
anodizer 0.23.0
```

## Commands

```
$ anodizer --help
Release Rust projects with ease

Usage: anodizer [OPTIONS] [COMMAND]

Commands:
  release      Run the full release pipeline. Re-running the identical command converges on
               already-published state instead of double-publishing, so a re-run is how a failed
               release is recovered
  build        Build binaries only (always runs in snapshot mode)
  check        Validate configuration and run determinism checks
  init         Generate starter config, or enroll version-bearing files
  changelog    Manage CHANGELOG.md: refresh the pending section, or render notes/JSON
  completion   Generate shell completions
  healthcheck  Check availability of required external tools
  preflight    Verify the environment can run the configured release: required tools, env
               vars/secrets (presence only -- values are never printed), endpoint reachability,
               docker daemon, and loadable key material, all derived from the resolved config.
  man          Generate man pages to stdout
  jsonschema   Output JSON Schema for .anodizer.yaml
  resolve-tag  Resolve a git tag to its matching crate in the config
  targets      Emit the configured build targets as a GitHub Actions matrix
  vocabulary   Emit the canonical `--skip` / `--publishers` token vocabulary
  tools        Emit the external CLI tools the resolved config's pipeline will invoke
  tag          Auto-tag based on commit message directives
  continue     Resume a release after a transient failure or after `--prepare`/`--split`
  publish      Run only the publish stages (release, blob, publish) from a completed dist/
  promote      Promote an already-published artifact from a pre-release track to a stable track
  bump         Bump crate versions (Conventional Commits -> semver level)
  announce     Run only the announce stage from a completed dist/
  notify       Send a notification through configured announce integrations
  mcp          MCP server management
```

Two of the repo's `release:*` task targets named commands this version does
not have. `anodizer check` now requires a subcommand (`check config`), and
`anodizer verify` is gone; `Taskfile.yml` was corrected to `anodizer check
config` and `anodizer preflight` in the same change as this capture.

## Schema

`anodizer jsonschema` emits 616 KB of JSON Schema for `.anodizer.yaml`. The
whole document has **no required top-level field**, and none of the blocks
this config uses (`defaults`, `crates`, `release`, `changelog`) declares a
required key either:

```
$ anodizer jsonschema | python3 -c "import json,sys; s=json.load(sys.stdin); print('title:', s['title']); print('required:', s.get('required'))"
title: Config
required: None
```

Required keys appear only inside blocks this config does not use, e.g.
`SnapshotConfig` requires `version_template` and `ExtraFileSpec`'s object
form requires `glob`.

## What 0.23.0 derives, and what the config still carries

Derived, and therefore absent from `.anodizer.yaml`:

| Field | Derived from | Observed |
| --- | --- | --- |
| `project_name` | the crate's `Cargo.toml` | `inferred project_name 'probe' from Cargo.toml` |
| version | the crate's `Cargo.toml` / the git tag | `no git tags found, defaulting to v0.0.0 (snapshot mode)` |
| `dist` | default `./dist` | `wrote ./dist/metadata.json` |
| archive names | `{ProjectName}_{Version}_{Os}_{Arch}` | `creating ./dist/probe_0.0.0-SNAPSHOT-none_linux_amd64.tar.gz` |
| checksum name and algorithm | `{ProjectName}_{Version}_checksums.txt`, sha256 | `combined checksums -> ./dist/probe_0.0.0-SNAPSHOT-none_checksums.txt` |
| release repo | the git remote | no `release.github` block is needed |
| tag template | `CrateConfig` default | no `tag_template` is needed |
| runner OS per triple | the triple itself | see `anodizer targets` below |
| cross strategy | `auto` | picks `cargo zigbuild` when a triple is not the host's |

Carried, because nothing can derive it:

| Field | Why |
| --- | --- |
| `defaults.targets` | a Rust workspace names no release triples anywhere |
| `crates[].name` / `path` | eleven members compile, one ships, and the manifest carries no marker for which |
| `crates[].depends_on: []` | anodizer derives ordering from path dependencies and then requires each one to be listed as a released crate |
| `archives: false` | the archiver cannot express the bundled layout, see below |
| `extra_files` | the bundles are produced outside anodizer |
| `changelog` groups and filters | the conventional-prefix to section mapping is this repo's convention |

## `anodizer targets`: the matrix comes from the config

```
$ anodizer targets
OS (runner)          TARGET                                   ARTIFACT
ubuntu-latest        x86_64-unknown-linux-gnu                 dist-Linux
ubuntu-latest        aarch64-unknown-linux-gnu                dist-Linux
macos-latest         x86_64-apple-darwin                      dist-macOS
macos-latest         aarch64-apple-darwin                     dist-macOS
windows-latest       x86_64-pc-windows-msvc                   dist-Windows
```

```
$ anodizer targets --json
{"include":[{"os":"ubuntu-latest","target":"x86_64-unknown-linux-gnu","artifact":"dist-Linux"},{"os":"ubuntu-latest","target":"aarch64-unknown-linux-gnu","artifact":"dist-Linux"},{"os":"macos-latest","target":"x86_64-apple-darwin","artifact":"dist-macOS"},{"os":"macos-latest","target":"aarch64-apple-darwin","artifact":"dist-macOS"},{"os":"windows-latest","target":"x86_64-pc-windows-msvc","artifact":"dist-Windows"}]}
```

`.github/workflows/release.yml` feeds that JSON straight into
`strategy.matrix`, so the platform list has exactly one definition.

## Why the archiver does not build the bundle

The shipped artifact is a prefix, not a flat directory: the editor sits at
`bin/view` and resolves its engine at `libexec/view/` two levels up from
itself. Four probes against a throwaway crate established that anodizer's
archiver cannot express that prefix.

`archives[].files[].src` **is** template-expanded, and `dst` is a directory
prefix (`dst: libexec/view` with a file `src` yields `libexec/view/nvim`;
`dst: libexec/view/nvim` yields `libexec/view/nvim/nvim`). A directory `src`
with no glob is dropped. That much works. The binary is the problem:

- `strip_binary_directory: false` still placed the binary at the archive
  root.
- `ids: ["no-such-build"]` skipped the whole archive rather than producing a
  binary-free one: `skipped archives[a] -- crate probe has no binaries
  matching ids ["no-such-build"] (set meta: true if this is intentional)`.
- `meta: true` archives are built once for the whole crate, not once per
  target: `meta archive for crate 'probe' target 'unknown' has zero files`.
  A per-target engine cannot ride one.
- `wrap_in_directory` accepts a template and did place the binary under
  `probe-0.0.0-.../bin`, but a `dst` climbing back out of it is refused:
  `tar.gz: adding engine-stage/.../nvim as probe-.../bin/../libexec/view/nvim`.

Build hooks were the remaining escape and are not one: `{{ .Target }}` and
`{{ Target }}` both render empty in a `builds[].hooks.post` command, and a
hook's environment carries no target variable either (it inherits the parent
environment wholesale, which is a second reason this pipeline runs no hooks
in CI, where that parent holds the release secrets).

So `scripts/package-bundle.sh` builds the archives and anodizer publishes
them. What anodizer still does is the part only it does: the changelog from
commit subjects, the checksum file, and the GitHub release.

## Verbatim dry run of the shape this repo uses

`archives: false`, bundles handed in through `extra_files`, build and archive
stages skipped:

```
$ anodizer release --snapshot --skip build,archive
   Preparing release
     Warning no git tags found, defaulting to v0.0.0 (snapshot mode).
   Archiving source
     - skipped source archive -- not enabled
  Cataloging dependencies
     - skipped SBOM -- none configured
   Computing checksums
     - combined checksums -> ./dist/probe_0.0.0-SNAPSHOT-none_checksums.txt
   Verifying release
     - verify-release skipped: disabled by config
     Summary
     - publishers  none ran (publish stages did not run)
  Finalizing
     - wrote ./dist/metadata.json
     - wrote ./dist/artifacts.json

$ cat dist/probe_0.0.0-SNAPSHOT-none_checksums.txt
5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03  probe-v1-linux.tar.gz
98ea6e4f216f2fb4b69fff9b3a44842c38686ca685f3f55dc48c5d3fb1107be4  probe-v1-mac.tar.gz
```

The bundles must live outside `dist/`: anodizer refuses to start against a
non-empty one (`dist directory './dist' is not empty; use --clean to remove
it first`), which is why the workflow stages them under `bundles/`.

This repo's own config validates against the installed version:

```
$ anodizer check config
   - validating configuration
   - Config is valid.
```

## Signing

`signs[].artifacts` accepts `all`, `archive`, `binary`, `checksum`,
`package` and `sbom`. None of those names covers `release.extra_files`, and
whether `all` reaches them cannot be established locally: keyless cosign
needs an OIDC token only a GitHub Actions run has. Rather than ship a
`signs:` block whose coverage is a guess, the release workflow signs each
bundle with `cosign sign-blob` and then verifies every one of them with
`cosign verify-blob` in the same job, before anodizer is invoked at all. A
signature nobody verified before upload is a signature the first user
discovers is broken.

`anodizer healthcheck` confirms cosign is a tool the pipeline can see:

```
$ anodizer healthcheck
   - Anodizer Environment Health Check
   - ========================================
   - OK cargo                Rust package manager (cargo 1.98.0 (797e8a9bc 2026-08-05))
   - OK git                  Version control (git version 2.53.0)
   - OK cosign               Sigstore container signing (GitVersion:    v2.4.3)
   - OK gpg                  GNU Privacy Guard (signing) (gpg (GnuPG) 2.4.8)
```

## The engine's own prefix, and the parsers

The pinned engine's release assets unpack to a prefix, and the packaging
takes three things out of it:

```
nvim-linux-x86_64/bin/nvim
nvim-linux-x86_64/lib/nvim/parser/{c,lua,markdown,markdown_inline,query,vim,vimdoc}.so
nvim-linux-x86_64/share/nvim/runtime/
```

nvim derives the parser directory from its own `argv[0]`, so lifting the
binary to `libexec/view/nvim` loses every bundled parser while leaving the
editor otherwise working. Measured against the pinned engine, with
`$VIMRUNTIME` exported exactly as the editor exports it:

```
$ # engine left in its own prefix
PARSE ok=true translation_unit

$ # engine at libexec/view/nvim, parsers left at libexec/view/lib/nvim/parser
PARSE ok=false .../treesitter/languagetree.lua:132: No parser for language "c"

$ # same, with lib/nvim/parser copied onto $VIMRUNTIME/parser
PARSE ok=true translation_unit
```

The packaging therefore copies `lib/nvim/parser` onto the runtime
directory, which is on `runtimepath` and searched wherever the binary sits.
The Windows asset needs the same treatment for a different reason: its
`bin/` holds `lua51.dll`, `DbgHelp.dll` and `win32yank.exe` beside
`nvim.exe`, and `nvim.exe` does not start without them. The script copies
the whole of `bin/` into `libexec/view/` for that reason, on every platform.
