//! Typed convenience wrappers for the specific nvim RPC calls the terminal
//! frontend needs, so no caller outside this crate has to construct an
//! `rmpv::Value` by hand. `scripts/audit-deps.sh` forbids the bin crate
//! `view` from depending on `rmpv` directly; these methods are the sanctioned
//! way for it to reach the same calls.

use crate::handle::{EngineError, EngineHandle};
use crate::rpc::RpcError;
use rmpv::Value;
use std::time::Duration;
use view_core::msg::OptionValue;
use view_core::native::mappings::{default_maps, is_spellable, MappingSpec, COMMAND};

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
})";

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

/// Opens `path` as `:edit` would, taking it as its single positional
/// vararg. Constant, like every other chunk here: no caller data is
/// interpolated into the source itself.
///
/// `vim.fn.fnameescape` guards the one caller-controlled value the chunk
/// does interpolate: `path` reaches `vim.cmd.edit` as a literal argument to
/// an ex command, and an unescaped path containing a space, `%`, `#`, or a
/// leading `+` would otherwise be parsed as command syntax rather than a
/// filename -- see `docs/tree-open-file-wire-capture.md` for the live
/// capture backing that, including the unescaped negative control.
const OPEN_FILE_CHUNK: &str = "\
local path = ...
vim.cmd.edit(vim.fn.fnameescape(path))";

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
/// already handles, see `docs/tree-input-prompt-wire-capture.md`. Returns
/// the choice bare, nvim's own documented `confirm()` contract (`:help
/// confirm()`): `1` for Yes, `2` for No, `0` for a force-closed dialog
/// (`<Esc>` or an interrupt).
const TREE_DELETE_CONFIRM_CHUNK: &str = "\
local prompt = ...
return vim.fn.confirm(prompt, '&Yes\\n&No')";

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
    /// Live-verified against a real `nvim --clean --embed` (see
    /// `.superpowers/sdd/p2-task-10-report.md` for the captured transcript):
    /// registering this autocmd immediately AFTER `ui_attach` returns loses
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
                Value::Array(vec![Value::from(format!("Delete {path}?"))]),
            ],
            generation,
            path.to_owned(),
        )
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
    /// --clean --embed` (see `.superpowers/sdd/p2-task-10-report.md`):
    /// `++once` (self-clearing, never fires twice), plain `rpcrequest` (not
    /// `rpcnotify` -- the spec mandates blocking here), targeting
    /// `channel_id` explicitly (nvim has no loopback shorthand).
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
}
