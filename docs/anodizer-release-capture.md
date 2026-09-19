# anodizer release capture

What the installed anodizer actually does, captured from the tool before
`.anodizer.yaml` was written. A release config written from recall is the same
defect class as a hand-written download URL that re-derives names the tool
already knows: it agrees with reality until the tool moves.

Captured 2026-09-03 on dev-linux. Every fenced block below is the tool's own
output, byte for byte, with exactly one substitution: the U+2014 dash anodizer
prints inside its prose is written `--`, because `scripts/check-style.sh` bans
that character from `docs/` and `README.md`. Nothing else is changed: the
status bullets, check marks and arrows the tool prints are reproduced as it
prints them, and no line is elided. Blocks needing no substitution at all are
marked where that is the case.

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
               docker daemon, and loadable key material, all derived from the resolved config. Every
               failure is reported in one pass and the exit code is non-zero when anything is
               missing. The same checks run automatically at the start of `anodizer release`. Also
               prints the per-publisher reconcile table (is the target version already published?);
               only a required publisher's content divergence exits non-zero -- an already-complete
               or unreachable publisher does not
  man          Generate man pages to stdout
  jsonschema   Output JSON Schema for .anodizer.yaml
  resolve-tag  Resolve a git tag to its matching crate in the config
  targets      Emit the configured build targets as a GitHub Actions matrix
  vocabulary   Emit the canonical `--skip` / `--publishers` token vocabulary
  tools        Emit the external CLI tools the resolved config's pipeline will invoke
  tag          Auto-tag based on commit message directives
  continue     Resume a release after a transient failure or after `--prepare`/`--split`
  publish      Run only the publish stages (release, blob, publish) from a completed dist/
  promote      Promote an already-published artifact from a pre-release track to a stable track,
               without rebuilding
  bump         Bump crate versions (Conventional Commits → semver level)
  announce     Run only the announce stage from a completed dist/
  notify       Send a notification through configured announce integrations
  mcp          MCP server management
  help         Print this message or the help of the given subcommand(s)

Options:
  -f, --config <CONFIG>  Path to config file (overrides auto-detection)
      --verbose          Enable verbose output
      --debug            Enable debug output
  -q, --quiet            Suppress non-error output
      --strict           Strict mode: configured features that silently skip become hard errors
  -h, --help             Print help
  -V, --version          Print version
```

Two of the repo's `release:*` task targets named commands this version does not
have. `anodizer check` now requires a subcommand (`check config`), and
`anodizer verify` is gone. `Taskfile.yml` was corrected in the same change as
this capture: `release:check` runs `anodizer check config`, `release:verify`
became `release:preflight` running `anodizer preflight` (the command that
actually exists, named for what it does), and `release:publish` was removed,
because publishing is what pushing a tag does and a local target for it would
upload artifacts that never passed the workflow's signing and verification.

## Schema

`anodizer jsonschema` emits 616 KB of JSON Schema for `.anodizer.yaml`. The
whole document has **no required top-level field**, and none of the blocks this
config uses (`defaults`, `crates`, `release`, `changelog`) declares a required
key either:

```
$ anodizer jsonschema | python3 -c "import json,sys; s=json.load(sys.stdin); print('title:', s['title']); print('required:', s.get('required'))"
title: Config
required: None
```

Required keys appear only inside blocks this config does not use, e.g.
`SnapshotConfig` requires `version_template` and `ExtraFileSpec`'s object form
requires `glob`.

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

`.github/workflows/release.yml` does not run that command itself. The
first-party `tj-smith47/anodizer-action@v1` already exposes it: an
`install-only: true` step emits the same JSON as its `split-matrix` output,
which the workflow feeds to `strategy.matrix`. That is the mechanism the
sibling repos use to install and run the tool, and the version comes from the
same `vars.ANODIZER_VERSION` repo variable they read, so the platform list has
exactly one definition and the tool has exactly one install path.

## Why the archiver does not build the bundle

The shipped artifact places files under a shared prefix: the editor sits at
`bin/view` and resolves its engine at `libexec/view/` two levels up from
itself. Four probes against a throwaway crate established that anodizer's
archiver cannot express that prefix.

`archives[].files[].src` **is** template-expanded, and `dst` is a directory
prefix (`dst: libexec/view` with a file `src` yields `libexec/view/nvim`;
`dst: libexec/view/nvim` yields `libexec/view/nvim/nvim`). A directory `src`
with no glob is dropped. That much works. The binary is the problem:

- `strip_binary_directory: false` still placed the binary at the archive root.
- `wrap_in_directory` accepts a template and did place the binary under
  `probe-0.0.0-SNAPSHOT--x86_64-unknown-linux-gnu/bin`, but a `dst` climbing
  back out of it is refused outright.
- `ids: ["no-such-build"]` skipped the whole archive and produced no
  binary-free one.
- `meta: true` archives are built once for the whole crate, with no target, so
  no per-target engine can ride one.

```
$ anodizer release --snapshot
    Creating archives
     • creating ./dist/A_x86_64-unknown-linux-gnu.tar.gz
       Error archive failed: tar.gz: adding engine-stage/x86_64-unknown-linux-gnu/nvim as probe-0.0.0-SNAPSHOT--x86_64-unknown-linux-gnu/bin/../libexec/view/nvim
       Error tar.gz: adding engine-stage/x86_64-unknown-linux-gnu/nvim as probe-0.0.0-SNAPSHOT--x86_64-unknown-linux-gnu/bin/../libexec/view/nvim

$ anodizer release --snapshot   # with that entry removed
    Creating archives
     Warning skipped archives[b] -- crate probe has no binaries matching ids ["no-such-build"] (set `meta: true` if this is intentional)
       Error archive failed: archive: meta archive for crate 'probe' target 'unknown' has zero files. Check your `files:` patterns -- meta archives must bundle at least one file.
       Error archive: meta archive for crate 'probe' target 'unknown' has zero files. Check your `files:` patterns -- meta archives must bundle at least one file.
```

Build hooks were the remaining escape and are not one: `{{ .Target }}` and
`{{ Target }}` both render empty in a `builds[].hooks.post` command, and a
hook's environment carries no target variable either (it inherits the parent
environment wholesale, which is a second reason this pipeline runs no hooks in
CI, where that parent holds the release secrets).

So `scripts/package-bundle.sh` builds the archives and anodizer publishes them.
What anodizer still does is the part only it does: the changelog from commit
subjects, the checksum file, and the GitHub release.

## Verbatim dry run of the shape this repo uses

`archives: false`, bundles handed in through `extra_files`, build and archive
stages skipped:

```
$ anodizer release --snapshot --skip build,archive
     Warning git tag --points-at exited non-zero; returning no tags
   Preparing release
     Warning error finding tags matching template: git tag --list failed: fatal: not a git repository (or any of the parent directories): .git
     Warning no git tags found, defaulting to v0.0.0 (snapshot mode).
   Archiving source
     • skipped source archive -- not enabled
  Cataloging dependencies
     • skipped SBOM -- none configured
   Computing checksums
     • combined checksums → ./dist/probe_0.0.0-SNAPSHOT-_checksums.txt
   Verifying release
     • verify-release skipped: disabled by config
     Summary
     • publishers  none ran (publish stages did not run)
     • run flags   submitter_gated=false announce_gated=false
  Finalizing
     • wrote ./dist/metadata.json
     • wrote ./dist/artifacts.json

$ cat dist/probe_0.0.0-SNAPSHOT-_checksums.txt
5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03  probe-v1-linux.tar.gz
98ea6e4f216f2fb4b69fff9b3a44842c38686ca685f3f55dc48c5d3fb1107be4  probe-v1-mac.tar.gz
```

The bundles must live outside `dist/`: anodizer refuses to start against a
non-empty one
(`dist directory './dist' is not empty; use --clean to remove it first`), which
is why the workflow stages them under `bundles/`.

This repo's own config validates against the installed version:

```
$ anodizer check config
   - validating configuration
   - Config is valid.
```

## Signing

`signs[].artifacts` accepts `all`, `archive`, `binary`, `checksum`, `package`
and `sbom`. None of those names covers `release.extra_files`, and whether `all`
reaches them cannot be established locally: keyless cosign needs an OIDC token
only a GitHub Actions run has. Because a `signs:` block's coverage would be a
guess, the release workflow signs each bundle with `cosign sign-blob` and then
verifies every one of them with `cosign verify-blob` in the same job, before
anodizer is invoked at all. A signature nobody verified before upload is a
signature the first user discovers is broken.

`anodizer healthcheck` confirms cosign is a tool the pipeline can see:

No substitution applies to this block; it carries no U+2014 dash.

```
$ anodizer healthcheck
   • Anodizer Environment Health Check
   • ========================================
   • ✓ cargo                Rust package manager (cargo 1.98.0 (797e8a9bc 2026-08-05))
   • ✓ git                  Version control (git version 2.53.0)
   • ✓ docker               Container runtime (Docker version 29.4.3, build 055a478)
   • ✗ podman               Container runtime (Linux-only alt backend)
   • ✓ nfpm                 Linux package builder (deb/rpm/apk) (nfpm: a simple and 0-dependencies apk, arch linux, deb, ipk, msix, and rpm packager written in Go)
   • ✓ cargo-zigbuild       Cross-compilation via Zig (cargo-zigbuild 0.22.1)
   • ✗ zig                  Zig toolchain (linker/libc behind cargo-zigbuild)
   • ✓ cross                Cross-compilation via Docker (cross 0.2.5)
   • ✓ gpg                  GNU Privacy Guard (signing) (gpg (GnuPG) 2.4.8)
   • ✓ cosign               Sigstore container signing (GitVersion:    v2.4.3)
   • ✗ aws                  AWS CLI (S3 blob storage)
   • ✓ gsutil               Google Cloud Storage CLI (gsutil version: 5.27)
   • ✗ az                   Azure CLI (Blob storage)
   • 9 available, 4 missing
```

## The engine's own prefix, and the parsers

The pinned engine's release assets unpack to a prefix, and the packaging takes
three things out of it:

```
nvim-linux-x86_64/bin/nvim
nvim-linux-x86_64/lib/nvim/parser/{c,lua,markdown,markdown_inline,query,vim,vimdoc}.so
nvim-linux-x86_64/share/nvim/runtime/
```

nvim derives the parser directory from its own `argv[0]`, so lifting the binary
to `libexec/view/nvim` loses every bundled parser while leaving the editor
otherwise working. Measured against the pinned engine, with `$VIMRUNTIME`
exported exactly as the editor exports it:

```
$ # engine left in its own prefix
PARSE ok=true translation_unit

$ # engine at libexec/view/nvim, parsers left at libexec/view/lib/nvim/parser
PARSE ok=false .../treesitter/languagetree.lua:132: No parser for language "c"

$ # same, with lib/nvim/parser copied onto $VIMRUNTIME/parser
PARSE ok=true translation_unit
```

The packaging therefore copies `lib/nvim/parser` onto the runtime directory,
which is on `runtimepath` and searched wherever the binary sits, and then
asserts the destination exists, so an engine that moves its parsers upstream
fails the build immediately, catching the break before it ships as an editor
that highlights nothing. The Windows asset needs the same treatment for a
different reason: its `bin/` holds `lua51.dll`, `DbgHelp.dll` and
`win32yank.exe` beside `nvim.exe`, and `nvim.exe` does not start without them.
The script copies the whole of `bin/` into `libexec/view/` for that reason, on
every platform.

## Building the Windows zip

Measured on a real Windows host (PowerShell 5.1, Git for Windows), three ways
of writing the `.zip` from the Git Bash step the workflow runs:

```
$ # A: the Git Bash path handed to PowerShell as-is
Compress-Archive : The path '\c\Users\Administrator\t15probe' either does not exist or is not a
valid file system path.
A exit=1

$ # B: the same path through cygpath
B exit=0
bundle\libexec\
bundle\bin\view.exe
bundle\libexec\view\share\
bundle\libexec\view\nvim.exe

$ # C: Windows' own bsdtar, tar -a -cf
C exit=0
bundle/
bundle/bin/
bundle/libexec/
bundle/libexec/view/
bundle/libexec/view/nvim.exe
bundle/libexec/view/share/nvim/runtime/filetype.lua
```

A is the bug: Git Bash prints POSIX-style paths that Windows cannot resolve. B
works but writes backslash-separated entry names, which every non-Windows unzip
folds into one literal file name. C is what the packaging uses:
`"$SYSTEMROOT/System32/tar.exe" -a -cf`, which is present on every Windows
runner (`bsdtar 3.8.4 - libarchive 3.8.4`), needs no path translation, and
writes the forward-slash names the format specifies. The `tar` on Git Bash's
own `PATH` is GNU tar 1.35, which writes no zip container at all.
