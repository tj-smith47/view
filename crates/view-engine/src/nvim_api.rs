//! Typed convenience wrappers for the specific nvim RPC calls the terminal
//! frontend needs, so no caller outside this crate has to construct an
//! `rmpv::Value` by hand. `scripts/audit-deps.sh` forbids the bin crate
//! `view` from depending on `rmpv` directly; these methods are the sanctioned
//! way for it to reach the same calls.

use crate::handle::{saturate_u32, EngineError, EngineHandle};
use crate::process::SWAP_RECOVERY_PROBE;
use crate::rpc::RpcError;
use rmpv::Value;
use std::path::PathBuf;
use std::time::Duration;
use view_core::msg::{BufferHandle, OptionValue, TextEdit};
use view_core::native::ai_context::{
    CurrentBufferRead, CursorRead, DiagnosticEntry, DiagnosticSeverity, QuickfixEntry,
    SelectionRead,
};
use view_core::native::mappings::{default_maps, is_spellable, MappingSpec, COMMAND};

/// Upper bound on how long each of [`EngineHandle::read_current_buffer_text`],
/// [`EngineHandle::read_cursor_context`], [`EngineHandle::read_diagnostic_entries`],
/// and [`EngineHandle::read_quickfix_entries`] waits for nvim's reply. Same
/// rationale as [`GET_MODE_TIMEOUT`]: each is a synchronous nvim-side read
/// issued at prompt-submission time, so a wedged engine must not hang the
/// submission indefinitely. Reads only -- [`EngineHandle::set_buf_text`], the
/// one write in this group, bounds itself on [`BUF_SET_TEXT_TIMEOUT`]
/// instead, matching every other purpose-named timeout in this file rather
/// than folding a write into a constant documented as reads.
const CONTEXT_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on how long [`EngineHandle::set_buf_text`] waits for nvim's
/// reply. Same 5-second bound as [`CONTEXT_READ_TIMEOUT`] (a batched
/// `nvim_buf_set_text` application is not meaningfully slower than a read),
/// kept as its own constant rather than shared with the reads: this is the
/// one call in the group that mutates buffer text, and a future change to
/// the read timeout must not silently retune how long a write blocks too.
const BUF_SET_TEXT_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on how long [`EngineHandle::ui_attach`] waits for nvim's
/// reply before giving up.
///
/// The caller issues this request after the terminal has already entered
/// raw mode; an unbounded wait against a wedged engine would leave the
/// terminal in that state with no way out short of killing the process from
/// outside.
const UI_ATTACH_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on how long [`EngineHandle::register_vim_enter_autocmd`]
/// waits for nvim's reply. Same rationale as [`UI_ATTACH_TIMEOUT`]: this
/// runs during startup, before the paint loop's own unbounded-notify regime
/// begins, so it still needs a bound.
const REGISTER_VIM_ENTER_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on how long [`EngineHandle::eval_str`] waits for nvim's
/// reply. Callers of this probe are test/oracle harnesses driving their own
/// bounded polling loops (see `view-oracle`'s `EngineSession`), never the
/// paint loop itself, but an unbounded wait against a wedged engine would
/// still hang whatever harness is blocked on the answer.
const EVAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on how long [`EngineHandle::get_mode`] waits for nvim's
/// reply. `nvim_get_mode` is answered on receipt even while nvim's main
/// loop is busy or blocked (see [`EngineHandle::get_mode`]), so a healthy
/// engine replies near-instantly; this bound only covers a dead or wedged
/// connection.
const GET_MODE_TIMEOUT: Duration = Duration::from_secs(5);

/// The lua chunk [`EngineHandle::feed_keys`] runs inside nvim, taking the
/// key notation as its single vararg. Constant by construction: no caller
/// data is ever interpolated into it, so no quote, backslash, or newline in
/// a notation can change what runs.
const FEED_KEYS_CHUNK: &str =
    "vim.fn.feedkeys(vim.api.nvim_replace_termcodes(..., true, true, true), 't')";

/// The lua chunk [`EngineHandle::hold_option`] runs inside nvim, taking the
/// option name and its value as its two varargs. Constant by construction
/// for the same reason as [`FEED_KEYS_CHUNK`]: no caller data is
/// interpolated, so no option name or value can change what runs.
///
/// Two guards, because one cannot see every write. `OptionSet` catches a
/// write the moment it happens, before anything redraws -- but nvim does
/// not nest autocommands, so a write made *inside* another autocommand's
/// callback fires no `OptionSet` at all, and that is exactly how a
/// superseded plugin re-asserts its option (lualine's `ColorScheme`
/// autocmd, defined without `nested`, re-runs its `setup()`). `SafeState`
/// is the backstop for that class: it fires once nvim is back in its main
/// loop with nothing pending, which nvim reaches before it redraws, so the
/// value is restored ahead of the first frame that could have shown the
/// plugin's. Both halves were measured against the live heavy fixture --
/// an `OptionSet`-only guard left `laststatus` at lualine's `2` after
/// `:colorscheme`, and with `SafeState` added every frame a redraw witness
/// recorded was painted with the held `0`.
///
/// The re-assert is guarded by a read so the common case (nothing changed
/// the option) is one API call per idle transition and writes no option,
/// which matters because `SafeState` fires every time nvim waits for input.
/// The guard's own write cannot re-enter it for the same
/// no-nesting reason it exists.
///
/// Global options only. Both the set and the re-assert pass an empty `{}`
/// opts table, which `nvim_set_option_value` and `nvim_get_option_value`
/// read as the current window and buffer, so a window- or buffer-local
/// option would be held only wherever the callback happened to run and left
/// to the plugin everywhere else. Every caller reaches this through
/// `view_native`'s takeover table, whose `option` field states the same
/// precondition and whose rows are checked against a live nvim's
/// `nvim_get_option_info2` scope.
const HOLD_OPTION_CHUNK: &str = "\
local name, value = ...
vim.api.nvim_set_option_value(name, value, {})
local group = vim.api.nvim_create_augroup('view-hold-' .. name, { clear = true })
local function hold()
  if vim.api.nvim_get_option_value(name, {}) ~= value then
    vim.api.nvim_set_option_value(name, value, {})
  end
end
vim.api.nvim_create_autocmd('OptionSet', {
  group = group,
  pattern = name,
  callback = hold,
})
vim.api.nvim_create_autocmd('SafeState', {
  group = group,
  callback = hold,
})";

/// The lua chunk [`EngineHandle::register_mappings`] runs inside nvim,
/// taking view's channel id, the specs to register, every feature/verb pair
/// the command can complete, and the command's own name as its four
/// varargs. Constant by construction for the same reason as
/// [`FEED_KEYS_CHUNK`]: no caller data is interpolated into the Lua source.
///
/// One chunk rather than a call per key, and it answers with the whole claim
/// list: what view claimed is one fact a user is told once, so it is
/// established in one atomic pass over the specs rather than reassembled
/// from replies that interleave with startup traffic.
///
/// What the user's config already mapped is snapshotted BEFORE the first key
/// is set, since setting it is what destroys the answer. The snapshot spans
/// the global table and every loaded buffer's own, because
/// `vim.fn.maparg(lhs, 'n')` answers only for the current buffer: a
/// buffer-local mapping elsewhere -- an ftplugin's, most commonly -- beats
/// view's global one wherever it applies, so a claim report built from
/// `maparg` alone would say a key was free while the user watches it keep
/// doing what it always did. Keys are compared after
/// `nvim_replace_termcodes`, which is what turns `<leader>ff` into the bytes
/// a registered mapping is actually stored under.
///
/// The right-hand side is a plain `<Cmd>rpcnotify(...)<CR>` string rather
/// than a Lua callback so that `:map`, `maparg()`, and every plugin that
/// introspects mappings show exactly what view did, in a form a user can
/// read and copy. It is built with `string.format` from the spec's own
/// fields inside the chunk, never interpolated into the chunk source, and
/// every token reaching it is `[a-z0-9_-]` by
/// [`is_spellable`](view_core::native::mappings::is_spellable), applied in
/// [`register_mappings`](EngineHandle::register_mappings).
///
/// Normal mode is the whole scope, matching
/// [`MappingSpec`](view_core::native::mappings::MappingSpec)'s own
/// normal-mode-only contract: the snapshot reads `'n'` maps, globally and
/// per loaded buffer, and every key is set with `vim.keymap.set('n', ...)`.
/// A spec carries no mode to vary that by, so all three `'n'` literals in
/// the chunk below are the scope, not a default some caller may override.
///
/// The command registers unconditionally, outside the spec loop: a user who
/// turned every default key off, or every feature, still has a way in.
const REGISTER_MAPPINGS_CHUNK: &str = "\
local channel, specs, entries, command = ...
local taken = {}
local function note(maps)
  for _, m in ipairs(maps) do
    if m.lhsraw then taken[m.lhsraw] = true end
    taken[m.lhs] = true
  end
end
note(vim.api.nvim_get_keymap('n'))
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(buf) then
    note(vim.api.nvim_buf_get_keymap(buf, 'n'))
  end
end
local claimed = {}
for _, spec in ipairs(specs) do
  local resolved = vim.api.nvim_replace_termcodes(spec.lhs, true, true, true)
  local rhs = string.format(
    \"<Cmd>call rpcnotify(%d, 'view_invoke', '%s', '%s')<CR>\",
    channel, spec.feature, spec.verb)
  vim.keymap.set('n', spec.lhs, rhs, {
    desc = string.format('view: %s %s', spec.feature, spec.verb),
    silent = true,
  })
  claimed[#claimed + 1] = {
    feature = spec.feature,
    lhs = spec.lhs,
    had_user_mapping = (taken[resolved] or taken[spec.lhs]) == true,
  }
end
vim.api.nvim_create_user_command(command, function(opts)
  vim.rpcnotify(channel, 'view_invoke', opts.fargs[1] or '', opts.fargs[2] or '')
end, {
  nargs = '*',
  desc = 'invoke a view native feature',
  complete = function(lead, line)
    local words = vim.split(vim.trim(line), '%s+')
    local at = #words - 1 + ((line:sub(-1) == ' ') and 1 or 0)
    local seen, out = {}, {}
    for _, entry in ipairs(entries) do
      local word = nil
      if at <= 1 then
        word = entry.feature
      elseif entry.feature == words[2] then
        word = entry.verb
      end
      if word and not seen[word] and vim.startswith(word, lead) then
        seen[word] = true
        out[#out + 1] = word
      end
    end
    table.sort(out)
    return out
  end,
})
return claimed";

/// The lua chunk [`EngineHandle::register_bridge`] runs inside nvim, taking
/// view's channel id as its single vararg. Constant by construction for the
/// same reason as [`FEED_KEYS_CHUNK`]: no caller data is interpolated into
/// the Lua source.
///
/// One augroup carrying every editor-state trigger view listens to, not one
/// registration per consumer. The registration is precisely what a restarted
/// engine loses, and three separate registrations are three chances for one
/// to be missed -- leaving its consumer quietly stale while the other two
/// keep working, a failure with no symptom at the point it happens. Created
/// with `clear = true` so re-issuing it replaces the group rather than
/// stacking a second copy of every autocommand.
///
/// `ColorScheme` alone forwards `args.match` (the scheme's name) through the
/// shared `relay` closure, reading no state of its own. The statusline's
/// three segment triggers each compute their own richer payload instead of
/// a bare match, because "the buffer's name" is not what any of their
/// consumers need:
///
/// - `DiagnosticChanged`'s callback calls `vim.diagnostic.count(0)` --
///   synchronous and already how nvim itself would answer `:call
///   diagnostic#count()`, so it costs the autocommand nothing a plain match
///   forward would not have.
/// - The git trigger group's callback calls `vim.system(...)`
///   asynchronously, off nvim's main loop, and only sends its `rpcnotify`
///   once that resolves -- never blocking the autocommand itself despite
///   shelling out.
/// - The new buffer trigger group's callback reads `vim.fn.expand('%:t')`
///   and `vim.bo.modified`, both plain synchronous option/API reads.
///
/// None of the three blocks nvim's main loop for longer than an ordinary
/// autocommand already would; only the git lookup does real I/O, and it is
/// the one callback that hands that off asynchronously rather than doing it
/// inline.
///
/// `BufEnter`, `DirChanged`, and `FocusGained` are the git triggers: the
/// repository a branch is read from changes when the active buffer changes
/// or the working directory moves, and the branch itself can change under a
/// backgrounded editor, which is what returning focus is the signal for.
/// `BufEnter`, `BufFilePost`, `BufWritePost`, and `BufModifiedSet` are the
/// buffer triggers: the first three cover a new or renamed file landing in
/// the current window, and `BufModifiedSet` alone covers every actual
/// modified-flag transition without the per-keystroke flood
/// `TextChanged`/`TextChangedI` would add.
const REGISTER_BRIDGE_CHUNK: &str = "\
local channel = ...
local group = vim.api.nvim_create_augroup('view_bridge', { clear = true })
local function relay(event)
  return function(args)
    vim.rpcnotify(channel, 'view_bridge', event, args.match or '')
  end
end
vim.api.nvim_create_autocmd('ColorScheme', {
  group = group,
  callback = relay('colorscheme'),
})
vim.api.nvim_create_autocmd('DiagnosticChanged', {
  group = group,
  callback = function()
    local counts = vim.diagnostic.count(0)
    local errors = counts[vim.diagnostic.severity.ERROR] or 0
    local warnings = counts[vim.diagnostic.severity.WARN] or 0
    vim.rpcnotify(channel, 'view_bridge', 'diagnostics', errors, warnings)
  end,
})
vim.api.nvim_create_autocmd({ 'BufEnter', 'DirChanged', 'FocusGained' }, {
  group = group,
  callback = function()
    vim.system({ 'git', 'rev-parse', '--abbrev-ref', 'HEAD' }, { text = true }, function(res)
      local branch = ''
      if res.code == 0 and res.stdout then
        branch = res.stdout:gsub('%s+$', '')
      end
      vim.rpcnotify(channel, 'view_bridge', 'git', branch)
    end)
  end,
})
vim.api.nvim_create_autocmd({ 'BufEnter', 'BufFilePost', 'BufWritePost', 'BufModifiedSet' }, {
  group = group,
  callback = function()
    vim.rpcnotify(channel, 'view_bridge', 'buffer', vim.fn.expand('%:t'), vim.bo.modified)
  end,
})
vim.api.nvim_create_autocmd('VimLeavePre', {
  group = group,
  callback = function()
    pcall(vim.rpcrequest, channel, 'view_leaving')
  end,
})";

/// `VimLeavePre` gets its own method rather than another `view_bridge`
/// event: every other hook in the group is editor state a later frame
/// recomputes, delivered best-effort into the runtime channel, while this
/// one is evidence the reader itself must hold at the moment the stream
/// ends -- after which there is no frame, no channel and no engine left to
/// ask (see [`EngineHandle::announced_exit`]).
///
/// It is the group's only `rpcrequest`, and it is one because a
/// notification is not delivery: `rpcnotify` hands the bytes to nvim's
/// event loop and returns, and a process on its way out can exit before
/// that loop ever writes them. On Windows it does exactly that: a wire
/// probe over a failing suite saw the announcement arrive zero times in
/// ten exits, view reading each deliberate `:qa!` as a crash and
/// respawning the editor its user had just closed. A
/// request cannot be lost that way: nvim stays inside `VimLeavePre` until
/// the reply arrives, so an exit that reached this line is an exit view
/// saw. `pcall` because a channel already gone (view died first) must not
/// turn nvim's exit into an error message.
///
/// The lua chunk [`EngineHandle::register_clipboard`] runs inside nvim,
/// taking view's channel id as its single vararg. Installs `g:clipboard`
/// (`:help g:clipboard`) so `"+y`/`"+p` route through view's own clipboard
/// worker instead of nvim's auto-detected shell tool.
///
/// `paste` issues a blocking `rpcrequest`: `g:clipboard.paste` must return
/// the lines synchronously (nvim has no async paste-provider hook), so the
/// closure blocks on the same `EngineRequest`/`Effect::Reply` contract
/// `VimEnter` uses (see [`view_core::msg::EngineRequest::ClipboardGet`]).
/// It returns the `[lines, regtype]` pair `view_clipboard_get` answers
/// with -- the pair form, not the bare-list form, is what lets a paste
/// restore a linewise copy's register type instead of nvim defaulting
/// every paste to charwise (`v`); verified against the pinned engine (see
/// `docs/clipboard-provider-wire-capture.md`).
///
/// `copy` also issues a blocking `rpcrequest`, not `rpcnotify`: nvim
/// ignores its return value, but routing it through the same
/// `EngineRequest`/reply contract as `paste` means a copy and a paste that
/// race each other serialize through one channel instead of a notify
/// silently overtaking a request already in flight. `copy`'s second
/// argument is nvim's own `regtype` for the yank (`v`/`V`/blockwise
/// `<C-v>` with an optional trailing width), forwarded verbatim as a
/// fourth positional `rpcrequest` argument so `view_clipboard_set`'s
/// decoder (see `decode_clipboard_set`) can round-trip it back through
/// the system clipboard's trailing-newline convention instead of losing
/// it -- the bug the wire capture doc's `[lines, regtype]` conclusion
/// exists to prevent.
///
/// Both `'+'` and `'*'` are wired, and to the same backend: `copy`/`paste`
/// dicts missing either key error on that register when accessed (verified
/// empirically, see the capture doc above), and arboard exposes one system
/// clipboard with no cross-platform primary-selection equivalent to give
/// `'*'` a distinct backend.
///
/// `cache_enabled = 0`: nvim reads the clipboard freshly at every paste
/// rather than caching between the copy and paste calls, so `"+p` never
/// returns text that was current only when some earlier copy ran.
const REGISTER_CLIPBOARD_CHUNK: &str = "local channel = ...
if vim.g.clipboard == nil then
  vim.g.clipboard = {
    name = 'view',
    copy = {
      ['+'] = function(lines, regtype) vim.rpcrequest(channel, 'view_clipboard_set', '+', lines, regtype) end,
      ['*'] = function(lines, regtype) vim.rpcrequest(channel, 'view_clipboard_set', '*', lines, regtype) end,
    },
    paste = {
      ['+'] = function() return vim.rpcrequest(channel, 'view_clipboard_get', '+') end,
      ['*'] = function() return vim.rpcrequest(channel, 'view_clipboard_get', '*') end,
    },
    cache_enabled = 0,
  }
end";

/// Lists every listed, loaded buffer for the picker's `Source::Buffers`
/// corpus, verified live against the pinned engine -- see
/// `docs/picker-buffer-list-wire-capture.md` for the captured reply shapes
/// this chunk's `buflisted` filter and `[No Name]`-eligible empty `name`
/// both depend on. Constant, like every other chunk here: no caller data is
/// interpolated into it.
const BUFFER_LIST_CHUNK: &str = "\
local out = {}
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(buf) and vim.bo[buf].buflisted then
    out[#out + 1] = {
      bufnr = buf,
      name = vim.api.nvim_buf_get_name(buf),
      modified = vim.bo[buf].modified,
    }
  end
end
return out";

/// Resolves the picker preview pane's text for a candidate path, verified
/// live against the pinned engine -- see
/// `docs/picker-preview-wire-capture.md` for the captured reply shapes
/// (`loaded`/`lines`) this chunk's `nvim_buf_is_loaded`/name-match lookup
/// produces, and the load-bearing case (a modified-but-unsaved buffer
/// answers with its modified content, never the file on disk). Constant,
/// like every other chunk here: no caller data is interpolated into it --
/// the candidate path travels as `nvim_exec_lua`'s positional vararg
/// instead, the same convention `REGISTER_MAPPINGS_CHUNK` uses.
///
/// Both sides of the name comparison are canonicalized before comparing: a
/// candidate path reached through a symlink (the picker's own root, or an
/// ancestor directory, symlinked) would otherwise never byte-equal
/// `nvim_buf_get_name`'s resolved name, silently answering `loaded = false`
/// and falling back to a stale on-disk read for a buffer that is, in fact,
/// open and modified. `vim.uv.fs_realpath` resolves symlinks for a path
/// that exists on disk (the common case for a real buffer); a brand-new
/// unsaved buffer's name may not exist on disk yet, so a failed realpath
/// falls back to `fnamemodify(..., ':p')`'s plain absolute-path
/// normalization. The empty-string guard matters specifically because
/// `fnamemodify('', ':p')` resolves to nvim's own cwd rather than staying
/// empty, which would otherwise turn nvim's `[No Name]` scratch buffers
/// into false-positive matches against any candidate path equal to nvim's
/// cwd.
const PREVIEW_CHUNK: &str = "\
local path = ...
local function canon(p)
  if p == '' then
    return p
  end
  return vim.uv.fs_realpath(p) or vim.fn.fnamemodify(p, ':p')
end
local wanted = canon(path)
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(buf) and canon(vim.api.nvim_buf_get_name(buf)) == wanted then
    return { loaded = true, lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false) }
  end
end
return { loaded = false }";

/// The `canon()` both [`LOAD_HIDDEN_CHUNK`] and
/// [`HIDDEN_CANON_PROBE_CHUNK`] embed, as one literal rather than two
/// copies: the probe exists to pin [`canonical_hidden_key`] against the
/// resolution the shipped chunk actually uses, and a second copy of the
/// algorithm would let the two drift apart in exactly the way the pin is
/// there to catch. A macro rather than a `const` because `concat!` composes
/// literals, not constants.
macro_rules! hidden_canon_lua {
    () => {
        "\
local function canon(p)
  if p == '' then
    return p
  end
  local real = vim.uv.fs_realpath(p)
  if real then
    return real
  end
  local head = vim.fn.fnamemodify(p, ':h')
  local real_head = vim.uv.fs_realpath(head)
  if real_head then
    local sep = real_head:sub(-1) == '/' and '' or '/'
    return real_head .. sep .. vim.fn.fnamemodify(p, ':t')
  end
  return p
end
"
    };
}

/// Keys [`EngineHandle::hidden_bufs`](crate::handle::EngineHandle) in
/// agreement with whatever buffer `vim.fn.bufadd` actually resolves a
/// `load_hidden` call onto -- symlinks resolved where the target exists,
/// [`nvim_style_absolute`]'s own parent-resolution as the fallback for a
/// path that does not exist yet -- so two different spellings of the same
/// file, existing or not, share one
/// [`HiddenHold`](crate::hidden_buffers::HiddenHold) entry instead of racing
/// each other's cleanup. Computed independently of nvim's own resolution, on
/// this side of the wire, rather than waiting for a reply to learn it: the
/// hold must exist the instant [`EngineHandle::load_hidden`] is called,
/// before any reply carrying nvim's own answer could possibly have arrived
/// (see that method's own doc).
///
/// `std::fs::canonicalize` is a blocking `stat` walk run inline on whichever
/// thread calls `load_hidden`/`release_hidden` -- the single-threaded
/// executor in `view::runtime`, off the paint loop and off the per-keystroke
/// key-dispatch path. It fires once per diff review opened or closed, not
/// once per frame or per keystroke, so a slow filesystem stalls only that
/// one open/close action rather than every keystroke or repaint -- an
/// acceptable, bounded latency cost for what stays a synchronous call.
pub(crate) fn canonical_hidden_key(path: &str) -> String {
    // A relative spelling has no answer on this side of the wire: nvim
    // resolves one against its own cwd, which `:cd` moves and this process
    // never observes (`docs/hidden-buffer-wire-capture.md` case 19). Rather
    // than inventing a second authority from this process's cwd, the key is
    // the spelling itself -- and `hidden_path_refusal` keeps such a
    // spelling from ever reaching a hold in the first place.
    if !std::path::Path::new(path).is_absolute() {
        return path.to_owned();
    }
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved.to_string_lossy().into_owned();
    }
    nvim_style_absolute(std::path::Path::new(path))
        .to_string_lossy()
        .into_owned()
}

/// Why a path can never name a hidden buffer. Each member is a spelling
/// whose identity `vim.fn.bufadd` and [`canonical_hidden_key`] would answer
/// differently, so the pair is refused outright instead of normalized: one
/// authority decides hidden-buffer identity, and every spelling it cannot
/// decide unambiguously is off-contract.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiddenPathRefusal {
    /// Empty, or nothing but whitespace. `bufadd` resolves it onto nvim's
    /// own `[No Name]` buffer -- the user's scratch buffer, which a review
    /// would attach to and write its hunks into
    /// (`docs/hidden-buffer-wire-capture.md` case 17).
    Blank,
    /// Not absolute. nvim resolves it against its own cwd and this process
    /// would resolve the key against its own; the two diverge the moment
    /// the user runs `:cd` (case 19). `docs/acp-v1-wire-capture.md`'s
    /// `Diff` schema documents `path` as "The absolute file path being
    /// modified," so an absolute spelling is what the wire promised anyway.
    Relative,
    /// Ends in a path separator. `bufadd` keeps the separator and resolves
    /// the spelling onto a *second* buffer over the same file, while the
    /// key drops it -- one hold over two buffers (case 18). A trailing
    /// separator names a directory, and a directory is already refused.
    TrailingSeparator,
}

impl std::fmt::Display for HiddenPathRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Blank => "a blank path",
            Self::Relative => "a relative path",
            Self::TrailingSeparator => "a path ending in a separator",
        })
    }
}

/// Which rule refuses `path` as a hidden buffer's identity, or `None` when
/// none does. The one predicate
/// [`EngineHandle::load_hidden`](EngineHandle::load_hidden) and
/// [`EngineHandle::release_hidden`](EngineHandle::release_hidden) both
/// consult, so a spelling that cannot take a hold can never issue a release
/// that looks for one either.
///
/// Mirrored on the nvim side by [`LOAD_HIDDEN_CHUNK`]'s own blank and
/// trailing-separator refusals (the relative case has no mirror by design:
/// nvim's cwd is the *correct* authority for a relative spelling, and the
/// divergence is entirely on this side), and upstream of both by
/// `view_ai`'s ACP boundary, which drops a proposal whose path is off the
/// wire's own absolute-path contract before either is reached.
#[must_use]
pub fn hidden_path_refusal(path: &str) -> Option<HiddenPathRefusal> {
    if path.trim().is_empty() {
        return Some(HiddenPathRefusal::Blank);
    }
    if path.ends_with(std::path::is_separator) {
        return Some(HiddenPathRefusal::TrailingSeparator);
    }
    if !std::path::Path::new(path).is_absolute() {
        return Some(HiddenPathRefusal::Relative);
    }
    None
}

/// Resolves an absolute path the way `vim.fn.bufadd` resolves one that does
/// not fully exist -- see `docs/hidden-buffer-wire-capture.md` case 15,
/// which measured this directly against `bufadd` itself rather than against
/// `vim.fn.fnamemodify(p, ':p')` (a different, weaker function that does
/// *not* resolve a symlinked directory unless a `.`/`..` component happens
/// to force it to actually walk the directory chain -- `bufadd`'s own
/// identity check carries no such gate, and neither does
/// `LOAD_HIDDEN_CHUNK`'s own `canon()`, which mirrors this same
/// parent-realpath resolution for exactly this reason -- see that chunk's
/// own doc).
///
/// `bufadd` resolves the entire immediate parent directory as one unit --
/// equivalent to a `chdir` into it succeeding, which resolves any symlinks
/// in it as a side effect -- and joins the resolved parent with the file
/// name. If that whole-parent resolution fails (any single component of the
/// parent does not exist), the path is left completely unresolved: there is
/// no fallback to a shallower existing ancestor. `std::fs::canonicalize` on
/// the parent alone reproduces exactly this -- success or failure, in one
/// step, with no separate lexical collapsing of `.`/`..` needed (resolving
/// the parent removes them as an intrinsic part of resolving it) and no
/// multi-level ancestor walk to write.
fn nvim_style_absolute(path: &std::path::Path) -> std::path::PathBuf {
    let (Some(parent), Some(file_name)) = (path.parent(), path.file_name()) else {
        return path.to_path_buf();
    };
    std::fs::canonicalize(parent)
        .map(|resolved_parent| resolved_parent.join(file_name))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Resolves a path to the real buffer handle nvim holds it under, for
/// `RpcCall::LoadHidden`. Constant, like every other chunk here: the path
/// travels as `nvim_exec_lua`'s positional varargs, never interpolated into
/// the source. See `docs/hidden-buffer-wire-capture.md` for every reply
/// shape this chunk's own logic depends on.
///
/// The existing-buffer lookup runs before any creation is considered,
/// reusing `PREVIEW_CHUNK`'s own canonicalized name-match scan (symlink-safe,
/// "loaded buffer wins over disk") rather than `bufnr`/`bufadd`'s exact-string
/// name matching: a diff review opens against a file the agent proposed
/// changes to, which may be one no window has ever shown, one a previous
/// `load_hidden` call already created hidden for this same path, or one a
/// real window already has open -- all three are the same buffer identity by
/// path, and the scan finds whichever of them already exists, unmodified or
/// not (capture #5), before ever creating a second one over the same file.
/// Unlike `PREVIEW_CHUNK`, the scan matches on name alone rather than
/// requiring `nvim_buf_is_loaded`: a match that is not yet loaded (capture
/// #13 -- a buffer an earlier session left behind, or one `:bwipeout`-ed but
/// never deleted) is `bufload`-ed in place and still reported
/// `created = false`, since this call did not make it exist. Skipping that
/// case would let an unloaded match fall through to the create branch below
/// and misreport `created = true` for a buffer this connection did not
/// create -- exactly the fact `RpcCall::ReleaseHidden`'s ownership-gated
/// delete depends on `created` never getting wrong.
///
/// Only when nothing is found does this create, and it creates through
/// nvim's own `:edit`-equivalent file-open pipeline -- `vim.fn.bufadd`
/// followed by `vim.fn.bufload` -- rather than `nvim_create_buf` plus a
/// manual `readfile`/`nvim_buf_set_lines` population. The two are not
/// equivalent (capture #10-#12): `bufload`'s own read is what nvim treats as
/// the buffer's undo baseline (`undotree().seq_cur == 0`, matching a real
/// `:edit`), where populating via `nvim_buf_set_lines` recorded that
/// population itself as an undoable edit -- a single `u` right after loading
/// emptied the buffer back to nothing. `bufload` also detects `fileformat`
/// and `endofline` from the source file, where the old chunk's buffer
/// defaulted to Unix line endings regardless of what was on disk, silently
/// corrupting a CRLF file's line endings on the next `:write`. Both also run
/// nvim's ordinary file-open autocommands (filetype detection among them),
/// which `nvim_create_buf` never triggers.
///
/// A blank path and a path ending in a separator are refused before
/// anything else runs, the same `buf = 0` answer the directory refusal
/// gives. Both name something no review can be bound to and both resolve
/// to a buffer the Rust-side hold key cannot agree with: `bufadd('')` is
/// nvim's own `[No Name]` scratch buffer, whose empty name the scan below
/// would otherwise match (`docs/hidden-buffer-wire-capture.md` case 17),
/// and a trailing separator is a *second* buffer over the same file that
/// the key drops the separator from (case 18) -- one hold, two buffers.
/// The scan skips every name-less buffer for the same reason, so no
/// resolution without a name can ever be returned as a hit regardless of
/// what `wanted` holds. See
/// [`hidden_path_refusal`], which refuses the identical two spellings (and
/// relative ones, which nvim's own cwd resolves correctly and only this
/// side gets wrong) before either reaches the wire.
///
/// A path that exists but is not a regular file is refused outright, and
/// that refusal runs first -- before the existing-buffer scan below, not
/// after. A directory that already has a buffer (any window that ever ran
/// `:edit <dir>` leaves one) would otherwise pass straight through the scan
/// and resolve onto that buffer instead of hitting the refusal at all:
/// `bufload` on a directory succeeds and yields a browsable listing, whose
/// rows a review would then write its hunks over. A path that does not
/// exist yet is not refused -- the new-file proposal's own case -- and
/// `bufadd` resolves it to the empty buffer the file will be created as.
/// `bufadd` alone is what keeps the buffer unlisted (`buflisted = 0`, never
/// in `:ls`, never in the picker's `Source::Buffers`, capture #10) until
/// something else -- a real `:edit` on the same path, capture #14 -- chooses
/// to list it.
///
/// `canon()`'s own fallback (for a path `fs_realpath` cannot resolve
/// outright, because the leaf does not exist yet) resolves only the
/// immediate parent directory and joins the file name back on, rather than
/// `fnamemodify(p, ':p')` -- which does not resolve a symlinked directory at
/// all unless a `.`/`..` component happens to force it to actually walk the
/// chain (`docs/hidden-buffer-wire-capture.md` case 15). `bufadd` carries no
/// such gate: it resolves the whole parent as a unit regardless of what the
/// spelling looks like. A `canon()` that disagreed with `bufadd` here would
/// still let two spellings resolve onto the identical buffer (`bufadd`
/// itself decides that, not this scan), but the scan's own match would miss
/// it, fall through to the `bufadd` branch below, and misreport
/// `created = true` for a reuse the scan simply failed to see -- corrupting
/// `RpcCall::ReleaseHidden`'s ownership gate exactly as a genuinely wrong
/// `created` would (see the paragraph above): a connection would believe it
/// owns, and may delete, a buffer a real window or another connection
/// created. Matching `bufadd`'s own resolution here is what lets the scan
/// catch the reuse directly, so `created` is never reported by the
/// fallthrough branch for a buffer that already existed under a different
/// spelling of the same path.
///
/// The buffer's `b:changedtick` is read in the same chunk that resolves it,
/// so the review's first write can name a buffer version without waiting
/// for an edit event to learn one -- and an edit landing between this
/// resolve and that write moves the tick, which is exactly the case that
/// must refuse the write.
const LOAD_HIDDEN_CHUNK: &str = concat!(
    "local path = ...\n",
    hidden_canon_lua!(),
    "\
local tail = path:sub(-1)
if path:match('^%s*$') ~= nil or tail == '/' or tail == '\\\\' then
  return { buf = 0, created = false, changedtick = 0 }
end
local stat = (vim.uv or vim.loop).fs_stat(path)
if stat ~= nil and stat.type ~= 'file' then
  return { buf = 0, created = false, changedtick = 0 }
end
local wanted = canon(path)
for _, b in ipairs(vim.api.nvim_list_bufs()) do
  local name = vim.api.nvim_buf_get_name(b)
  if name ~= '' and canon(name) == wanted then
    if not vim.api.nvim_buf_is_loaded(b) then
      vim.fn.bufload(b)
    end
    return { buf = b, created = false, changedtick = vim.api.nvim_buf_get_changedtick(b) }
  end
end
local buf = vim.fn.bufadd(path)
if buf == 0 then
  return { buf = 0, created = false, changedtick = 0 }
end
vim.fn.bufload(buf)
return { buf = buf, created = true, changedtick = vim.api.nvim_buf_get_changedtick(buf) }"
);

/// [`LOAD_HIDDEN_CHUNK`]'s `canon()` alone, returning its answer for the
/// path it is given instead of a buffer. Composed from the identical
/// literal the chunk itself embeds, so the live test that pins
/// [`canonical_hidden_key`] against nvim's own resolution cannot drift from
/// the chunk it exists to pin -- a probe carrying its own copy of `canon()`
/// would agree with the Rust key while the shipped chunk quietly disagreed.
///
/// Gated behind the `test-support` feature (which this crate's own
/// `Cargo.toml` enables for itself during `cargo test` via a self
/// dev-dependency), like [`EngineHandle::start`](crate::handle::EngineHandle::start):
/// nothing in a shipping build resolves a path without also resolving a
/// buffer for it.
#[cfg(any(test, feature = "test-support"))]
pub const HIDDEN_CANON_PROBE_CHUNK: &str = concat!(
    "local path = ...\n",
    hidden_canon_lua!(),
    "return canon(path)"
);

/// [`LOAD_HIDDEN_CHUNK`] itself, for the live tests that drive its nvim-side
/// refusals directly rather than through
/// [`EngineHandle::load_hidden`](EngineHandle::load_hidden) -- which refuses
/// the same spellings first, and would otherwise leave the chunk's own half
/// of the belt-and-braces pair unexercised against real nvim. Gated behind
/// `test-support` for the reason [`HIDDEN_CANON_PROBE_CHUNK`] is.
#[cfg(any(test, feature = "test-support"))]
pub const HIDDEN_LOAD_CHUNK: &str = LOAD_HIDDEN_CHUNK;

/// [`canonical_hidden_key`]'s answer, for the live test that pins it
/// against [`HIDDEN_CANON_PROBE_CHUNK`]'s -- two implementations of one
/// algorithm, which nothing but a test comparing them keeps in agreement.
/// Gated behind `test-support` for the reason that chunk is.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn hidden_buffer_key(path: &str) -> String {
    canonical_hidden_key(path)
}

/// Deletes `buf` for `RpcCall::ReleaseHidden`, once its hold's refcount has
/// reached zero -- but only when nothing would be disrupted by doing so.
/// See `docs/hidden-buffer-wire-capture.md` capture #8: nvim's own
/// `nvim_buf_delete(buf, {})` does NOT refuse a buffer a window is
/// currently showing the way it refuses a modified one (case 7) -- it
/// substitutes a fresh empty buffer into every window that had it and
/// proceeds, which for a buffer `load_hidden`'s existing-buffer lookup
/// resolved onto a real, normally-opened window would silently swap the
/// user's own file out from under them. `vim.fn.win_findbuf` is checked
/// here, in Lua, before ever calling `nvim_buf_delete`, rather than trusting
/// nvim to refuse on its own.
///
/// `buflisted` is checked alongside it (capture #14): a window can adopt a
/// hidden buffer by `:edit`-ing its own path, flipping `buflisted` 0 -> 1,
/// and that stays true even after every window showing it closes again --
/// `win_findbuf` alone goes back to seeing nothing and would let this delete
/// through for a buffer that is, by then, the user's own. This is
/// belt-and-braces alongside `EngineHandle`'s `owned` gate (see
/// `HiddenHold`), which already refuses to attempt this notify at all for a
/// hold this connection never created; this second check covers the hold it
/// did create but the user has since adopted.
///
/// The modified-buffer case still relies on nvim's own refusal (case 7,
/// re-confirmed under capture #9) -- `pcall` only silences that refusal's
/// error, which this fire-and-forget call has nowhere to report to
/// regardless.
pub(crate) const RELEASE_HIDDEN_CHUNK: &str = "\
local buf = ...
if next(vim.fn.win_findbuf(buf)) == nil and vim.fn.buflisted(buf) == 0 then
  pcall(vim.api.nvim_buf_delete, buf, {})
end";

/// Opens `path` as `:edit` would, taking it as its single positional
/// vararg. Constant, like every other chunk here: no caller data is
/// interpolated into the source itself.
///
/// `path` reaches nvim as a parsed argument with filename magic switched
/// off, rather than as text an ex command re-parses: a space, `%`, `#` or a
/// leading `+` in a filename is command syntax to `:edit`. The two halves
/// carry different characters -- the argument list is what keeps the space
/// and the leading `+` out of command parsing, and `magic.file = false` is
/// what stops `%`, `#` and `\` from expanding as filename magic on top of
/// it -- each half measured separately in
/// `docs/tree-open-file-wire-capture.md`, including the magic-left-on
/// negative control.
///
/// Escaping the text was the other way to get there and it does not
/// survive the platform: `fnameescape` escapes `\` because `\` is a
/// metacharacter on Unix, and on Windows `\` is the path separator, so
/// every Windows path arrived doubled and opened nothing at all.
const OPEN_FILE_CHUNK: &str = "\
local path = ...
vim.api.nvim_cmd({ cmd = 'edit', args = { path }, magic = { file = false, bar = false } }, {})";

/// Renames a file on disk and, when a buffer is open for the old path,
/// retargets that buffer onto the new one in the same call -- verified live
/// against the pinned engine, see `docs/tree-rename-wire-capture.md` for the
/// captured cases this chunk's collision guard and buffer-retarget logic
/// both depend on. Constant, like every other chunk here: both paths travel
/// as `nvim_exec_lua`'s positional varargs, never interpolated into the
/// source.
///
/// Refuses to overwrite an existing destination (`ok = false`, both files
/// untouched) rather than reproducing `vim.fn.rename`'s own silent-overwrite
/// behavior, confirmed live and documented in the capture above. `wanted` is
/// resolved from `old_path` before the rename runs, while the file still
/// exists at that location to resolve a real path for.
///
/// Every loaded buffer's own name is canonicalized into `snapshot` before
/// `vim.fn.rename` runs, for the same reason `wanted` is: `vim.uv.fs_realpath`
/// only resolves a symlink component while the target still exists on disk
/// at that path, so a buffer opened through a symlinked ancestor directory
/// would canonicalize correctly here but fail silently (falling back to the
/// unresolved `fnamemodify` path, which never matches `wanted`) if computed
/// after the rename has already moved the file out from under the old
/// location -- see `docs/tree-rename-wire-capture.md`'s symlink case.
const RENAME_CHUNK: &str = "\
local old_path, new_path = ...
if vim.uv.fs_stat(new_path) then
  return { ok = false }
end
local function canon(p)
  return vim.uv.fs_realpath(p) or vim.fn.fnamemodify(p, ':p')
end
local wanted = canon(old_path)
local snapshot = {}
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(buf) then
    snapshot[#snapshot + 1] = { buf = buf, canon = canon(vim.api.nvim_buf_get_name(buf)) }
  end
end
local rc = vim.fn.rename(old_path, new_path)
if rc ~= 0 then
  return { ok = false }
end
for _, entry in ipairs(snapshot) do
  if entry.canon == wanted then
    vim.api.nvim_buf_set_name(entry.buf, new_path)
    break
  end
end
return { ok = true }";

/// Asks nvim for typed text via a blocked `vim.fn.input()`, primed with a
/// `kind = "confirm"` `nvim_echo` so the answer arrives on the wire as the
/// same `msg_show`/`cmdline_show` pair every other confirm-class prompt
/// does -- live-verified against the pinned engine, see
/// `docs/tree-input-prompt-wire-capture.md`. Shared by
/// [`EngineHandle::tree_create_prompt`] (`default` empty) and
/// [`EngineHandle::tree_rename_prompt`] (`default` the entry's current
/// name); the chunk itself does not distinguish the two, only the caller's
/// arguments do. Returns the typed string bare (an empty string for both an
/// unanswered `<CR>` and an `<Esc>`, indistinguishable on the wire and
/// identical in meaning here: nothing to act on).
const TREE_INPUT_PROMPT_CHUNK: &str = "\
local prompt, default = ...
vim.api.nvim_echo({{prompt, 'Question'}}, false, {kind = 'confirm'})
return vim.fn.input({prompt = prompt, default = default})";

/// Asks nvim to confirm an action via a blocked `vim.fn.confirm(prompt,
/// \"&Yes\\n&No\")`, reusing nvim's own `[Y]es, (N)o: ` accelerator prompt
/// -- live-verified to arrive on the wire as the exact same `msg_show`/
/// `cmdline_show` pair `PromptState`'s existing `Answer::Choices` parsing
/// already handles, see `docs/tree-input-prompt-wire-capture.md`.
///
/// Before it ever asks that question, the chunk checks `path` (canonicalized
/// with the same `fs_realpath`-or-`fnamemodify` fallback `RENAME_CHUNK` uses,
/// so a buffer reached through a symlinked ancestor directory is still
/// found) against every loaded buffer's own canon name. A match returns
/// `{ buffer_open = true }` without ever calling `vim.fn.confirm` at all --
/// there is no point blocking on a question whose "Yes" this chunk is about
/// to refuse anyway. Otherwise it returns `{ choice = N }`, nvim's own
/// documented `confirm()` contract (`:help confirm()`): `1` for Yes, `2` for
/// No, `0` for a force-closed dialog (`<Esc>` or an interrupt).
const TREE_DELETE_CONFIRM_CHUNK: &str = "\
local prompt, path = ...
local function canon(p)
  return vim.uv.fs_realpath(p) or vim.fn.fnamemodify(p, ':p')
end
local wanted = canon(path)
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(buf) and canon(vim.api.nvim_buf_get_name(buf)) == wanted then
    return { buffer_open = true }
  end
end
return { choice = vim.fn.confirm(prompt, '&Yes\\n&No') }";

/// What nvim did with an [`EngineHandle::set_buf_text`] batch.
///
/// A refusal is not an error: it is the ordinary outcome of the buffer
/// moving between the moment a caller computed its edits and the moment
/// nvim would have applied them, and the caller answers it by recomputing,
/// never by retrying the same rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufWriteOutcome {
    /// Every edit in the batch was applied, leaving the buffer at this
    /// `b:changedtick` -- what the caller's next write names, so it does
    /// not have to wait for the edit event still on its way to it.
    Applied { changedtick: u64 },
    /// Nothing was written: the buffer's `b:changedtick` had moved past the
    /// one the call named.
    BufferAdvanced,
}

/// Decodes [`BUF_SET_TEXT_CHUNK`]'s reply. Anything but an explicit
/// `applied = false` reads as applied: the chunk answers that key on every
/// path it takes, so a reply without it can only be a shape this crate has
/// never seen from the pinned engine, and treating an applied write as
/// refused would put an accepted hunk back on screen as undecided.
fn decode_buf_set_text_reply(reply: &Value) -> BufWriteOutcome {
    let applied = reply
        .as_map()
        .and_then(|pairs| crate::wire::map_find(pairs, "applied"))
        .and_then(Value::as_bool);
    if applied == Some(false) {
        return BufWriteOutcome::BufferAdvanced;
    }
    let changedtick = reply
        .as_map()
        .and_then(|pairs| crate::wire::map_find(pairs, "changedtick"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    BufWriteOutcome::Applied { changedtick }
}

/// Applies [`EngineHandle::set_buf_text`]'s batched edits via
/// `nvim_buf_set_text`, verified live against the pinned engine (see
/// `docs/buf-set-text-wire-capture.md`): `nvim_command('undojoin')` (`vim.cmd`
/// here) issued immediately before the loop's first `nvim_buf_set_text` call
/// links this whole batch onto the previous undo entry, and every row/col in
/// `edits` is passed straight through as the 0-indexed byte columns
/// `nvim_buf_set_text` itself expects. A batch targeting a buffer that no
/// longer exists throws inside the loop (`Invalid buffer id: N`), which
/// `nvim_exec_lua` surfaces as this request's `Err`, exactly like any other
/// rejected chunk here -- never a silently dropped edit.
///
/// The `expected` tick, when the caller names one, is checked here rather
/// than on the Rust side, and before the first edit runs: a check on the
/// caller's side of the wire cannot close the race, since the buffer can
/// move between that check and this apply, and checking mid-loop could
/// leave a batch half applied. A buffer whose tick has moved answers
/// `applied = false` with nothing written at all.
///
/// The guard is `type(expected) == 'number'`, not `expected ~= nil`:
/// msgpack's nil crosses into Lua as `vim.NIL`, a userdata sentinel that
/// is not `nil`, so an `expected ~= nil` test reads "no expectation" as an
/// expectation no tick can equal -- live-observed as every unstamped write
/// being refused.
///
/// The `undojoin` command itself is wrapped in `pcall`, live-confirmed
/// necessary: `:undojoin` throws `E790: undojoin is not allowed after undo`
/// whenever the immediately preceding action was an undo (`:help
/// undo-joining`), not only when there is no preceding undo entry at all.
/// An unguarded throw there aborts the whole chunk before the loop below
/// ever runs, silently dropping every edit in the batch -- an accepted diff
/// hunk that happened to land right after the user pressed `u` would simply
/// never apply. `pcall`'s own success flag is deliberately discarded: on
/// `E790` (or any other rejection) the edits still apply, just as their own
/// unjoined undo step, which is the fallback
/// [`view_core::msg::RpcCall::BufSetText`]'s own doc requires rather than
/// dropping an accepted hunk.
const BUF_SET_TEXT_CHUNK: &str = "\
local buf, undojoin, expected, edits = ...
if type(expected) == 'number' and vim.api.nvim_buf_get_changedtick(buf) ~= expected then
  return { applied = false }
end
if undojoin then
  pcall(vim.cmd, 'undojoin')
end
for _, edit in ipairs(edits) do
  vim.api.nvim_buf_set_text(buf, edit.start_row, edit.start_col, edit.end_row, edit.end_col, edit.lines)
end
return { applied = true, changedtick = vim.api.nvim_buf_get_changedtick(buf) }";

/// Reads the current buffer's path and nvim-authoritative text for
/// [`EngineHandle::read_current_buffer_text`], verified live against the
/// pinned engine (see `docs/ai-context-reads-wire-capture.md`): an unnamed
/// scratch buffer answers with `path = ''`, matching `PREVIEW_CHUNK`'s own
/// convention for the same case, and `text` is every line joined with `\n`
/// -- nvim's own buffer content, never the file on disk, so an unsaved edit
/// is what this reads.
const CURRENT_BUFFER_TEXT_CHUNK: &str = "\
local buf = vim.api.nvim_get_current_buf()
return { path = vim.api.nvim_buf_get_name(buf), text = table.concat(vim.api.nvim_buf_get_lines(buf, 0, -1, false), '\\n') }";

/// Reads the buffer-space cursor and, when one is active, the visual
/// selection for [`EngineHandle::read_cursor_context`], verified live
/// against the pinned engine (see `docs/ai-context-reads-wire-capture.md`).
/// `line`/`col` are `nvim_win_get_cursor`'s own values as they cross the
/// wire (1-indexed line, 0-indexed byte column -- nvim's own mixed
/// convention); [`decode_cursor_context_reply`] is what renormalizes `col`
/// to the single 1-indexed convention every `EngineReadSnapshot` position
/// field shares, not this chunk. A selection is considered active exactly
/// when `nvim_get_mode()` reports one of the three visual submodes (`v`,
/// `V`, or blockwise `\22`) at the moment of the call -- stale `'<`/`'>`
/// marks left over from a selection the user already exited are
/// deliberately not read, since those persist long after the selection
/// that set them is gone and would otherwise misreport "active" forever.
///
/// While active, the selection's endpoints come from `getpos('v')` (the
/// anchor) and `getpos('.')` (the cursor), reordered so
/// `selection_start <= selection_end` regardless of which direction the
/// user selected in. The three submodes read their text differently, each
/// live-verified (see `docs/ai-context-reads-wire-capture.md`):
///
/// - Charwise (`v`): `nvim_buf_get_text` on the endpoints' byte columns,
///   with the end column extended past the full last character rather than
///   `getpos`'s own byte offset of that character's first byte --
///   live-confirmed that passing `getpos`'s raw column straight through as
///   an exclusive end (this chunk's original, buggy form) truncates mid
///   multi-byte UTF-8 sequence, producing an invalid string the decoder
///   silently turns into "no selection" rather than the actual text.
/// - Linewise (`V`): every full line from `selection_start` to
///   `selection_end`, ignoring both endpoints' columns entirely -- a
///   linewise selection has none, by nvim's own definition of the mode.
/// - Blockwise (`\22`): a SCREEN-column rectangle joined with `\n`. Byte
///   columns alone (this chunk's original, round-1 form) are wrong whenever
///   a row contains a multi-byte character: the rectangle's bounds are
///   shared screen columns held constant across every row, and a given
///   screen column lands at a different byte offset on each row depending
///   on how many multi-byte characters precede it there -- live-confirmed
///   the round-1 form sliced mid-character on such a row, producing invalid
///   UTF-8 the decoder silently turned into "no selection" (see
///   `docs/ai-context-reads-wire-capture.md`, "Fix round 2"). The rectangle
///   itself is bounded by `virtcol('v', 1)[1]`/`virtcol('.', 1)[1]` (the
///   LIST form's START cell) for the low bound and the plain SCALAR form
///   (the END cell) for the high bound -- never the scalar form for the low
///   bound, which is wrong on any row not containing the multi-cell
///   character (a tab, an East-Asian-wide character) that defines it:
///   live-confirmed a scalar-only low bound built from a leading tab's own
///   end cell (screen col 8) shifted every OTHER row's rectangle 8 columns
///   right (see "Fix round 3"). `blockwise_row_text` walks each row's
///   screen columns via `vim.fn.virtcol2col(win, lnum, vcol)` and
///   `vim.fn.virtcol({lnum, byte}, 1)`, copying a character's raw bytes
///   when its own span sits entirely inside `[lo_vcol, hi_vcol]` and
///   padding with one space per covered cell when the rectangle only
///   partially covers it (on either edge) -- nvim's own block-yank
///   behavior for a tab or wide character split by the rectangle's edge,
///   never that character's raw, unsplittable bytes. A `$`-block
///   (`getcurpos()`'s `curswant` field, 1-indexed `getcurpos()[5]` in Lua,
///   equal to nvim's `MAXCOL` sentinel `2147483647`) extends every row to
///   its own end instead of the shared screen-column bound, expressed as a
///   per-row high bound (`virtcol({row, '$'}) - 1`) fed through that same
///   walker rather than a raw byte slice -- a raw slice skips the padding
///   rule, and a `$`-block's shared LOW bound splits a multi-cell
///   character just as readily (live-confirmed against a leading tab).
///   A row ending strictly before `lo_vcol` contributes
///   `pad_vcol - lo_vcol + 1` spaces, nvim's own padding for a line too
///   short to reach the block at all; the boundary is exact and
///   asymmetric -- a row reaching `lo_vcol - 1` contributes nothing, and a
///   row merely ending INSIDE the rectangle contributes only the cells it
///   has, never trailing padding. `pad_vcol` is the shared `hi_vcol` for an
///   ordinary block, but for a `$`-block it is the WIDEST row's
///   `virtcol({row, '$'})` across the whole block, computed in one pre-pass
///   -- the per-row high bound a `$`-block otherwise uses would size a
///   short row's padding against that row's own end, collapsing it to
///   nothing. Only an INTERIOR row can reach that branch under a
///   `$`-block: `lo_vcol` is the minimum of the two ENDPOINT rows' own
///   virtcols and `$` puts the cursor at its row's end, so neither endpoint
///   row can end strictly before it.
///
/// The `selection_*` keys are simply absent from the reply when no
/// selection is active, the same "absent key, not a null" convention
/// `PREVIEW_CHUNK`'s `loaded: false` case uses.
const CURSOR_CONTEXT_CHUNK: &str = "\
local function line_text(line_number)
  return vim.api.nvim_buf_get_lines(0, line_number - 1, line_number, false)[1] or ''
end
local function byte_end_of_char(line, byte_col0)
  local charidx = vim.fn.charidx(line, byte_col0)
  if charidx == -1 then
    return #line
  end
  local nextbyte = vim.fn.byteidx(line, charidx + 1)
  if nextbyte == -1 then
    return #line
  end
  return nextbyte
end
local function blockwise_row_text(win, row, lo_vcol, hi_vcol, dollar_block, pad_vcol)
  local line = line_text(row)
  local end_vcol = vim.fn.virtcol({ row, '$' })
  if dollar_block then
    hi_vcol = end_vcol - 1
  end
  if end_vcol < lo_vcol then
    return string.rep(' ', math.max(pad_vcol - lo_vcol + 1, 0))
  end
  local parts = {}
  local v = lo_vcol
  while v <= hi_vcol and v < end_vcol do
    local byte1 = vim.fn.virtcol2col(win, row, v)
    local span = vim.fn.virtcol({ row, byte1 }, 1)
    local start_v, char_end_v = span[1], span[2]
    local covered_end = math.min(char_end_v, hi_vcol)
    if start_v < v or char_end_v > hi_vcol then
      parts[#parts + 1] = string.rep(' ', covered_end - v + 1)
    else
      local char_end_byte = byte_end_of_char(line, byte1 - 1)
      parts[#parts + 1] = string.sub(line, byte1, char_end_byte)
    end
    v = covered_end + 1
  end
  return table.concat(parts)
end
local cur = vim.api.nvim_win_get_cursor(0)
local out = { line = cur[1], col = cur[2] }
local mode = vim.api.nvim_get_mode().mode
if mode == 'v' or mode == 'V' or mode == '\\22' then
  local vstart = vim.fn.getpos('v')
  local vend = vim.fn.getpos('.')
  local srow, scol, erow, ecol = vstart[2], vstart[3], vend[2], vend[3]
  if srow > erow or (srow == erow and scol > ecol) then
    srow, scol, erow, ecol = erow, ecol, srow, scol
  end
  local text
  if mode == 'V' then
    text = table.concat(vim.api.nvim_buf_get_lines(0, srow - 1, erow, false), '\\n')
  elseif mode == '\\22' then
    local win = vim.api.nvim_get_current_win()
    local lo_vcol = math.min(vim.fn.virtcol('v', 1)[1], vim.fn.virtcol('.', 1)[1])
    local hi_vcol = math.max(vim.fn.virtcol('v'), vim.fn.virtcol('.'))
    local dollar_block = vim.fn.getcurpos()[5] == 2147483647
    local pad_vcol = hi_vcol
    if dollar_block then
      pad_vcol = 0
      for row = srow, erow do
        pad_vcol = math.max(pad_vcol, vim.fn.virtcol({ row, '$' }))
      end
    end
    local rows = {}
    for row = srow, erow do
      rows[#rows + 1] = blockwise_row_text(win, row, lo_vcol, hi_vcol, dollar_block, pad_vcol)
    end
    text = table.concat(rows, '\\n')
  else
    local endline = line_text(erow)
    local end_byte0 = byte_end_of_char(endline, ecol - 1)
    local lines = vim.api.nvim_buf_get_text(0, srow - 1, scol - 1, erow - 1, end_byte0, {})
    text = table.concat(lines, '\\n')
  end
  out.selection_text = text
  out.selection_start = srow
  out.selection_end = erow
end
return out";

/// Reads every current entry from `vim.diagnostic.get(0)` for
/// [`EngineHandle::read_diagnostic_entries`], verified live against the
/// pinned engine (see `docs/ai-context-reads-wire-capture.md`): `lnum`/`col`
/// cross the wire as the diagnostic API's own 0-indexed byte positions,
/// verbatim from this chunk -- [`decode_diagnostic_entries_reply`] is what
/// renormalizes both to the single 1-indexed convention every
/// `EngineReadSnapshot` position field shares (matching
/// [`QUICKFIX_ENTRIES_CHUNK`]'s source, already 1-indexed on the wire).
/// `severity` is nvim's own `vim.diagnostic.severity` integer (`1`=Error ..
/// `4`=Hint), mapped onto [`DiagnosticSeverity`] by
/// [`decode_diagnostic_entries_reply`].
const DIAGNOSTIC_ENTRIES_CHUNK: &str = "\
local out = {}
for _, d in ipairs(vim.diagnostic.get(0)) do
  out[#out + 1] = { line = d.lnum, col = d.col, severity = d.severity, message = d.message }
end
return out";

/// Reads every current entry from `getqflist()` for
/// [`EngineHandle::read_quickfix_entries`], verified live against the pinned
/// engine (see `docs/ai-context-reads-wire-capture.md`). `getqflist()`
/// itself carries no `filename` field per entry -- only `bufnr`, live-
/// confirmed against the pinned engine -- so this chunk resolves each
/// entry's path via `nvim_buf_get_name(bufnr)`, falling back to an empty
/// string for `bufnr == 0` (an entry with no buffer at all, the same
/// `PREVIEW_CHUNK`/`CURRENT_BUFFER_TEXT_CHUNK` convention for "no name").
/// `lnum`/`col` are `getqflist`'s own 1-indexed values, verbatim.
const QUICKFIX_ENTRIES_CHUNK: &str = "\
local out = {}
for _, item in ipairs(vim.fn.getqflist()) do
  local path = ''
  if item.bufnr and item.bufnr ~= 0 then
    path = vim.api.nvim_buf_get_name(item.bufnr)
  end
  out[#out + 1] = { path = path, line = item.lnum, col = item.col, text = item.text }
end
return out";

/// The `ext_*` UI capabilities [`EngineHandle::ui_attach`] requests. Public
/// so a corpus/oracle runner attaching its own reference connection can
/// request the identical set nvim sees from the real paint loop, rather
/// than restating the list and risking the two drifting apart.
pub const UI_EXT_OPTIONS: &[&str] = &[
    "ext_linegrid",
    "ext_cmdline",
    "ext_popupmenu",
    "ext_messages",
    "ext_tabline",
];

/// The child descriptor [`crate::process::EngineConfig::with_stdin_relay`]
/// duplicates the caller's own stdin onto, and the value
/// [`EngineHandle::ui_attach_with_stdin_relay`] sends as `nvim_ui_attach`'s
/// `stdin_fd` option. Fixed at 3 rather than discovered at runtime: child
/// fd 0 is `--embed`'s own RPC write end and fd 1 its read end (see
/// `build_command` in `process.rs`), fd 2 is `Stdio::null()`, and
/// `:help ui-startup-stdin` names exactly this constraint ("fd=0 is
/// already... used to send RPC data... it must use some other file
/// descriptor, like fd=3 or higher").
pub(crate) const STDIN_RELAY_CHILD_FD: i32 = 3;

impl EngineHandle {
    /// Attaches this connection as nvim's UI at `width` x `height` cells
    /// with the full set of native-rendering extensions enabled:
    /// `ext_linegrid`, `ext_cmdline`, `ext_popupmenu`, `ext_messages`, and
    /// `ext_tabline`. Without these, nvim falls back to painting cmdline,
    /// messages, popupmenu, and tabline content directly into the grid,
    /// which this frontend has no way to distinguish from ordinary buffer
    /// text; attaching all five up front is what makes
    /// [`crate::ui_events::decode_redraw`]'s mode/cmdline/messages/tabline/
    /// popupmenu variants reachable at all.
    ///
    /// A `request`, not a `notify`: the caller needs to know attach succeeded
    /// before entering the paint loop. This is the only request the paint
    /// loop's setup makes; every nvim call issued once the loop is running
    /// goes through `notify` instead, so a slow response never stalls a
    /// frame. Bounded by `UI_ATTACH_TIMEOUT` rather than unbounded, since
    /// the caller has typically already put the terminal into raw mode by
    /// this point, and an unresponsive engine must not freeze it forever.
    ///
    /// # Errors
    ///
    /// Returns the `EngineError` from the underlying request if it fails,
    /// nvim rejects the attach, or the reply does not arrive within
    /// `UI_ATTACH_TIMEOUT`.
    pub fn ui_attach(&self, width: u16, height: u16) -> Result<(), EngineError> {
        let opts = UI_EXT_OPTIONS
            .iter()
            .map(|&name| (Value::from(name), Value::from(true)))
            .collect();
        self.request_timeout(
            "nvim_ui_attach",
            vec![Value::from(width), Value::from(height), Value::Map(opts)],
            UI_ATTACH_TIMEOUT,
        )?;
        Ok(())
    }

    /// Identical to [`ui_attach`](Self::ui_attach), plus the `stdin_fd`
    /// option naming [`STDIN_RELAY_CHILD_FD`] as the descriptor nvim should
    /// read piped stdin content from (`:help ui-startup-stdin`), for a
    /// caller whose `EngineConfig` was built with
    /// [`with_stdin_relay`](crate::process::EngineConfig::with_stdin_relay).
    ///
    /// A second, additive method rather than a parameter on `ui_attach`
    /// itself: every other caller across this workspace (the oracle, every
    /// live integration test, `view`'s own ordinary startup) attaches with
    /// no relay and would otherwise all need a new argument they never use.
    ///
    /// # Errors
    ///
    /// Same as [`ui_attach`](Self::ui_attach).
    pub fn ui_attach_with_stdin_relay(&self, width: u16, height: u16) -> Result<(), EngineError> {
        let mut opts: Vec<(Value, Value)> = UI_EXT_OPTIONS
            .iter()
            .map(|&name| (Value::from(name), Value::from(true)))
            .collect();
        opts.push((
            Value::from("stdin_fd"),
            Value::from(i64::from(STDIN_RELAY_CHILD_FD)),
        ));
        self.request_timeout(
            "nvim_ui_attach",
            vec![Value::from(width), Value::from(height), Value::Map(opts)],
            UI_ATTACH_TIMEOUT,
        )?;
        Ok(())
    }

    /// Registers a one-shot `VimEnter` autocmd whose callback issues a
    /// BLOCKING `rpcrequest(channel_id, 'view_vim_enter')` back to this
    /// connection -- the end-to-end proof that `update()`'s
    /// `Msg::EngineRequest(EngineRequest::VimEnter)` arm and its
    /// `Effect::Reply` actually unblock nvim's own main loop, not merely
    /// that the message decodes (a deadlock here hangs startup forever).
    ///
    /// # Ordering: call this BEFORE [`ui_attach`](Self::ui_attach), never
    /// after
    ///
    /// Live-verified against a real `nvim --clean --embed`: registering
    /// this autocmd immediately AFTER `ui_attach` returns loses
    /// the race entirely -- a `--clean` startup's config sourcing and
    /// `VimEnter` dispatch were both already complete (300+ redraw damage
    /// events already staged) by the time the registration request even
    /// reached nvim's main loop. The embed contract's "attach precedes
    /// config sourcing" guarantee protects exactly the window BEFORE
    /// `ui_attach`: nvim services ordinary requests on this connection
    /// freely while blocked waiting for a UI to attach, but cannot begin
    /// sourcing config (and thus cannot fire `VimEnter`) until `ui_attach`
    /// itself returns. Registering here, before that call, is what actually
    /// wins the race; after it is not "usually late", it is unconditionally
    /// too late for a `--clean`-speed startup.
    ///
    /// `channel_id` is this connection's own id from `nvim_get_api_info`
    /// (captured in [`crate::process::Engine::api_info`] at spawn time): a
    /// self-targeted `rpcrequest` needs an explicit channel number, and nvim
    /// has no "loopback" shorthand for dispatching a request back to the
    /// very connection asking.
    ///
    /// A `request`, not a `notify`, for the same reason [`ui_attach`]
    /// (Self::ui_attach) is: the caller needs to know the autocmd is live
    /// before it dares call `ui_attach`, or config sourcing could start
    /// racing an unregistered hook.
    ///
    /// # Errors
    ///
    /// Returns the `EngineError` from the underlying request if it fails,
    /// nvim rejects the command, or the reply does not arrive within
    /// [`REGISTER_VIM_ENTER_TIMEOUT`].
    pub fn register_vim_enter_autocmd(&self, channel_id: u64) -> Result<(), EngineError> {
        let cmd =
            format!("autocmd VimEnter * ++once call rpcrequest({channel_id}, 'view_vim_enter')");
        self.request_timeout(
            "nvim_command",
            vec![Value::from(cmd)],
            REGISTER_VIM_ENTER_TIMEOUT,
        )?;
        Ok(())
    }

    /// Registers the single `view_bridge` autocmd group -- the one channel
    /// every editor-state change view reacts to arrives on. What it hooks,
    /// and why it is one group rather than three, is in
    /// [`REGISTER_BRIDGE_CHUNK`]. Each trigger answers asynchronously with a
    /// `view_bridge` notification carrying an event name and the event's
    /// `match`; `colorscheme` becomes `Msg::ColorSchemeChanged`.
    ///
    /// # Ordering: call this BEFORE [`ui_attach`](Self::ui_attach), never
    /// after
    ///
    /// The window this needs is the one
    /// [`register_vim_enter_autocmd`](Self::register_vim_enter_autocmd)
    /// documents in full: nvim cannot begin sourcing the user's config until
    /// `ui_attach` returns, and a config whose `:colorscheme` fires before
    /// this group exists is a switch nothing observes -- the cold-start cache
    /// then keeps whatever it was seeded with until the user changes scheme a
    /// second time.
    ///
    /// A `notify`, where `register_vim_enter_autocmd` is a `request`, and the
    /// difference is not an inconsistency. That one must be *live* before
    /// `ui_attach` is called, because what it registers is a hook nvim will
    /// block on. This one only needs to be *ordered* before it: the writer
    /// thread preserves the order calls are made in and nvim services one
    /// connection's stream in order, so this chunk runs before nvim answers
    /// the `ui_attach` request and therefore before config sourcing can fire
    /// anything. Waiting for a reply would buy nothing and would put a
    /// bounded blocking call on the same trait the paint loop drives.
    ///
    /// The cost of a notify is that a chunk nvim rejects fails silently.
    /// [`REGISTER_BRIDGE_CHUNK`] is constant, so the only way it can fail is
    /// an engine that is already gone -- which the very next call reports --
    /// and the arrival of a real notification is asserted end-to-end against
    /// a live engine rather than inferred from a reply.
    ///
    /// `channel_id` is this connection's own id from `nvim_get_api_info`
    /// (captured in [`crate::process::Engine::api_info`] at spawn time),
    /// needed for the same reason
    /// [`register_mappings`](Self::register_mappings) needs it: `rpcnotify`
    /// dispatches to an explicit channel number and nvim has no loopback
    /// shorthand for the connection asking.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn register_bridge(&self, channel_id: u64) -> Result<(), EngineError> {
        self.notify(
            "nvim_exec_lua",
            vec![
                Value::from(REGISTER_BRIDGE_CHUNK),
                Value::Array(vec![Value::from(channel_id)]),
            ],
        )
    }

    /// Injects view's `g:clipboard` provider (see
    /// [`REGISTER_CLIPBOARD_CHUNK`]). A `notify`, like `register_bridge`:
    /// this only needs to be *ordered*, never *live before `ui_attach`
    /// returns* -- and in fact cannot be issued that early at all, since the
    /// user's config has not sourced yet at that point. The precedence check
    /// it performs (leave an existing `g:clipboard` untouched) depends on
    /// exactly the fact that only exists once sourcing is done: whether the
    /// user's config set it.
    ///
    /// `channel_id` is this connection's own id from `nvim_get_api_info`,
    /// needed for the same reason [`register_bridge`](Self::register_bridge)
    /// needs it: the registered closures dispatch back to this connection
    /// by explicit channel number.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn register_clipboard(&self, channel_id: u64) -> Result<(), EngineError> {
        self.notify(
            "nvim_exec_lua",
            vec![
                Value::from(REGISTER_CLIPBOARD_CHUNK),
                Value::Array(vec![Value::from(channel_id)]),
            ],
        )
    }

    /// Forwards one encoded key `notation` (see `view_tui::keys::encode_key`)
    /// to nvim via `nvim_input`.
    ///
    /// Fire-and-forget: the paint loop calls this once per keystroke and must
    /// never block waiting for nvim to process it, or one slow keystroke
    /// stalls every frame queued behind it.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn input(&self, notation: &str) -> Result<(), EngineError> {
        self.notify("nvim_input", vec![Value::from(notation)])
    }

    /// Queues one encoded key `notation` into nvim's typeahead via
    /// `feedkeys()` in `"t"` mode: remapped and accounted for exactly as if
    /// the user had typed it, appended after anything already queued, and
    /// left for the main loop to consume rather than executed inline (no
    /// `"x"` flag).
    ///
    /// Distinct from [`input`](Self::input), not a replacement for it. Keys
    /// sent through `nvim_input` land in nvim's low-level input buffer and
    /// move into the typeahead buffer in pieces, so a driver watching for
    /// nvim's own idle signal can observe an empty typeahead while the tail
    /// of what it sent is still queued a layer further out -- live-observed
    /// as a `SafeState` firing in the middle of a script. `feedkeys()`
    /// inserts the whole string into the typeahead in one step instead,
    /// which is what a caller sending a *script* it intends to wait for
    /// needs. `input` remains the right call for a single interactive
    /// keystroke and the only one that reaches a session already blocked in
    /// a key-wait, which never services the deferred request this one is.
    ///
    /// The `<...>` notation is translated by `nvim_replace_termcodes`
    /// inside nvim rather than translated here and passed as key bytes:
    /// those bytes carry `K_SPECIAL` (`0x80`) prefixes and are not valid
    /// UTF-8, so they cannot survive a round trip through the wire's string
    /// type on this side. Only the notation itself -- plain text -- crosses
    /// the connection.
    ///
    /// `nvim_exec_lua(String code, Array args) -> Object` (verified against
    /// the pinned engine's own `api_info`) with `notation` passed as an
    /// *argument*, not interpolated into the chunk: a constant chunk cannot
    /// be escaped by anything a caller sends. Quoting the notation into a
    /// command string instead would make every quote, backslash, and
    /// newline in a script a correctness question -- and a literal newline
    /// would end the command outright, a way to lose a script that
    /// `nvim_input` never had.
    ///
    /// A request, not a notify: a rejected chunk (a runtime error inside
    /// nvim) must surface as an error rather than as a script that silently
    /// never ran.
    ///
    /// # Errors
    ///
    /// Returns the `EngineError` from the underlying request if it fails,
    /// nvim rejects the call, or the reply does not arrive within
    /// [`EVAL_TIMEOUT`].
    pub fn feed_keys(&self, notation: &str) -> Result<(), EngineError> {
        self.request_timeout(
            "nvim_exec_lua",
            vec![
                Value::from(FEED_KEYS_CHUNK),
                Value::Array(vec![Value::from(notation)]),
            ],
            EVAL_TIMEOUT,
        )?;
        Ok(())
    }

    /// Notifies nvim of a terminal resize to `width` x `height` cells via
    /// `nvim_ui_try_resize`.
    ///
    /// Fire-and-forget for the same reason as [`input`](Self::input): resize
    /// events arrive inside the paint loop and must not block it.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn try_resize(&self, width: u16, height: u16) -> Result<(), EngineError> {
        self.notify(
            "nvim_ui_try_resize",
            vec![Value::from(width), Value::from(height)],
        )
    }

    /// Streams `text` into nvim via `nvim_paste` as a single non-streamed
    /// call (`phase = -1`, per `nvim --api-info`'s
    /// `nvim_paste(String data, Boolean crlf, Integer phase)` signature),
    /// with no line-ending translation (`crlf = false`): terminal input
    /// already arrives with the pty's own newline convention, so nvim must
    /// not perform an additional CRLF fixup on top of it. Routing paste
    /// through `nvim_paste` rather than replaying it as `nvim_input`
    /// keystrokes avoids mid-paste mappings, autoindent mangling, and a
    /// separate undo unit per line.
    ///
    /// Fire-and-forget for the same reason as [`input`](Self::input): a
    /// bracketed paste must not block the paint loop waiting for nvim to
    /// finish inserting it.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn paste(&self, text: &str) -> Result<(), EngineError> {
        self.notify(
            "nvim_paste",
            vec![Value::from(text), Value::from(false), Value::from(-1)],
        )
    }

    /// Forwards one mouse event to nvim via `nvim_input_mouse`, per
    /// `nvim --api-info`'s `nvim_input_mouse(String button, String action,
    /// String modifier, Integer grid, Integer row, Integer col)` signature
    /// (verified with a live capture, not memory: the parameter order and
    /// names come straight from that decode). `grid` is hardcoded to `0`
    /// (single-grid semantics per the same doc: "0 to let Nvim decide
    /// positioning of windows"), since this frontend has no multigrid
    /// window layout of its own to report.
    ///
    /// Fire-and-forget for the same reason as [`input`](Self::input): a
    /// mouse event arrives inside the paint loop and must not block it.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn input_mouse(
        &self,
        button: &str,
        action: &str,
        modifier: &str,
        row: u16,
        col: u16,
    ) -> Result<(), EngineError> {
        self.notify(
            "nvim_input_mouse",
            vec![
                Value::from(button),
                Value::from(action),
                Value::from(modifier),
                Value::from(0),
                Value::from(row),
                Value::from(col),
            ],
        )
    }

    /// Clears and redraws nvim's screen, which also retracts every message it
    /// currently considers shown (a `msg_clear` on the redraw stream).
    ///
    /// The retraction is what callers want. With `ext_messages` attached
    /// nvim draws no message into the grid at all, so a report the user did
    /// not ask for -- a swap-recovery report, most of all -- is view's own
    /// overlay from the moment it decodes, and stays up until nvim says it
    /// is over.
    ///
    /// `:mode` rather than `:redraw!`, measured against the pinned engine
    /// rather than assumed: both repaint, and only `:mode` retracts. A
    /// `:redraw!` issued over this channel emits the fresh viewport and
    /// nothing else, leaving every message nvim had shown still shown, while
    /// `:mode` emits `grid_clear` + `msg_clear` -- the same pair the `<C-l>`
    /// a user would otherwise have to type produces, which is the whole
    /// point of issuing this for them. `redraw_live.rs`'s
    /// `a_redraw_retracts_the_messages_nvim_had_shown` pins it.
    ///
    /// A notification, not a request, for the same reason
    /// [`input`](Self::input) is one: this is issued from the runtime loop,
    /// which must never block on nvim, and nothing here reads a result.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn redraw(&self) -> Result<(), EngineError> {
        self.notify("nvim_command", vec![Value::from("mode")])
    }

    /// Reads [`SWAP_RECOVERY_PROBE`] -- what this engine replayed out of a
    /// swap file while starting, whether it wrote its own report about doing
    /// so, and the error it raised if the recovery could not be performed --
    /// as an async request whose answer crosses back as `Msg::SwapRecovered`,
    /// tagged `generation`, through the connection's pump.
    ///
    /// Async by construction, like [`probe_default_hl`](Self::probe_default_hl):
    /// the caller is the runtime loop and a synchronous `nvim_eval` there
    /// would park the whole session on the engine it is asking.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited.
    pub fn probe_swap_recovery(&self, generation: u64) -> Result<(), EngineError> {
        self.request_swap_recovery(
            "nvim_eval",
            vec![Value::from(SWAP_RECOVERY_PROBE)],
            generation,
        )
    }

    /// Sets option `name` to `value` via `nvim_set_option_value(String
    /// name, Object value, Dict opts)`, with an empty `opts` map: no
    /// `win`/`buf` key means nvim applies the change the way `:set` does,
    /// which is what a session-wide takeover of a surface needs.
    ///
    /// A notification, not a request: nothing waits on the result, so the
    /// paint loop that emitted it never blocks on nvim's reply. A rejected
    /// option name surfaces as an nvim error message rather than as an
    /// `Err` here, the same tradeoff every other fire-and-forget wrapper on
    /// this handle makes.
    pub fn set_option(&self, name: &str, value: &OptionValue) -> Result<(), EngineError> {
        self.notify(
            "nvim_set_option_value",
            vec![
                Value::from(name),
                option_value(value),
                Value::Map(Vec::new()),
            ],
        )
    }

    /// Sets `name` to `value` and installs a session-lifetime guard that
    /// puts it back whenever anything else changes it: the durable takeover
    /// [`crate::RpcCall::HoldOption`] describes.
    ///
    /// One `nvim_exec_lua` chunk rather than a set call followed by an
    /// autocmd call, because the two halves are not independently useful: a
    /// takeover that set the option but failed to install its guard is the
    /// silent lapse this whole call exists to prevent, and there is no
    /// caller that wants one without the other. `name` and `value` ride as
    /// *arguments* to a constant chunk (same rule as
    /// [`feed_keys`](Self::feed_keys)): no option name or value can escape
    /// into the Lua source.
    ///
    /// The guard lives in a per-option augroup created with `clear = true`,
    /// so re-applying a plan replaces both of its autocommands rather than
    /// stacking a second pair, and it compares before it writes, so an
    /// unrelated event costs one option read. Why it takes two events, and
    /// which write each one catches, is in [`HOLD_OPTION_CHUNK`].
    ///
    /// A notification, not a request, like every other call the paint loop
    /// may emit: nothing waits on the result.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn hold_option(&self, name: &str, value: &OptionValue) -> Result<(), EngineError> {
        self.notify(
            "nvim_exec_lua",
            vec![
                Value::from(HOLD_OPTION_CHUNK),
                Value::Array(vec![Value::from(name), option_value(value)]),
            ],
        )
    }

    /// Evaluates `expr` via `nvim_eval` and renders the result as a string,
    /// the state-parity probe engine-attached oracles use to compare their
    /// decoded screen state against nvim's own ground truth (buffer text,
    /// cursor position, mode, register contents -- any vimscript expression
    /// a probe needs to read back).
    ///
    /// `nvim_eval(String expr) -> Object` (verified via a live `nvim
    /// --api-info` capture, decoded with `rmpv`: a single positional string
    /// argument, a msgpack `Object` result of whatever type the expression
    /// itself evaluates to -- `getline(1)` returns a String, `line('.')`
    /// returns an Integer, `mode()` returns a String). Rendered by
    /// [`value_to_string`] into a plain `String` rather than leaking
    /// `rmpv::Value` past the engine boundary: `scripts/audit-deps.sh`
    /// confines `rmpv` to `view-engine`, and this is the sanctioned way a
    /// typed caller (the oracle's `EngineSession`) reaches the same result
    /// without constructing or matching on a wire value itself.
    ///
    /// # Errors
    ///
    /// Returns the `EngineError` from the underlying request if it fails,
    /// nvim rejects the expression (a vimscript error), or the reply does
    /// not arrive within [`EVAL_TIMEOUT`].
    pub fn eval_str(&self, expr: &str) -> Result<String, EngineError> {
        let value = self.request_timeout("nvim_eval", vec![Value::from(expr)], EVAL_TIMEOUT)?;
        Ok(value_to_string(&value))
    }

    /// Runs `cmd` as an ex command via `nvim_command`, waiting for nvim to
    /// finish executing it (or fail) before returning.
    ///
    /// `nvim_command(String command) -> nil` (verified against the pinned
    /// engine's own `api_info`; the decoded capture backing this signature
    /// lives in `docs/stdin-relay-wire-capture.md`'s `nvim --api-info`
    /// section): a *request*, not `notify`, and
    /// deliberately so -- a caller that needs a synchronous barrier ahead
    /// of a command that may end the connection outright (`:cq`, `:qa!`)
    /// needs the request's own `Err` as proof the command already ran,
    /// which a fire-and-forget `notify` cannot provide (contrast
    /// [`input`](Self::input), whose whole point is never blocking on
    /// nvim). An ordinary command that answers normally just returns
    /// `Ok(())`.
    ///
    /// # Errors
    ///
    /// Returns the `EngineError` from the underlying request if it fails,
    /// nvim rejects the command (a vimscript error), the connection closes
    /// before a reply arrives (e.g. `:cq`/`:qa!` ending the process
    /// mid-request), or the reply does not arrive within [`EVAL_TIMEOUT`].
    pub fn command(&self, cmd: &str) -> Result<(), EngineError> {
        self.request_timeout("nvim_command", vec![Value::from(cmd)], EVAL_TIMEOUT)?;
        Ok(())
    }

    /// Reads nvim's current mode name and blocked flag via `nvim_get_mode`
    /// (`nvim_get_mode() -> {"mode": String, "blocking": Boolean}`, the
    /// mode string in `mode(1)`'s own format). Unlike every other request
    /// here, `nvim_get_mode` is one of the few the pinned nvim documents as
    /// non-deferred, or `fast` (`:help api-fast` names it outright): nvim
    /// answers it immediately on receipt, even while its main loop is
    /// blocked waiting for a key -- a hit-enter prompt, a pending
    /// `t`/`f`/`r` character argument, a register name after `"` -- states
    /// in which a deferred request like `nvim_eval` waits until the key
    /// arrives (live-verified against the pinned nvim: `nvim_eval` times
    /// out in every `blocking = true` state this reply reports, while this
    /// call still answers). That makes it the one probe an embedded driver
    /// can use to distinguish "engine is wedged" from "engine is
    /// deliberately waiting for a key", which is what `view-oracle`'s
    /// quiesce and snapshot machinery calls it for.
    ///
    /// The API metadata is no second opinion on any of that, in either
    /// direction: the pinned engine reports no per-function `fast` flag at
    /// all (absent on every entry, including functions its own
    /// documentation names as `fast`), so a method's flag reading as unset
    /// there is the absence of an answer rather than a negative one.
    ///
    /// # Errors
    ///
    /// Returns the `EngineError` from the underlying request if it fails,
    /// the reply does not arrive within [`GET_MODE_TIMEOUT`], or the reply
    /// is not the documented map shape (surfaced as
    /// [`RpcError::Malformed`] rather than degraded to a placeholder a
    /// differential comparison could silently accept on both sides).
    pub fn get_mode(&self) -> Result<(String, bool), EngineError> {
        let value = self.request_timeout("nvim_get_mode", vec![], GET_MODE_TIMEOUT)?;
        let malformed =
            || EngineError::Rpc(RpcError::Malformed(format!("nvim_get_mode reply: {value}")));
        let Value::Map(pairs) = &value else {
            return Err(malformed());
        };
        let mut mode = None;
        let mut blocking = None;
        for (key, val) in pairs {
            match key.as_str() {
                Some("mode") => mode = val.as_str().map(str::to_string),
                Some("blocking") => blocking = val.as_bool(),
                _ => {}
            }
        }
        match (mode, blocking) {
            (Some(mode), Some(blocking)) => Ok((mode, blocking)),
            _ => Err(malformed()),
        }
    }

    /// Issues `nvim_get_hl(0, {name = "Normal"})` as an async probe tagged
    /// with `generation`, resolving the wire ambiguity in
    /// `default_colors_set`'s `rgb_bg`/`rgb_fg` (nvim sends `0` both for
    /// "unset" and for "genuinely black/default-fg-colored"; a probe
    /// reply's `fg`/`bg` map key presence is what disambiguates the two --
    /// see [`crate::handle::EngineHandle::request_probe`]'s doc comment for
    /// the live-verified reply shapes). Async by construction: this issues
    /// the request via [`EngineHandle::request_probe`] and returns
    /// immediately; the reply crosses back as `Msg::HlProbeReply` through
    /// the connection's pump, never by blocking this call.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited.
    pub fn probe_default_hl(&self, generation: u64) -> Result<(), EngineError> {
        self.request_probe(
            "nvim_get_hl",
            vec![
                Value::from(0),
                Value::Map(vec![(Value::from("name"), Value::from("Normal"))]),
            ],
            generation,
        )
    }
    /// Registers `specs` as real nvim mappings and the `:View` command in
    /// one [`REGISTER_MAPPINGS_CHUNK`] pass, notifying back to `channel_id`
    /// when either is used.
    ///
    /// Async by construction, like [`probe_default_hl`](Self::probe_default_hl):
    /// this issues the request through
    /// [`EngineHandle::request_mappings`] and returns immediately, and the
    /// claim list crosses back as `Msg::MappingsClaimed` through the
    /// connection's pump. The caller is the runtime loop, which never awaits
    /// an RPC reply.
    ///
    /// A request rather than a notification, unlike the other calls the loop
    /// emits: the reply is the claim list, and an error reply is how a chunk
    /// nvim refused surfaces at all instead of as keys that silently never
    /// registered.
    ///
    /// The `:View` completion candidates come from
    /// [`default_maps`](view_core::native::mappings::default_maps) rather
    /// than from `specs`: the command is registered whatever the user has
    /// turned off, so what it completes is every entry point this build
    /// has, not the subset this session mapped a key to.
    ///
    /// A spec whose tokens
    /// [cannot be spelled](view_core::native::mappings::is_spellable) inside
    /// the mapping the chunk generates is dropped here rather than sent: this
    /// method takes any `&[MappingSpec]`, and the table's own vetting in
    /// `view-core` cannot speak for a spec a future caller assembles.
    /// Dropping is the safe direction -- view registers nothing, so the key
    /// stays whatever the user's config made it -- and the omission is
    /// visible, since a dropped spec returns no claim either.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited.
    pub fn register_mappings(
        &self,
        specs: &[MappingSpec],
        channel_id: u64,
    ) -> Result<(), EngineError> {
        let specs = specs
            .iter()
            .filter(|spec| is_spellable(spec))
            .map(|spec| {
                Value::Map(vec![
                    (Value::from("feature"), Value::from(spec.feature)),
                    (Value::from("lhs"), Value::from(spec.lhs)),
                    (Value::from("verb"), Value::from(spec.verb)),
                ])
            })
            .collect();
        let entries = default_maps()
            .iter()
            .map(|spec| {
                Value::Map(vec![
                    (Value::from("feature"), Value::from(spec.feature)),
                    (Value::from("verb"), Value::from(spec.verb)),
                ])
            })
            .collect();
        self.request_mappings(
            "nvim_exec_lua",
            vec![
                Value::from(REGISTER_MAPPINGS_CHUNK),
                Value::Array(vec![
                    Value::from(channel_id),
                    Value::Array(specs),
                    Value::Array(entries),
                    Value::from(COMMAND),
                ]),
            ],
        )
    }

    /// Issues [`BUFFER_LIST_CHUNK`] as an async request tagged with
    /// `generation`, resolving `Source::Buffers`'s corpus. Async by
    /// construction, like [`probe_default_hl`](Self::probe_default_hl): this
    /// issues the request through [`EngineHandle::request_buffer_list`] and
    /// returns immediately; the list crosses back as `Msg::PickerBufferList`
    /// through the connection's pump. See
    /// `docs/picker-buffer-list-wire-capture.md` for the reply shapes
    /// `crate::handle`'s `decode_buffer_list_reply` decodes.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited.
    pub fn list_buffers(&self, generation: u64) -> Result<(), EngineError> {
        self.request_buffer_list(
            "nvim_exec_lua",
            vec![Value::from(BUFFER_LIST_CHUNK)],
            generation,
        )
    }

    /// Issues [`PREVIEW_CHUNK`] as an async request tagged with `generation`,
    /// resolving the picker preview pane's text for `path`. Async by
    /// construction, like [`list_buffers`](Self::list_buffers): this issues
    /// the request through [`EngineHandle::request_preview`] and returns
    /// immediately; the answer crosses back as `Msg::PickerPreviewReply`
    /// through the connection's pump. `path` also travels with the waiter
    /// (unlike `list_buffers`, whose reply needs no echo) so the eventual
    /// reply can name which candidate it answers, since the picker's
    /// selection may have moved on by the time it lands. See
    /// `docs/picker-preview-wire-capture.md` for the reply shapes
    /// `crate::handle`'s `decode_preview_reply` decodes.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited.
    pub fn preview_buffer(&self, path: &str, generation: u64) -> Result<(), EngineError> {
        self.request_preview(
            "nvim_exec_lua",
            vec![
                Value::from(PREVIEW_CHUNK),
                Value::Array(vec![Value::from(path)]),
            ],
            generation,
            path.to_owned(),
        )
    }

    /// Issues [`LOAD_HIDDEN_CHUNK`] as an async request tagged with
    /// `generation`, resolving `path` to the buffer handle a diff review (or
    /// any other holder) attaches to and writes into -- creating an
    /// unlisted, file-backed hidden buffer for it when nothing already
    /// holds it, and always taking one hold on the path's refcount
    /// regardless of which branch resolved it.
    ///
    /// The hold is recorded before the request is even sent, keyed by
    /// [`canonical_hidden_key`] -- not after a reply decodes a handle, and
    /// not keyed by the raw `path` string. Taking it here, synchronously, on
    /// whichever thread called this method, is what keeps a
    /// [`release_hidden`](Self::release_hidden) call that overtakes this
    /// call's own reply from finding nothing to decrement: the entry already
    /// exists the instant this method returns, so that release always has a
    /// count to bring to zero regardless of reply timing, and the reader
    /// thread finishes resolving the hold's buffer whenever the reply
    /// eventually lands, attempting the delete itself if the count already
    /// reached zero by then (see `EngineHandle::hidden_bufs`'s own doc).
    /// Canonicalizing the key the same way nvim's own `canon()` does is what
    /// lets two different spellings of the same path share this one entry
    /// instead of racing each other's cleanup.
    ///
    /// If the request itself never reaches the wire (an `Err` below), the
    /// hold taken above is reversed immediately rather than left to leak: no
    /// reply is ever coming to resolve an entry for a request nvim never
    /// received, so nothing else will ever clean it up.
    ///
    /// Async by construction, like [`list_buffers`](Self::list_buffers):
    /// this issues the request through
    /// [`EngineHandle::request_load_hidden`] and returns immediately; the
    /// handle crosses back as `Msg::HiddenBufferLoaded` through the
    /// connection's pump.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited, or
    /// `EngineError::UnusablePath` for a spelling
    /// [`hidden_path_refusal`] refuses -- raised before any hold is taken
    /// and before anything reaches the wire, so the connection is untouched
    /// and the caller owes no [`release_hidden`](Self::release_hidden) for
    /// it (that call refuses the identical spelling as a no-op regardless).
    pub fn load_hidden(&self, path: &str, generation: u64) -> Result<(), EngineError> {
        if let Some(reason) = hidden_path_refusal(path) {
            return Err(EngineError::UnusablePath {
                path: path.to_owned(),
                reason,
            });
        }
        let key = canonical_hidden_key(path);
        self.note_hidden_acquire(&key);
        let sent = self.request_load_hidden(
            "nvim_exec_lua",
            vec![
                Value::from(LOAD_HIDDEN_CHUNK),
                Value::Array(vec![Value::from(path)]),
            ],
            generation,
            key.clone(),
        );
        if sent.is_err() {
            self.note_hidden_acquire_failed(&key);
        }
        sent
    }

    /// Releases one hold this connection's [`load_hidden`](Self::load_hidden)
    /// acquired for `path`: decrements `path`'s in-flight holder count and,
    /// only if that decrement brings it to zero and the buffer it resolved
    /// to is already known, issues [`RELEASE_HIDDEN_CHUNK`] for it -- and
    /// only when this connection created that buffer itself, never for one
    /// an earlier `load_hidden` call found already open (a real window's own
    /// buffer, or another connection's). No reply is awaited or needed -- a
    /// decrement-to-zero delete the chunk skips or nvim itself refuses (an
    /// unsaved edit -- see `docs/hidden-buffer-wire-capture.md`) is not an
    /// error this call surfaces: the hold is released either way, and the
    /// buffer simply outlives this release rather than losing content, or a
    /// window's own display, nobody asked to discard.
    ///
    /// `path` is canonicalized through the same [`canonical_hidden_key`]
    /// [`load_hidden`](Self::load_hidden) keyed its hold with, so a release
    /// spelled differently than its matching load still finds the same
    /// entry.
    ///
    /// A no-op for a `path` with no recorded hold (nothing to decrement,
    /// nothing to delete) -- every caller owes exactly one of these per
    /// `load_hidden` call, but a defensive extra call must never panic or
    /// delete a buffer this connection never created. Also a no-op, without
    /// deleting anything yet, when this decrement brings the count to zero
    /// but the matching `load_hidden` call's reply has not landed yet: the
    /// entry is left in place for the reader thread to finish once it does.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited, only when a decrement-to-zero
    /// delete was actually attempted -- a decrement that leaves the count
    /// above zero, a `path` with no hold at all, or one whose buffer is not
    /// yet known, never touches the wire and always answers `Ok(())`.
    pub fn release_hidden(&self, path: &str) -> Result<(), EngineError> {
        // A spelling load_hidden refuses never took a hold, so there is
        // nothing here to decrement and nothing this connection could own.
        // Answered as the same no-op an unheld path already gets rather
        // than as an error: a review's close still owes exactly one of
        // these per bind, whether or not its bind was ever accepted, and a
        // failed release would read as a lost engine to the caller.
        if hidden_path_refusal(path).is_some() {
            return Ok(());
        }
        let key = canonical_hidden_key(path);
        let Some(buf) = self.note_hidden_release(&key) else {
            return Ok(());
        };
        self.notify(
            "nvim_exec_lua",
            vec![
                Value::from(RELEASE_HIDDEN_CHUNK),
                Value::Array(vec![Value::from(buf.0)]),
            ],
        )
    }

    /// Opens `path` via [`OPEN_FILE_CHUNK`], reusing an already-loaded
    /// buffer for it the same way `:edit` would rather than duplicating it.
    /// Fire-and-forget: the tree overlay that issued this closes on the
    /// same keypress, so nothing waits on a reply.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited.
    pub fn open_file(&self, path: &str) -> Result<(), EngineError> {
        self.notify(
            "nvim_exec_lua",
            vec![
                Value::from(OPEN_FILE_CHUNK),
                Value::Array(vec![Value::from(path)]),
            ],
        )
    }

    /// Issues [`RENAME_CHUNK`] as an async request tagged with `generation`,
    /// renaming `old_path` to `new_path` and retargeting any open buffer
    /// along with it. Async by construction, like
    /// [`preview_buffer`](Self::preview_buffer): this issues the request
    /// through [`EngineHandle::request_rename`] and returns immediately; the
    /// answer crosses back as `Msg::TreeRenameReply` through the
    /// connection's pump. See `docs/tree-rename-wire-capture.md` for the
    /// reply shape `crate::handle`'s `decode_rename_reply` decodes.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited.
    pub fn rename_file(
        &self,
        old_path: &str,
        new_path: &str,
        generation: u64,
    ) -> Result<(), EngineError> {
        self.request_rename(
            "nvim_exec_lua",
            vec![
                Value::from(RENAME_CHUNK),
                Value::Array(vec![Value::from(old_path), Value::from(new_path)]),
            ],
            generation,
        )
    }

    /// Issues [`TREE_INPUT_PROMPT_CHUNK`] with an empty `default`, as an
    /// async request tagged with `generation`, asking for the name of a new
    /// entry to create beneath the tree's current directory. Async by
    /// construction, like [`rename_file`](Self::rename_file): the typed name
    /// (or `None` for a cancelled prompt) crosses back as
    /// `Msg::TreeCreatePromptReply` through the connection's pump. See
    /// `docs/tree-input-prompt-wire-capture.md` for the reply shape
    /// `crate::handle`'s `decode_prompt_reply` decodes.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited.
    pub fn tree_create_prompt(&self, generation: u64) -> Result<(), EngineError> {
        self.request_create_prompt(
            "nvim_exec_lua",
            vec![
                Value::from(TREE_INPUT_PROMPT_CHUNK),
                Value::Array(vec![Value::from("New file: "), Value::from("")]),
            ],
            generation,
        )
    }

    /// Issues [`TREE_INPUT_PROMPT_CHUNK`] with `current_name` as `default`,
    /// as an async request tagged with `generation`, asking for the tree's
    /// selected entry's new name. Async by construction, like
    /// [`tree_create_prompt`](Self::tree_create_prompt): the typed name (or
    /// `None` for a cancelled prompt) crosses back as
    /// `Msg::TreeRenamePromptReply` through the connection's pump, alongside
    /// the `old_path` the reply answers for. See
    /// `docs/tree-input-prompt-wire-capture.md` for the reply shape
    /// `crate::handle`'s `decode_prompt_reply` decodes.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited.
    pub fn tree_rename_prompt(
        &self,
        old_path: &str,
        current_name: &str,
        generation: u64,
    ) -> Result<(), EngineError> {
        self.request_rename_prompt(
            "nvim_exec_lua",
            vec![
                Value::from(TREE_INPUT_PROMPT_CHUNK),
                Value::Array(vec![Value::from("Rename: "), Value::from(current_name)]),
            ],
            generation,
            old_path.to_owned(),
        )
    }

    /// Issues [`TREE_DELETE_CONFIRM_CHUNK`] as an async request tagged with
    /// `generation`, asking the user to confirm deleting `path`. Async by
    /// construction, like [`tree_rename_prompt`](Self::tree_rename_prompt):
    /// the choice crosses back as `Msg::TreeDeleteConfirmReply` through the
    /// connection's pump, alongside the `path` the reply answers for. See
    /// `docs/tree-input-prompt-wire-capture.md` for the reply shape
    /// `crate::handle`'s `decode_delete_confirm_reply` decodes.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited.
    pub fn tree_delete_confirm(&self, path: &str, generation: u64) -> Result<(), EngineError> {
        self.request_delete_confirm(
            "nvim_exec_lua",
            vec![
                Value::from(TREE_DELETE_CONFIRM_CHUNK),
                Value::Array(vec![
                    Value::from(format!("Delete {path}?")),
                    Value::from(path),
                ]),
            ],
            generation,
            path.to_owned(),
        )
    }

    /// Applies `edits` to `buf` via [`BUF_SET_TEXT_CHUNK`], the only path
    /// that ever writes agent-proposed text (hard rule: nvim owns all buffer
    /// text -- see [`view_core::msg::RpcCall::BufSetText`]'s own doc for the
    /// per-hunk undo contract `undojoin` implements).
    ///
    /// `edits` is applied in descending `(start_row, start_col)` order --
    /// bottom of the buffer first -- regardless of the order the caller
    /// listed them in. `edits` must be non-overlapping (see [`TextEdit`]'s
    /// own doc); given that, applying bottom-to-top is what makes the batch
    /// order-insensitive: an edit lower in the buffer never shifts the
    /// still-pending row/col coordinates of one above it, whereas the
    /// reverse order would (an earlier top edit that grows or shrinks a line
    /// changes every column after it on that line, stale-ing any later edit
    /// still addressing the original coordinates).
    ///
    /// A request, not a notify, deliberately: a stale `buf` (closed between
    /// an agent's proposal and the user's accept) must surface as this
    /// call's `Err` rather than a silently dropped edit -- `notify` has no
    /// reply to carry that on. See [`BUF_SET_TEXT_CHUNK`]'s own doc for the
    /// live-captured error shape a stale handle produces.
    ///
    /// # Errors
    ///
    /// Returns the `EngineError` from the underlying request if it fails,
    /// nvim rejects the edit (including `buf` no longer existing), or the
    /// reply does not arrive within [`BUF_SET_TEXT_TIMEOUT`].
    pub fn set_buf_text(
        &self,
        buf: BufferHandle,
        edits: &[TextEdit],
        undojoin: bool,
        expected_changedtick: Option<u64>,
    ) -> Result<BufWriteOutcome, EngineError> {
        let mut ordered: Vec<&TextEdit> = edits.iter().collect();
        ordered.sort_by_key(|edit| std::cmp::Reverse((edit.start_row, edit.start_col)));
        let edits = ordered
            .iter()
            .map(|edit| {
                Value::Map(vec![
                    (Value::from("start_row"), Value::from(edit.start_row)),
                    (Value::from("start_col"), Value::from(edit.start_col)),
                    (Value::from("end_row"), Value::from(edit.end_row)),
                    (Value::from("end_col"), Value::from(edit.end_col)),
                    (
                        Value::from("lines"),
                        Value::Array(
                            edit.lines
                                .iter()
                                .map(|line| Value::from(line.as_str()))
                                .collect(),
                        ),
                    ),
                ])
            })
            .collect();
        let reply = self.request_timeout(
            "nvim_exec_lua",
            vec![
                Value::from(BUF_SET_TEXT_CHUNK),
                Value::Array(vec![
                    Value::from(buf.0),
                    Value::from(undojoin),
                    expected_changedtick.map_or(Value::Nil, Value::from),
                    Value::Array(edits),
                ]),
            ],
            BUF_SET_TEXT_TIMEOUT,
        )?;
        Ok(decode_buf_set_text_reply(&reply))
    }

    /// Subscribes to `buf`'s live edit stream via `nvim_buf_attach(buf,
    /// false, {})`, for `RpcCall::BufAttach`. `send_buffer: false` is
    /// load-bearing, confirmed live in
    /// `docs/nvim-buf-attach-wire-capture.md` capture #1: it is what keeps
    /// the attach itself from streaming the whole buffer as an initial
    /// `nvim_buf_lines_event`, so this connection's event volume for `buf`
    /// stays proportional to the edits that follow, never to the buffer's
    /// size at the moment of attach.
    ///
    /// A notify, not a request: nothing here blocks on nvim's boolean
    /// success reply, matching every other fire-and-forget call in this
    /// crate. `generation` is recorded locally
    /// ([`EngineHandle::note_buf_attach`]) only after this notify itself
    /// succeeds, not once nvim's own boolean reply could confirm the attach
    /// -- there is still nothing to gain by waiting on that reply (a
    /// rejected attach for a stale `buf` simply means no
    /// `nvim_buf_lines_event` for it ever arrives to look the entry up), but
    /// a notify that fails outright (`EngineError::Closed`) must not record
    /// an entry either: with the writer thread already gone, nothing will
    /// ever detach it.
    ///
    /// `buf` must already be the buffer's real, resolved handle -- never
    /// `0` ("current buffer"). Capture #1 in the wire-capture doc attaches
    /// buffer `0` and shows every resulting `nvim_buf_lines_event` naming
    /// it `Ext(0,[1])` (the real number, 1), so `generation` recorded under
    /// the sentinel `0` would sit under a key no event this attach produces
    /// is ever looked up by; the caller is responsible for resolving the
    /// handle before calling this (e.g. from a prior `ListBuffers` reply or
    /// `nvim_get_current_buf`), the same way `RpcCall::BufSetText`'s own
    /// `buf` is already a resolved handle by the time it reaches this
    /// layer.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn buf_attach(&self, buf: BufferHandle, generation: u64) -> Result<(), EngineError> {
        self.notify(
            "nvim_buf_attach",
            vec![
                Value::from(buf.0),
                Value::from(false),
                Value::Map(Vec::new()),
            ],
        )?;
        self.note_buf_attach(buf.0, generation);
        Ok(())
    }

    /// Unsubscribes from `buf`'s edit stream via `nvim_buf_detach`, for
    /// `RpcCall::BufDetach`. Removes the locally recorded generation
    /// ([`EngineHandle::note_buf_detach`]) before the notify is even sent,
    /// so a `nvim_buf_lines_event` already in flight from before this call
    /// finds no generation to stamp and is dropped rather than reaching a
    /// hunk-rebase state machine the user already dismissed -- live-verified
    /// in `docs/nvim-buf-attach-wire-capture.md` capture #4: no further
    /// event arrives for a detached buffer regardless.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn buf_detach(&self, buf: BufferHandle) -> Result<(), EngineError> {
        self.note_buf_detach(buf.0);
        self.notify("nvim_buf_detach", vec![Value::from(buf.0)])
    }

    /// Reads the current buffer's path and nvim-authoritative text via
    /// [`CURRENT_BUFFER_TEXT_CHUNK`], for `RpcCall::ReadCurrentBufferText`.
    ///
    /// # Errors
    ///
    /// Returns the `EngineError` from the underlying request if it fails,
    /// the reply does not arrive within [`CONTEXT_READ_TIMEOUT`], or the
    /// reply is not the documented map shape (surfaced as
    /// `EngineError::Rpc(RpcError::Malformed(_))`, the same convention
    /// [`EngineHandle::get_mode`] uses).
    pub fn read_current_buffer_text(&self) -> Result<CurrentBufferRead, EngineError> {
        let value = self.request_timeout(
            "nvim_exec_lua",
            vec![Value::from(CURRENT_BUFFER_TEXT_CHUNK), Value::Array(vec![])],
            CONTEXT_READ_TIMEOUT,
        )?;
        decode_current_buffer_text_reply(&value)
    }

    /// Reads the buffer-space cursor and, when one is active, the visual
    /// selection via [`CURSOR_CONTEXT_CHUNK`], for `RpcCall::ReadCursorContext`.
    ///
    /// # Errors
    ///
    /// Same terms as [`read_current_buffer_text`](Self::read_current_buffer_text).
    pub fn read_cursor_context(&self) -> Result<(CursorRead, Option<SelectionRead>), EngineError> {
        let value = self.request_timeout(
            "nvim_exec_lua",
            vec![Value::from(CURSOR_CONTEXT_CHUNK), Value::Array(vec![])],
            CONTEXT_READ_TIMEOUT,
        )?;
        decode_cursor_context_reply(&value)
    }

    /// Reads every current entry from `vim.diagnostic.get(0)` via
    /// [`DIAGNOSTIC_ENTRIES_CHUNK`], for `RpcCall::ReadDiagnosticEntries`.
    ///
    /// # Errors
    ///
    /// Same terms as [`read_current_buffer_text`](Self::read_current_buffer_text).
    pub fn read_diagnostic_entries(&self) -> Result<Vec<DiagnosticEntry>, EngineError> {
        let value = self.request_timeout(
            "nvim_exec_lua",
            vec![Value::from(DIAGNOSTIC_ENTRIES_CHUNK), Value::Array(vec![])],
            CONTEXT_READ_TIMEOUT,
        )?;
        decode_diagnostic_entries_reply(&value)
    }

    /// Reads every current entry from `getqflist()` via
    /// [`QUICKFIX_ENTRIES_CHUNK`], for `RpcCall::ReadQuickfixEntries`.
    ///
    /// # Errors
    ///
    /// Same terms as [`read_current_buffer_text`](Self::read_current_buffer_text).
    pub fn read_quickfix_entries(&self) -> Result<Vec<QuickfixEntry>, EngineError> {
        let value = self.request_timeout(
            "nvim_exec_lua",
            vec![Value::from(QUICKFIX_ENTRIES_CHUNK), Value::Array(vec![])],
            CONTEXT_READ_TIMEOUT,
        )?;
        decode_quickfix_entries_reply(&value)
    }
}

/// Maps one [`OptionValue`] onto the msgpack value nvim's option API takes.
///
/// Total by construction, and deliberately so: `OptionValue` is closed over
/// nvim's three option types, so a new variant must break this match rather
/// than fall through to a default that would set an option to something
/// nvim never asked for.
fn option_value(value: &OptionValue) -> Value {
    match value {
        OptionValue::Int(n) => Value::from(*n),
        OptionValue::Bool(b) => Value::from(*b),
        OptionValue::Str(s) => Value::from(s.as_str()),
    }
}

/// Renders an `nvim_eval` result as plain text for [`EngineHandle::eval_str`].
///
/// `Value`'s own `Display` impl is unsuitable: `rmpv::Utf8String::fmt`
/// formats through `Debug`, so a vimscript string result like `getline(1)`'s
/// `"hello"` would round-trip as the quoted literal `"\"hello\""` rather
/// than the bare `hello` a text-comparison oracle needs (`s.as_str()`
/// returning `None`, an ill-formed UTF-8 string on the wire, falls back to
/// a lossy conversion rather than silently dropping the reply). `Array`/
/// `Map`/`Binary`/`Ext` results (no probe this crate exposes evaluates to
/// one today) fall through to `Value`'s own `Display` rendering, which is
/// still total -- just not this function's primary concern.
fn value_to_string(value: &Value) -> String {
    match value {
        Value::Nil => "nil".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::String(s) => s.as_str().map_or_else(
            || String::from_utf8_lossy(s.as_bytes()).into_owned(),
            str::to_string,
        ),
        Value::Integer(i) => i.to_string(),
        Value::F32(f) => f.to_string(),
        Value::F64(f) => f.to_string(),
        other => other.to_string(),
    }
}

/// Decodes [`CURRENT_BUFFER_TEXT_CHUNK`]'s `{path, text}` reply, live-
/// verified against a real `nvim --clean --headless` (see
/// `docs/ai-context-reads-wire-capture.md`). Unlike `decode_preview_reply`'s
/// "absent or malformed degrades to a safe default" convention, a malformed
/// reply here surfaces as `Err` rather than an empty `CurrentBufferRead`:
/// the chunk's own two keys are unconditional (nvim always has a current
/// buffer, even an unnamed scratch one), so a shape missing either is a
/// contract violation this crate has never actually seen from the pinned
/// engine, not an expected "nothing to read" case.
fn decode_current_buffer_text_reply(result: &Value) -> Result<CurrentBufferRead, EngineError> {
    let malformed = || {
        EngineError::Rpc(RpcError::Malformed(format!(
            "current-buffer-text reply: {result}"
        )))
    };
    let pairs = result.as_map().ok_or_else(malformed)?;
    let path = crate::wire::map_find(pairs, "path")
        .and_then(Value::as_str)
        .ok_or_else(malformed)?;
    let text = crate::wire::map_find(pairs, "text")
        .and_then(Value::as_str)
        .ok_or_else(malformed)?;
    Ok(CurrentBufferRead::new(PathBuf::from(path), text.to_owned()))
}

/// Decodes [`CURSOR_CONTEXT_CHUNK`]'s `{line, col, selection_*}` reply,
/// live-verified against a real `nvim --clean --headless` (see
/// `docs/ai-context-reads-wire-capture.md`). `line`/`col` are unconditional
/// (nvim always has a cursor) and a shape missing either is malformed, the
/// same contract-violation reasoning
/// [`decode_current_buffer_text_reply`] documents. `col` crosses the wire
/// 0-indexed (`nvim_win_get_cursor`'s own convention); this decoder adds 1
/// so [`CursorRead::col`] carries the single 1-indexed convention every
/// `EngineReadSnapshot` position field shares (see that type's own doc).
/// `line` needs no such adjustment: `nvim_win_get_cursor`'s row is already
/// 1-indexed on the wire. The three `selection_*` keys are read together or
/// not at all: the chunk only ever writes all three or none, so a reply
/// carrying just one or two is treated as no active selection rather than a
/// partial one built from whichever keys happened to be present.
/// `selection_start`/`selection_end` need no adjustment either: they are
/// buffer line numbers, already 1-indexed the same way `line` is.
fn decode_cursor_context_reply(
    result: &Value,
) -> Result<(CursorRead, Option<SelectionRead>), EngineError> {
    let malformed = || {
        EngineError::Rpc(RpcError::Malformed(format!(
            "cursor-context reply: {result}"
        )))
    };
    let pairs = result.as_map().ok_or_else(malformed)?;
    let line = crate::wire::map_find(pairs, "line")
        .and_then(Value::as_u64)
        .ok_or_else(malformed)?;
    let col = crate::wire::map_find(pairs, "col")
        .and_then(Value::as_u64)
        .ok_or_else(malformed)?;
    let cursor = CursorRead::new(saturate_u32(line), saturate_u32(col).saturating_add(1));
    let selection = match (
        crate::wire::map_find(pairs, "selection_text").and_then(Value::as_str),
        crate::wire::map_find(pairs, "selection_start").and_then(Value::as_u64),
        crate::wire::map_find(pairs, "selection_end").and_then(Value::as_u64),
    ) {
        (Some(text), Some(start), Some(end)) => Some(SelectionRead::new(
            text.to_owned(),
            (saturate_u32(start), saturate_u32(end)),
        )),
        _ => None,
    };
    Ok((cursor, selection))
}

/// Decodes [`DIAGNOSTIC_ENTRIES_CHUNK`]'s reply, live-verified against a
/// real `nvim --clean --headless` (see
/// `docs/ai-context-reads-wire-capture.md`). A non-array `result` (a shape
/// this crate has never actually seen from the pinned engine, since the
/// chunk always returns a table) degrades to an empty list rather than an
/// `Err`, matching `decode_buffer_list_reply`'s convention for a corpus that
/// legitimately can be empty (no diagnostics currently posted) -- a row
/// missing any of its four fields is dropped rather than failing the whole
/// read. `line`/`col` cross the wire 0-indexed (`vim.diagnostic.get`'s own
/// convention); both get +1 here so [`DiagnosticEntry::line`]/`::col` carry
/// the same single 1-indexed convention [`decode_cursor_context_reply`]
/// normalizes onto (see `EngineReadSnapshot`'s own doc) -- `getqflist`'s
/// entries need no such adjustment, already 1-indexed on the wire (see
/// [`decode_quickfix_entries_reply`]). `severity` is
/// `vim.diagnostic.severity`'s own closed 1-4 range (`:help
/// diagnostic-severity`); an out-of-range value this crate has never seen
/// from the pinned engine drops the row rather than guessing a severity
/// nvim never reported.
fn decode_diagnostic_entries_reply(result: &Value) -> Result<Vec<DiagnosticEntry>, EngineError> {
    let Some(rows) = result.as_array() else {
        return Ok(Vec::new());
    };
    let entries = rows
        .iter()
        .filter_map(|row| {
            let pairs = row.as_map()?;
            let line =
                saturate_u32(crate::wire::map_find(pairs, "line")?.as_u64()?).saturating_add(1);
            let col =
                saturate_u32(crate::wire::map_find(pairs, "col")?.as_u64()?).saturating_add(1);
            let severity = match crate::wire::map_find(pairs, "severity")?.as_u64()? {
                1 => DiagnosticSeverity::Error,
                2 => DiagnosticSeverity::Warning,
                3 => DiagnosticSeverity::Info,
                4 => DiagnosticSeverity::Hint,
                _ => return None,
            };
            let message = crate::wire::map_find(pairs, "message")?
                .as_str()?
                .to_owned();
            Some(DiagnosticEntry::new(line, col, severity, message))
        })
        .collect();
    Ok(entries)
}

/// Decodes [`QUICKFIX_ENTRIES_CHUNK`]'s reply, live-verified against a real
/// `nvim --clean --headless` (see `docs/ai-context-reads-wire-capture.md`),
/// on the same "non-array degrades to empty, a malformed row is dropped"
/// terms as [`decode_diagnostic_entries_reply`] -- an empty quickfix list is
/// the ordinary case, not an error. `line`/`col` need no index adjustment
/// here, unlike that decoder's: `getqflist()` is already 1-indexed on the
/// wire, the same convention every `EngineReadSnapshot` position field
/// shares.
fn decode_quickfix_entries_reply(result: &Value) -> Result<Vec<QuickfixEntry>, EngineError> {
    let Some(rows) = result.as_array() else {
        return Ok(Vec::new());
    };
    let entries = rows
        .iter()
        .filter_map(|row| {
            let pairs = row.as_map()?;
            let path = crate::wire::map_find(pairs, "path")?.as_str()?.to_owned();
            let line = saturate_u32(crate::wire::map_find(pairs, "line")?.as_u64()?);
            let col = saturate_u32(crate::wire::map_find(pairs, "col")?.as_u64()?);
            let text = crate::wire::map_find(pairs, "text")?.as_str()?.to_owned();
            Some(QuickfixEntry::new(PathBuf::from(path), line, col, text))
        })
        .collect();
    Ok(entries)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::rpc::RpcMessage;
    use std::io::{BufReader, Write};

    /// A minimal fake peer that answers every incoming request with
    /// `result` and forwards `(method, params)` to the returned channel, so
    /// a test can assert on the exact wire shape a typed wrapper sends
    /// without a real nvim. Pass `Value::Nil` for tests that only care
    /// about the outgoing request shape, not the reply.
    ///
    /// Notifications are captured through the same channel: a
    /// fire-and-forget wrapper puts exactly as much on the wire as a
    /// blocking one does, and its shape is exactly as easy to get wrong.
    fn fake_peer_replying_with(
        result: Value,
    ) -> (
        EngineHandle,
        std::sync::mpsc::Receiver<(String, Vec<Value>)>,
    ) {
        let (peer_read, our_write) = std::io::pipe().unwrap();
        let (our_read, mut peer_write) = std::io::pipe().unwrap();
        let (cap_tx, cap_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(peer_read);
            while let Ok(v) = rmpv::decode::read_value(&mut r) {
                match RpcMessage::from_value(v) {
                    Ok(RpcMessage::Request {
                        msgid,
                        method,
                        params,
                    }) => {
                        let _ = cap_tx.send((method, params));
                        let resp = RpcMessage::Response {
                            msgid,
                            error: Value::Nil,
                            result: result.clone(),
                        };
                        if rmpv::encode::write_value(&mut peer_write, &resp.to_value()).is_err() {
                            break;
                        }
                        if peer_write.flush().is_err() {
                            break;
                        }
                    }
                    Ok(RpcMessage::Notification { method, params }) => {
                        let _ = cap_tx.send((method, params));
                    }
                    _ => {}
                }
            }
        });
        let (h, _notif_rx) = EngineHandle::start(our_read, our_write);
        (h, cap_rx)
    }

    /// Pins the exact vimscript shape live-verified against a real `nvim
    /// --clean --embed`: `++once` (self-clearing, never fires twice), plain
    /// `rpcrequest` (not `rpcnotify` -- the spec mandates blocking here),
    /// targeting `channel_id` explicitly (nvim has no loopback shorthand).
    #[test]
    fn register_vim_enter_autocmd_sends_the_exact_verified_vimscript_shape() {
        let (h, cap_rx) = fake_peer_replying_with(Value::Nil);
        h.register_vim_enter_autocmd(7).unwrap();
        let (method, params) = cap_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(method, "nvim_command");
        assert_eq!(
            params,
            vec![Value::from(
                "autocmd VimEnter * ++once call rpcrequest(7, 'view_vim_enter')"
            )]
        );
    }

    /// The registration crosses as one chunk with its data as arguments,
    /// never interpolated, and the chunk snapshots the existing mappings
    /// before it sets the first key: the order is the whole claim report's
    /// source of truth, and setting first would answer every key with view's
    /// own mapping.
    #[test]
    fn register_mappings_sends_one_chunk_carrying_its_data_as_arguments() {
        let (h, cap_rx) = fake_peer_replying_with(Value::Nil);
        h.register_mappings(default_maps(), 7).unwrap();
        let (method, params) = cap_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(method, "nvim_exec_lua");
        assert_eq!(params[0], Value::from(REGISTER_MAPPINGS_CHUNK));
        let args = params[1]
            .as_array()
            .expect("the chunk's arguments must cross as an array");
        assert_eq!(args[0], Value::from(7));
        assert_eq!(args[3], Value::from(COMMAND));
        let specs = args[1]
            .as_array()
            .expect("the specs must cross as an array");
        let entries = args[2]
            .as_array()
            .expect("the completion entries must cross as an array");
        assert_eq!(specs.len(), default_maps().len());
        assert_eq!(
            entries.len(),
            default_maps().len(),
            "the command completes every entry point this build has, whatever this session mapped"
        );
        let set = REGISTER_MAPPINGS_CHUNK
            .find("vim.keymap.set")
            .expect("the chunk must set the mapping");
        for source in ["nvim_get_keymap", "nvim_buf_get_keymap"] {
            let read = REGISTER_MAPPINGS_CHUNK
                .find(source)
                .unwrap_or_else(|| unreachable!("the chunk must consult {source}"));
            assert!(
                read < set,
                "{source} must be read before the first key is set"
            );
        }
    }

    /// The channel id crosses as an argument, not interpolated, and the
    /// method is a notification: a `request` here would put a blocking call
    /// on the surface the paint loop drives.
    #[test]
    fn register_bridge_sends_one_chunk_carrying_the_channel_as_an_argument() {
        let (h, cap_rx) = fake_peer_replying_with(Value::Nil);
        h.register_bridge(7).unwrap();
        let (method, params) = cap_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(method, "nvim_exec_lua");
        assert_eq!(params[0], Value::from(REGISTER_BRIDGE_CHUNK));
        assert_eq!(
            params[1],
            Value::Array(vec![Value::from(7)]),
            "the channel must cross as the chunk's only argument"
        );
        assert!(
            !REGISTER_BRIDGE_CHUNK.contains('7'),
            "the chunk must be constant: nothing about the caller may appear in its source"
        );
    }

    /// Every trigger the bridge exists to carry lives in the one group, and
    /// that group clears itself: a consumer added later must not need its own
    /// registration, and re-registering after an engine restart must not
    /// stack a second copy of every autocommand.
    #[test]
    fn the_bridge_chunk_hooks_every_trigger_in_one_self_clearing_group() {
        for event in [
            "'ColorScheme'",
            "'DiagnosticChanged'",
            "'BufEnter'",
            "'DirChanged'",
            "'FocusGained'",
            "'BufFilePost'",
            "'BufWritePost'",
            "'BufModifiedSet'",
        ] {
            assert!(
                REGISTER_BRIDGE_CHUNK.contains(event),
                "the bridge must hook {event}"
            );
        }
        assert_eq!(
            REGISTER_BRIDGE_CHUNK.matches("nvim_create_augroup").count(),
            1,
            "one group, or a restart can lose one registration and leave the others working"
        );
        assert!(REGISTER_BRIDGE_CHUNK.contains("'view_bridge', { clear = true }"));
        assert_eq!(
            REGISTER_BRIDGE_CHUNK
                .matches("vim.rpcnotify(channel, 'view_bridge'")
                .count(),
            4,
            "colorscheme through the shared relay, plus diagnostics/git/buffer each sending \
             their own richer payload instead of a bare match"
        );
    }

    /// The statusline's diagnostics, git, and buffer triggers each compute a
    /// real payload rather than forwarding `args.match` -- see
    /// [`REGISTER_BRIDGE_CHUNK`]'s doc for why none of the three blocks
    /// nvim's main loop by doing so.
    #[test]
    fn the_bridge_chunk_computes_richer_payloads_for_the_statusline_triggers() {
        assert!(
            REGISTER_BRIDGE_CHUNK.contains("vim.diagnostic.count(0)"),
            "diagnostics must read real counts, not forward a bare match"
        );
        assert!(
            REGISTER_BRIDGE_CHUNK.contains("vim.system("),
            "the git lookup must run asynchronously, off nvim's main loop"
        );
        assert!(
            REGISTER_BRIDGE_CHUNK.contains("vim.fn.expand('%:t')")
                && REGISTER_BRIDGE_CHUNK.contains("vim.bo.modified"),
            "the buffer trigger must carry the current file's name and modified flag"
        );
    }

    /// This method takes any `&[MappingSpec]`, and the generated right-hand
    /// side spells the feature and verb inside a quoted vimscript call. A
    /// spec that could close that call must never reach the chunk, whatever
    /// the shipped table happens to contain.
    #[test]
    fn a_spec_that_could_break_out_of_the_generated_mapping_never_reaches_the_chunk() {
        let (h, cap_rx) = fake_peer_replying_with(Value::Nil);
        let hostile = [
            MappingSpec {
                feature: "picker",
                lhs: "<leader>ff",
                verb: "files', 'x')|call system('id",
            },
            MappingSpec {
                feature: "picker",
                lhs: "<leader>fb",
                verb: "buffers",
            },
        ];
        h.register_mappings(&hostile, 7).unwrap();
        let (_, params) = cap_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let args = params[1].as_array().expect("the arguments array");
        let specs = args[1].as_array().expect("the specs array");
        assert_eq!(
            specs.len(),
            1,
            "only the spellable spec may cross, got {specs:?}"
        );
        let rendered = format!("{specs:?}");
        assert!(
            !rendered.contains("system("),
            "the hostile verb reached the chunk: {rendered}"
        );
        assert!(rendered.contains("buffers"), "{rendered}");
    }

    #[test]
    fn ui_attach_sends_the_full_ext_set() {
        let (h, cap_rx) = fake_peer_replying_with(Value::Nil);
        h.ui_attach(80, 24).unwrap();
        let (method, params) = cap_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(method, "nvim_ui_attach");
        assert_eq!(params[0], Value::from(80));
        assert_eq!(params[1], Value::from(24));
        let Value::Map(opts) = &params[2] else {
            unreachable!("expected an options map, got {:?}", params[2]);
        };
        for ext in [
            "ext_linegrid",
            "ext_cmdline",
            "ext_popupmenu",
            "ext_messages",
            "ext_tabline",
        ] {
            assert!(
                opts.iter()
                    .any(|(k, v)| k.as_str() == Some(ext) && v.as_bool() == Some(true)),
                "missing or false {ext} in ui_attach options"
            );
        }
    }

    #[test]
    fn ui_attach_with_stdin_relay_adds_stdin_fd_over_the_same_ext_set() {
        let (h, cap_rx) = fake_peer_replying_with(Value::Nil);
        h.ui_attach_with_stdin_relay(80, 24).unwrap();
        let (method, params) = cap_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(method, "nvim_ui_attach");
        let Value::Map(opts) = &params[2] else {
            unreachable!("expected an options map, got {:?}", params[2]);
        };
        for ext in [
            "ext_linegrid",
            "ext_cmdline",
            "ext_popupmenu",
            "ext_messages",
            "ext_tabline",
        ] {
            assert!(
                opts.iter()
                    .any(|(k, v)| k.as_str() == Some(ext) && v.as_bool() == Some(true)),
                "missing or false {ext} in ui_attach_with_stdin_relay options"
            );
        }
        assert!(
            opts.iter().any(|(k, v)| k.as_str() == Some("stdin_fd")
                && v.as_i64() == Some(i64::from(STDIN_RELAY_CHILD_FD))),
            "stdin_fd must name the child descriptor build_command relays onto, got {opts:?}"
        );
    }

    #[test]
    fn eval_str_sends_the_expression_as_a_single_positional_string() {
        let (h, cap_rx) = fake_peer_replying_with(Value::Nil);
        let _ = h.eval_str("getline(1)");
        let (method, params) = cap_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(method, "nvim_eval");
        assert_eq!(params, vec![Value::from("getline(1)")]);
    }

    #[test]
    fn feed_keys_passes_the_notation_as_an_argument_not_as_code() {
        let (h, cap_rx) = fake_peer_replying_with(Value::Nil);
        // every character a quoted-into-a-command implementation would
        // have had to escape, in one notation
        let hostile = "<Cmd>call setline(1, 'a\\b')<CR>\nix\"y'z";
        let _ = h.feed_keys(hostile);
        let (method, params) = cap_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(method, "nvim_exec_lua");
        assert_eq!(params[0], Value::from(FEED_KEYS_CHUNK));
        assert_eq!(params[1], Value::Array(vec![Value::from(hostile)]));
        assert!(
            !FEED_KEYS_CHUNK.contains("setline"),
            "the chunk must stay constant, whatever the notation carries"
        );
    }

    #[test]
    fn set_option_sends_name_value_and_an_empty_scope_map() {
        let (h, cap_rx) = fake_peer_replying_with(Value::Nil);
        h.set_option("laststatus", &OptionValue::Int(0)).unwrap();
        let (method, params) = cap_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(method, "nvim_set_option_value");
        assert_eq!(
            params,
            vec![
                Value::from("laststatus"),
                Value::from(0),
                Value::Map(Vec::new()),
            ]
        );
    }

    #[test]
    fn hold_option_sends_the_constant_chunk_with_name_and_value_as_arguments() {
        let (h, cap_rx) = fake_peer_replying_with(Value::Nil);
        h.hold_option("laststatus", &OptionValue::Int(0)).unwrap();
        let (method, params) = cap_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(method, "nvim_exec_lua");
        assert_eq!(
            params,
            vec![
                Value::from(HOLD_OPTION_CHUNK),
                Value::Array(vec![Value::from("laststatus"), Value::from(0)]),
            ]
        );
    }

    #[test]
    fn the_hold_chunk_both_sets_the_option_and_guards_it() {
        // the halves are what make a takeover durable; a chunk that lost
        // any one of them would still pass the wire-shape test above
        assert!(HOLD_OPTION_CHUNK.contains("nvim_set_option_value(name, value, {})"));
        assert!(HOLD_OPTION_CHUNK.contains("'OptionSet'"));
        assert!(
            HOLD_OPTION_CHUNK.contains("'SafeState'"),
            "without the idle backstop the guard cannot see a write made inside \
             another autocommand, which is how a superseded plugin re-asserts"
        );
        assert!(
            HOLD_OPTION_CHUNK.contains("{ clear = true }"),
            "a re-applied plan must replace its guard rather than stack a second one"
        );
    }

    #[test]
    fn every_option_value_kind_maps_to_its_own_msgpack_type() {
        assert_eq!(option_value(&OptionValue::Int(3)), Value::from(3));
        assert_eq!(option_value(&OptionValue::Bool(true)), Value::from(true));
        assert_eq!(
            option_value(&OptionValue::Str("%f".to_string())),
            Value::from("%f")
        );
    }

    #[test]
    fn eval_str_renders_a_string_result_bare() {
        let (h, _cap_rx) = fake_peer_replying_with(Value::from("hello"));
        assert_eq!(h.eval_str("getline(1)").unwrap(), "hello");
    }

    #[test]
    fn eval_str_renders_an_integer_result_as_decimal() {
        let (h, _cap_rx) = fake_peer_replying_with(Value::from(42));
        assert_eq!(h.eval_str("line('.')").unwrap(), "42");
    }

    /// The divergent table `docs/hidden-buffer-wire-capture.md` case 15
    /// measured between `fnamemodify(p, ':p')` and `bufadd`'s own identity
    /// resolution: a nonexistent parent leaves the whole path untouched
    /// (`.`/`..` and all), and `/../a`'s parent (`/..`) trivially resolves
    /// to root regardless of whether `a` itself exists. None of these three
    /// go anywhere near a symlink -- `nvim_style_absolute`'s symlink half is
    /// exercised live in `hidden_buffer_live.rs`, where an in-process check
    /// against a plain nonexistent directory cannot prove anything about
    /// `bufadd`'s own symlink handling.
    #[test]
    fn nvim_style_absolute_leaves_a_nonexistent_parent_completely_untouched() {
        let nonce = format!(
            "definitely-not-a-real-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        );
        assert!(
            !std::path::Path::new(&format!("/{nonce}")).exists(),
            "the nonce directory name must not already exist, or this test proves nothing"
        );

        let dot = std::path::PathBuf::from(format!("/{nonce}/./b"));
        assert_eq!(
            nvim_style_absolute(&dot),
            dot,
            "a '.' component under a nonexistent parent must be left exactly as given"
        );

        let dotdot = std::path::PathBuf::from(format!("/{nonce}/../b"));
        assert_eq!(
            nvim_style_absolute(&dotdot),
            dotdot,
            "a '..' component under a nonexistent parent must be left exactly as given"
        );
    }

    #[test]
    fn nvim_style_absolute_resolves_dotdot_at_root_to_root_itself() {
        let path = std::path::PathBuf::from("/../a");
        assert_eq!(
            nvim_style_absolute(&path),
            std::path::PathBuf::from("/a"),
            "root's parent is root -- '/..' must resolve trivially rather than being \
             left unresolved the way a nonexistent parent is"
        );
    }

    /// Every spelling whose buffer identity `bufadd` and
    /// `canonical_hidden_key` would answer differently, and the ordinary
    /// ones that must still get through. A `load_hidden` these let past is
    /// a hold keyed on a path nvim resolved somewhere else.
    #[test]
    fn every_spelling_the_key_and_bufadd_disagree_on_is_refused() {
        for blank in ["", " ", "\t", "\n  "] {
            assert_eq!(
                hidden_path_refusal(blank),
                Some(HiddenPathRefusal::Blank),
                "a blank path resolves onto nvim's own [No Name] buffer: {blank:?}"
            );
        }
        for relative in ["rel.rs", "./rel.rs", "../rel.rs", "a/b.rs"] {
            assert_eq!(
                hidden_path_refusal(relative),
                Some(HiddenPathRefusal::Relative),
                "nvim's cwd resolves this one and this process's cwd cannot: {relative:?}"
            );
        }
        assert_eq!(
            hidden_path_refusal("/tmp/a/b.rs/"),
            Some(HiddenPathRefusal::TrailingSeparator),
            "a trailing separator is a second buffer over the same file"
        );
        assert_eq!(
            hidden_path_refusal("/tmp/dir/"),
            Some(HiddenPathRefusal::TrailingSeparator),
            "a directory's own spelling is refused here, not left to the fs_stat check"
        );
        for usable in ["/tmp/a.rs", "/tmp/does/not/exist.rs", "/a", "/tmp/./a.rs"] {
            assert_eq!(
                hidden_path_refusal(usable),
                None,
                "an absolute file spelling must still get through: {usable:?}"
            );
        }
    }

    /// A refused spelling never becomes a hold: `load_hidden` answers
    /// before it takes one, and `release_hidden` answers before it looks
    /// for one, so the two can never disagree about whether a hold exists.
    #[test]
    fn a_refused_path_takes_no_hold_and_releases_without_error() {
        let (h, _cap_rx) = fake_peer_replying_with(Value::Nil);
        let err = h.load_hidden("", 7).expect_err("a blank path must refuse");
        assert!(
            matches!(
                err,
                EngineError::UnusablePath {
                    reason: HiddenPathRefusal::Blank,
                    ..
                }
            ),
            "a blank path must refuse as unusable, not as a lost engine: {err:?}"
        );
        assert!(
            h.hidden_bufs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "a refused load must leave no hold behind for a release to find"
        );
        h.release_hidden("")
            .expect("releasing a refused path is the same no-op an unheld path gets");
    }

    /// The Rust key never invents an answer for a spelling only nvim can
    /// resolve: a relative path keeps its own spelling rather than being
    /// joined onto this process's cwd, which nvim's `:cd` moves
    /// independently.
    #[test]
    fn a_relative_spelling_never_picks_up_this_processs_cwd() {
        // cargo runs a unit test with the package root as its cwd, so
        // these two really do resolve against it -- which is what makes
        // the difference observable at all: a key that consulted this
        // process's cwd would answer them as absolute paths nvim's own cwd
        // need never agree with.
        assert!(
            std::path::Path::new("Cargo.toml").exists()
                && std::path::Path::new("src/nvim_api.rs").exists(),
            "the fixtures must resolve against this process's cwd, or the \
             assertions below prove nothing"
        );
        assert_eq!(canonical_hidden_key("Cargo.toml"), "Cargo.toml");
        assert_eq!(canonical_hidden_key("src/nvim_api.rs"), "src/nvim_api.rs");
        assert_eq!(
            canonical_hidden_key("./src/nvim_api.rs"),
            "./src/nvim_api.rs"
        );
        assert_eq!(canonical_hidden_key("rel.rs"), "rel.rs");
    }

    /// The two chunks that must agree share one `canon()` literal rather
    /// than two copies, which is the whole reason the probe can pin the
    /// Rust key at all.
    #[test]
    fn the_canon_probe_carries_the_same_resolution_the_load_chunk_does() {
        let canon = hidden_canon_lua!();
        assert!(LOAD_HIDDEN_CHUNK.contains(canon));
        assert!(HIDDEN_CANON_PROBE_CHUNK.contains(canon));
    }

    /// The nvim-side halves of the refusal, checked as chunk text: a blank
    /// path answered before anything resolves, and a scan that can never
    /// return a buffer with no name.
    #[test]
    fn the_load_chunk_refuses_a_blank_path_and_never_matches_a_nameless_buffer() {
        assert!(
            LOAD_HIDDEN_CHUNK.contains("path:match('^%s*$') ~= nil"),
            "a blank path must be refused before the scan can match [No Name]"
        );
        assert!(
            LOAD_HIDDEN_CHUNK.contains("if name ~= '' and canon(name) == wanted then"),
            "a name-less buffer must never be returned as a hidden-buffer hit"
        );
        assert!(
            LOAD_HIDDEN_CHUNK.contains("tail == '/' or tail == '\\\\'"),
            "a trailing separator resolves onto a second buffer over the same file"
        );
    }
}
