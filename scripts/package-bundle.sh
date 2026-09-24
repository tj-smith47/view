#!/usr/bin/env bash
# The release archive's layout is spelled here and nowhere else on the
# producing side: these three paths are what BundledEngine::resolve_from
# reads beside an installed executable, so a move here that is not mirrored
# there ships an editor that silently spawns whatever nvim the machine
# happens to carry.
set -euo pipefail

cd -- "$(cd -- "$(dirname -- "$0")/.." && pwd)"

BIN_DIR="bin"
ENGINE_DIR="libexec/view"
RUNTIME_DIR="libexec/view/share/nvim/runtime"

# The engine's own tarball keeps its binary under `bin/` and its treesitter
# parsers under `lib/nvim/parser`, and nvim derives the second from its own
# argv[0]: lifted out of that prefix, it loads no parser at all and every
# highlight silently degrades. The parsers ride the runtime directory
# instead, which is on runtimepath and therefore searched wherever the
# binary sits. Its `bin/` siblings ride along too, because on Windows
# nvim.exe does not start without lua51.dll beside it and the clipboard
# provider is win32yank.exe from that same directory.
engine_asset() {
  case "$1" in
    x86_64-unknown-linux-gnu) echo "nvim-linux-x86_64.tar.gz" ;;
    aarch64-unknown-linux-gnu) echo "nvim-linux-arm64.tar.gz" ;;
    x86_64-apple-darwin) echo "nvim-macos-x86_64.tar.gz" ;;
    aarch64-apple-darwin) echo "nvim-macos-arm64.tar.gz" ;;
    x86_64-pc-windows-msvc) echo "nvim-win64.zip" ;;
    *) echo "PACKAGE FAIL: no pinned engine asset for target $1" >&2; return 1 ;;
  esac
}

exe_suffix() {
  case "$1" in
    *-windows-*) echo ".exe" ;;
    *) echo "" ;;
  esac
}

# Downloads and unpacks the pinned engine, answering with the prefix
# directory that carries `bin/`, `share/` and `lib/`. VIEW_ENGINE_PREFIX
# names one that is already on disk, which is what lets the layout test run
# the real staging code offline.
engine_prefix() {
  local target="$1" workdir="$2" asset pin url
  if [ -n "${VIEW_ENGINE_PREFIX:-}" ]; then
    echo "$VIEW_ENGINE_PREFIX"
    return
  fi
  asset="$(engine_asset "$target")"
  pin="$(tr -d '[:space:]' < .engine-pin)"
  url="https://github.com/neovim/neovim/releases/download/${pin}/${asset}"
  mkdir -p "$workdir"
  curl -fsSL -o "$workdir/$asset" "$url"
  case "$asset" in
    *.zip) unzip -q "$workdir/$asset" -d "$workdir" ;;
    *) tar xzf "$workdir/$asset" -C "$workdir" ;;
  esac
  # A glob rather than `find | head`: closing the pipe on the first match
  # leaves find killed by SIGPIPE under `set -o pipefail`, and taking the
  # first of several would pick an engine prefix at random. The engine's
  # assets unpack to exactly one directory, so anything else is a surprise
  # worth stopping on.
  local dirs=("$workdir"/*/)
  if [ "${#dirs[@]}" -ne 1 ] || [ ! -d "${dirs[0]}" ]; then
    echo "PACKAGE FAIL: expected one engine prefix under $workdir, found ${#dirs[@]}" >&2
    return 1
  fi
  echo "${dirs[0]%/}"
}

stage() {
  local target="$1" view_bin="$2" root="$3" suffix prefix
  suffix="$(exe_suffix "$target")"
  prefix="$(engine_prefix "$target" "$root.engine")"
  [ -d "$prefix" ] || { echo "PACKAGE FAIL: no engine prefix at '$prefix'" >&2; return 1; }

  mkdir -p "$root/$BIN_DIR" "$root/$ENGINE_DIR" "$root/$RUNTIME_DIR"
  cp "$view_bin" "$root/$BIN_DIR/view$suffix"
  chmod +x "$root/$BIN_DIR/view$suffix"
  cp -R "$prefix/bin/." "$root/$ENGINE_DIR/"
  cp -R "$prefix/share/nvim/runtime/." "$root/$RUNTIME_DIR/"
  cp -R "$prefix/lib/nvim/parser" "$root/$RUNTIME_DIR/parser"
  chmod +x "$root/$ENGINE_DIR/nvim$suffix"
  # Asserted at the destination, and on a parser rather than on the
  # directory holding it: an engine that moved or emptied its parsers
  # upstream would otherwise ship an editor that starts, opens files and
  # highlights nothing, with the build still green.
  if ! compgen -G "$root/$RUNTIME_DIR/parser/*" > /dev/null; then
    echo "PACKAGE FAIL: the bundle ships no treesitter parsers" >&2
    return 1
  fi
}

archive() {
  local root="$1" out="$2" parent base
  parent="$(cd -- "$(dirname -- "$root")" && pwd)"
  base="$(basename -- "$root")"
  case "$out" in
    # Windows ships its own bsdtar, which writes a zip container the GNU tar
    # on Git Bash's PATH cannot. Two alternatives were measured on a real
    # Windows host and rejected: a Git Bash path handed to PowerShell's
    # Compress-Archive is not a path Windows resolves ("either does not
    # exist or is not a valid file system path"), and cygpath'ing it works
    # but writes backslash-separated entry names that every non-Windows
    # unzip reads as part of the file name.
    *.zip) "${SYSTEMROOT:-C:/Windows}/System32/tar.exe" -C "$parent" -a -cf "$out" "$base" ;;
    *) tar -C "$parent" -czf "$out" "$base" ;;
  esac
}

case "${1:-}" in
  asset) engine_asset "${2:?target}" ;;
  stage) stage "${2:?target}" "${3:?view binary}" "${4:?root}" ;;
  archive) archive "${2:?root}" "${3:?output path}" ;;
  *)
    echo "usage: $0 asset <target>" >&2
    echo "       $0 stage <target> <view-binary> <root>" >&2
    echo "       $0 archive <root> <output-path>" >&2
    exit 2
    ;;
esac
