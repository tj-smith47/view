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
  find "$workdir" -maxdepth 1 -mindepth 1 -type d -print | head -1
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
  if [ -d "$prefix/lib/nvim/parser" ]; then
    cp -R "$prefix/lib/nvim/parser" "$root/$RUNTIME_DIR/parser"
  fi
  chmod +x "$root/$ENGINE_DIR/nvim$suffix"
}

archive() {
  local root="$1" out="$2" parent base
  parent="$(cd -- "$(dirname -- "$root")" && pwd)"
  base="$(basename -- "$root")"
  case "$out" in
    # Git Bash carries no zip(1), and its tar writes no zip container; the
    # PowerShell that every Windows runner has is the one archiver present.
    *.zip) powershell -NoProfile -Command \
      "Compress-Archive -Path '$parent/$base' -DestinationPath '$out' -Force" ;;
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
