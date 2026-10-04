//! The default keys, the desktop chords and the `:View` command: one
//! registration chunk that claims every one of them and answers with what
//! it took.
//!
//! Beside [`super::buffers`] and [`super::window_status`] in shape (a
//! constant Lua chunk plus the [`super::EngineHandle`] method that sends
//! it), and with a method of its own beside the data: the chunk's
//! five arguments are assembled from two different tables
//! ([`default_maps`] and [`command_only_forms`]) and a caller-supplied spec
//! list, which is [`mapping_args`]'s own job and belongs beside the chunk
//! it feeds.

use rmpv::Value;

use crate::handle::EngineError;
use view_core::native::mappings::{
    command_only_forms, default_maps, is_spellable, is_token, MappingSpec, Rhs, COMMAND,
};

/// The lua chunk [`EngineHandle::register_mappings`] runs inside nvim,
/// taking view's channel id, the specs to register, every feature/verb pair
/// the command can complete, the command's own name and
/// [`REGISTER_COMMAND_CHUNK`] as its five varargs. Constant by
/// construction for the same reason as
/// [`FEED_KEYS_CHUNK`](super::FEED_KEYS_CHUNK): no caller data is
/// interpolated into the Lua source.
///
/// A claim for a key that invokes view carries `keys`, the key sequence
/// nvim matches for it with the leader resolved, spelled by `keytrans()`.
/// View holds the keys typed behind that sequence until the invocation
/// reports back, so they reach the surface it opens.
///
/// One chunk for every key, and it answers with the whole claim list: what
/// view claimed is one fact a user is told once, so it is established in one
/// atomic pass over the specs. Replies to a call per key would interleave
/// with startup traffic.
///
/// It answers with one more reading beside the claims: whether the user's
/// config maps `:` in normal or visual mode
/// ([`MAPPINGS_COLON_KEY`]). The palette speculates the command line open on
/// that keystroke (`view_core::native::speculate::may_speculate_cmdline`)
/// and needs the answer on every `:`, which is not a question to put on the
/// wire per keystroke; the snapshot this chunk already takes has it.
///
/// Three `vim.fn.maparg(':', mode)` calls, for `n`, `x` and `v`, and no
/// keymap walk: `maparg` answers for the current buffer's own mappings as
/// well as the global ones, and the current buffer is the one the `:` is
/// going to. It reads a Lua-callback mapping as well as a string one (the
/// rhs comes back as `<Lua 42: ...>`, which is not the empty string
/// `maparg` returns for no mapping at all). The `:` reading costs those
/// three calls per fire; the user's keys below cost one walk of the global
/// normal-mode table, and no buffer's own.
///
/// Re-read on five events, reported on the `view_bridge` `colon_mapped`
/// event only when the answer moved. `User LazyLoad` and `User VeryLazy`,
/// so a plugin or a keymap file that loads late and maps `:` closes the
/// gate for the rest of the session --
/// and `BufEnter`, `FileType` and `BufWinEnter` beside it, because a config
/// that loads no plugin lazily fires the first one never, and because a
/// buffer-local `:` map belongs to whichever buffer is current: an ftplugin
/// mapping `:` in a file opened later is seen at the moment its buffer
/// becomes the one being typed into, and leaving that buffer is reported
/// the same way. `BufEnter` is what carries the window switch, which is the
/// change of current buffer the other two say nothing about: moving into a
/// window whose buffer maps `:` -- `<C-w>w`, a jump back out of a file
/// tree -- would otherwise leave the reading `false` and speculate a
/// palette for a key that reaches the mapping.
///
/// Those listeners get an augroup of their own (`view_colon_map`) that
/// never retires itself: a group that stops listening part way through a
/// session leaves every plugin loaded after that free to map `:`
/// unobserved.
///
/// A run first restores whatever the previous run left in
/// `vim.g.view_registered_keys` -- a profile flip or a late kitty-protocol
/// answer re-registers the whole set under a different modifier, and
/// without a restore the first run's claims would linger as view's own
/// mappings under keys the new run no longer spells that way. Every `lhs`
/// this run is about to set is snapshotted again, right before it is set,
/// into that same global for the *next* run to restore -- an empty dict
/// means nothing preceded view there, and the restore deletes view's own
/// mapping.
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
/// A [`Rhs::Keys`] spec's right-hand side is the chord's own key sequence,
/// set with no remapping: `<D-Left>` becomes `<C-w>h` exactly as if the
/// user had typed it, which costs nothing beyond what typing `<C-w>h`
/// already costs. Every other spec's right-hand side is the plain
/// `<Cmd>rpcnotify(...)<CR>` string every default key has always used, so
/// that `:map`, `maparg()`, and every plugin that introspects mappings show
/// exactly what view did, in a form a user can read and copy. It is built
/// with `string.format` from the spec's own fields inside the chunk, never
/// interpolated into the chunk source, and every token reaching it is
/// `[a-z0-9_-]` by [`is_spellable`], applied in
/// [`register_mappings`](EngineHandle::register_mappings).
///
/// Normal mode is the whole scope of the *claims*, matching
/// [`MappingSpec`]'s own normal-mode-only contract: the snapshot reads `'n'`
/// maps, globally and per loaded buffer, and every key is set with
/// `vim.keymap.set('n', ...)`. A spec carries no mode to vary that by, so
/// every `'n'` literal in the chunk below is the scope, and no caller
/// overrides it. The `:` reading beside them is the one thing that
/// spans `n`, `x` and `v`, because a `:` typed in visual mode opens a
/// command line too.
///
/// The same global snapshot answers with every sequence the user's config
/// maps in normal mode, spelled by `keytrans()`, and `'timeoutlen'` (`-1`
/// where `'timeout'` is off): a surface of view's own with a window of its
/// own passes those keys on to nvim, the way a buffer tile does, and waits
/// on a sequence's prefix as long as nvim would. `<Plug>` and `<SNR>` keys
/// are left out, since no one types them, and so are view's own, which
/// the claims carry. Both are read again by the same listener as `:`, and
/// sent on the `view_bridge` `user_keys` event when they moved, so a
/// mapping a config sets on `VeryLazy` reaches the windowed tree the way
/// it reaches a tile. That read walks every global normal-mode map, and one
/// file open raises three or four of the events, so the events of one tick
/// share a single walk scheduled after them.
///
/// The current buffer's own normal-mode mappings are read first and join
/// the keys, each shadowing a global mapping with the same lhs. `LspAttach`
/// re-reads them beside the events above, since a language server's keys
/// are set on the buffer after it is entered. `CursorHold` compares the
/// maps it sees, the current buffer's and the global ones, with the last
/// read and reads again only where they moved, for a mapping set from a
/// deferred callback or by a stub remapping itself. `OptionSet` on
/// `'timeout'` or `'timeoutlen'` reads again too.
///
/// Each of those keys is answered with whose it is, under `user_owners`
/// and on the `user_owners` bridge event, one row per key: its `keytrans()`
/// lhs, the mapping's `desc`, else its rhs, else `<Lua callback>`, whether
/// it is the buffer's own, and the file that set it relative to the config
/// directory. That file is the script nvim names (a positive `sid`), or
/// the file and line a Lua callback was defined at. A C function has none,
/// and a callback defined in nvim's own runtime names the file of a
/// function it wraps, else none. The event is sent when a row moved, a
/// description alone included. A claim carries the same description of
/// the mapping it was set over under `displaced`, read from the snapshot
/// the restore keeps, and the verb its spec names. The key log reads both,
/// so a fired mapping is described with no read of its own.
///
/// The same read answers with the user's command-line mappings and
/// abbreviations (`maplist()` rows in mode `c` or `!`), each as its
/// `keytrans()` lhs, its rhs and its `abbr`, `noremap`, `expr`, `nowait`
/// and `buffer` flags, a Lua callback counted as `expr`. nvim expands them
/// after view has sent the keys, so the input hold behind a `:View` reads
/// a submitted line through them. They ride the `user_keys` event as its
/// third argument.
///
/// Two more events re-read them, beside the five above: `SourcePost`, for
/// a config sourced again, and `CmdlineLeave` on a `:` line, for a
/// `:cabbrev` or `:cunmap` typed at the prompt. The read is scheduled, so
/// it runs after the command that line submits.
///
/// The command registers unconditionally, outside the spec loop: a user who
/// turned every default key off, or every feature, still has a way in.
pub(crate) const REGISTER_MAPPINGS_CHUNK: &str = "\
local channel, specs, entries, command, command_chunk = ...
local previous = vim.g.view_registered_keys or {}
for lhs, saved in pairs(previous) do
  if type(saved) == 'table' and next(saved) ~= nil then
    pcall(vim.fn.mapset, 'n', false, saved)
  else
    pcall(vim.keymap.del, 'n', lhs)
  end
end
local taken = {}
local function note(maps)
  for _, m in ipairs(maps) do
    if m.lhsraw then taken[m.lhsraw] = true end
    taken[m.lhs] = true
  end
end
note(vim.api.nvim_get_keymap('n'))
local config = vim.fn.stdpath('config') .. '/'
local scripts = {}
local function short(name)
  if vim.startswith(name, config) then return name:sub(#config + 1) end
  return name ~= '' and vim.fn.fnamemodify(name, ':~') or ''
end
local runtime = vim.fs.normalize(vim.env.VIMRUNTIME or '') .. '/'
local function defined(fn)
  local info = debug.getinfo(fn, 'S')
  local src = info.source or ''
  if info.what == 'C' or not vim.startswith(src, '@') then return nil end
  return vim.fs.normalize(src:sub(2)), info.linedefined
end
local function callback_script(fn)
  -- a Lua-set mapping records no script id unless nvim runs verbose, so
  -- the callback's own definition names the file
  local file, line = defined(fn)
  if file and vim.startswith(file, runtime) then
    -- nvim's keymap wrapper closes over the function the config passed
    file = nil
    local i = 1
    while file == nil do
      local name, value = debug.getupvalue(fn, i)
      if name == nil then break end
      if type(value) == 'function' then
        local f, l = defined(value)
        if f and not vim.startswith(f, runtime) then file, line = f, l end
      end
      i = i + 1
    end
  end
  return file and short(file) .. ':' .. line or nil
end
local function owner(m)
  if type(m) ~= 'table' or next(m) == nil then return nil end
  local label = m.desc
  if label == nil or label == '' then
    label = (m.rhs ~= nil and m.rhs ~= '') and m.rhs or '<Lua callback>'
  end
  local lhs = vim.fn.keytrans(m.lhsraw or m.lhs or '')
  local buffer = (m.buffer or 0) ~= 0
  if type(m.callback) == 'function' then
    return { lhs = lhs, label = label, script = callback_script(m.callback),
      buffer = buffer }
  end
  local sid = m.sid or 0
  if sid > 0 and scripts[sid] == nil then
    local ok, info = pcall(vim.fn.getscriptinfo, { sid = sid })
    scripts[sid] = short(ok and info[1] and info[1].name or '')
  end
  local script = scripts[sid]
  return { lhs = lhs, label = label, buffer = buffer,
    script = script ~= '' and script or nil }
end
local function signature(maps)
  local parts = {}
  for _, list in ipairs(maps) do
    for _, m in ipairs(list) do
      parts[#parts + 1] = m.lhs .. '\\0' .. (m.rhs or tostring(m.callback))
        .. '\\0' .. (m.desc or '')
    end
  end
  return table.concat(parts, '\\n')
end
local function keymaps()
  return { vim.api.nvim_buf_get_keymap(0, 'n'), vim.api.nvim_get_keymap('n') }
end
local held_maps = ''
local function read_user_keys()
  local keys, owners, rows, mapped = {}, {}, {}, {}
  local maps = keymaps()
  held_maps = signature(maps)
  for _, list in ipairs(maps) do
    for _, m in ipairs(list) do
      local lhs = vim.fn.keytrans(m.lhsraw or m.lhs)
      local typed = not vim.startswith(m.lhs, '<Plug>')
        and not vim.startswith(m.lhs, '<SNR>')
        and not vim.startswith(m.desc or '', 'view: ')
        and not mapped[lhs]
      if typed then
        mapped[lhs] = true
        keys[#keys + 1] = lhs
        local row = owner(m)
        if row then
          owners[#owners + 1] = row
          rows[#rows + 1] = table.concat({ row.lhs, row.label,
            row.script or '', tostring(row.buffer) }, '\\0')
        end
      end
    end
  end
  local cmdline, seen = {}, {}
  for _, abbr in ipairs({ false, true }) do
    for _, m in ipairs(vim.fn.maplist(abbr)) do
      if m.mode == 'c' or m.mode == '!' then
        local row = {
          lhs = vim.fn.keytrans(m.lhsraw or m.lhs),
          rhs = m.rhs or '',
          abbr = abbr,
          noremap = m.noremap == 1,
          expr = m.expr == 1 or m.callback ~= nil,
          nowait = m.nowait == 1,
          buffer = (m.buffer or 0) ~= 0,
        }
        cmdline[#cmdline + 1] = row
        seen[#seen + 1] = table.concat({ row.lhs, row.rhs,
          tostring(abbr), tostring(row.noremap), tostring(row.expr),
          tostring(row.nowait), tostring(row.buffer) }, '\\0')
      end
    end
  end
  local wait = vim.o.timeout and vim.o.timeoutlen or -1
  return keys, wait, cmdline, table.concat(seen, '\\n'), owners,
    table.concat(rows, '\\n')
end
local user_keys, timeoutlen, cmdline_maps, cmdline_read, user_owners,
  owners_read = read_user_keys()
local user_read = table.concat(user_keys, ' ') .. ' ' .. timeoutlen
  .. '\\n' .. cmdline_read
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(buf) then
    note(vim.api.nvim_buf_get_keymap(buf, 'n'))
  end
end
local function colon_mapped()
  for _, mode in ipairs({ 'n', 'x', 'v' }) do
    if vim.fn.maparg(':', mode) ~= '' then return true end
  end
  return false
end
local colon = colon_mapped()
local group = vim.api.nvim_create_augroup('view_colon_map', { clear = true })
local keys_pending = false
local function reread_keys()
  keys_pending = false
  local keys, wait, maps, maps_read, owners, read_owners = read_user_keys()
  local read = table.concat(keys, ' ') .. ' ' .. wait .. '\\n' .. maps_read
  if read ~= user_read then
    user_read = read
    pcall(vim.rpcnotify, channel, 'view_bridge', 'user_keys', keys, wait,
      maps)
  end
  if read_owners ~= owners_read then
    owners_read = read_owners
    pcall(vim.rpcnotify, channel, 'view_bridge', 'user_owners', owners)
  end
end
local function reread()
  local now = colon_mapped()
  if now ~= colon then
    colon = now
    pcall(vim.rpcnotify, channel, 'view_bridge', 'colon_mapped', now)
  end
  if not keys_pending then
    keys_pending = true
    vim.schedule(reread_keys)
  end
end
vim.api.nvim_create_autocmd('User', {
  group = group,
  pattern = { 'LazyLoad', 'VeryLazy' },
  callback = reread,
})
vim.api.nvim_create_autocmd({ 'BufEnter', 'FileType', 'BufWinEnter',
  'LspAttach' }, {
  group = group,
  callback = reread,
})
vim.api.nvim_create_autocmd('OptionSet', {
  group = group,
  pattern = { 'timeout', 'timeoutlen' },
  callback = reread,
})
-- a mapping set from a deferred callback, or by a stub remapping itself,
-- raises no event of its own, so an idle moment compares the maps it sees
vim.api.nvim_create_autocmd('CursorHold', {
  group = group,
  callback = function()
    if signature(keymaps()) ~= held_maps then reread() end
  end,
})
vim.api.nvim_create_autocmd('SourcePost', { group = group, callback = reread })
vim.api.nvim_create_autocmd('CmdlineLeave', {
  group = group,
  pattern = ':',
  callback = reread,
})
local claimed = {}
local registered_now = {}
for _, spec in ipairs(specs) do
  local resolved = vim.api.nvim_replace_termcodes(spec.lhs, true, true, true)
  registered_now[spec.lhs] = vim.fn.maparg(spec.lhs, 'n', false, true)
  local rhs
  if spec.keys then
    rhs = spec.keys
  else
    rhs = string.format(
      \"<Cmd>call rpcnotify(%d, 'view_invoke', '%s', '%s')<CR>\",
      channel, spec.feature, spec.verb)
  end
  vim.keymap.set('n', spec.lhs, rhs, {
    desc = string.format('view: %s %s', spec.feature, spec.verb),
    silent = true,
  })
  claimed[#claimed + 1] = {
    feature = spec.feature,
    verb = spec.verb,
    lhs = spec.lhs,
    had_user_mapping = (taken[resolved] or taken[spec.lhs]) == true,
    keys = (not spec.keys) and vim.fn.keytrans(resolved) or nil,
    displaced = owner(registered_now[spec.lhs]),
  }
end
vim.g.view_registered_keys = registered_now
assert(load(command_chunk))(channel, entries, command)
return {
  claims = claimed,
  colon_mapped = colon,
  user_keys = user_keys,
  user_owners = user_owners,
  timeoutlen = timeoutlen,
  cmdline_maps = cmdline_maps,
}";

/// The lua chunk that creates the `:View` command, taking view's channel id,
/// every feature/verb pair the command completes, and the command's own
/// name as its three varargs.
///
/// It depends on nothing a config sets (the keys wait for `mapleader`, the
/// command has no key), so the startup `--cmd` runs it before the user's
/// config and before any `-c`: `view -c 'View ai open'` and an `init.lua`
/// that calls `:View` both find the command there. The command's
/// invocations reach view as notifications, and a feature invoked before
/// the session can open it takes the deferred-command path.
/// [`REGISTER_MAPPINGS_CHUNK`] runs it again, so a caller that registers
/// over the channel alone still gets the command.
pub(crate) const REGISTER_COMMAND_CHUNK: &str = "\
local channel, entries, command = ...
vim.api.nvim_create_user_command(command, function(opts)
  local verb = table.concat(vim.list_slice(opts.fargs, 2), ' ')
  vim.rpcnotify(channel, 'view_invoke', opts.fargs[1] or '', verb)
end, {
  nargs = '*',
  bar = true,
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
})";

/// Every feature/verb pair `:View` completes: the keyed entry points and the
/// command-only forms, whatever this session mapped.
fn command_entries() -> impl Iterator<Item = (&'static str, &'static str)> {
    default_maps()
        .iter()
        .map(|spec| (spec.feature, spec.verb))
        .chain(
            command_only_forms()
                .iter()
                .map(|form| (form.feature, form.verb)),
        )
}

/// [`command_entries`] as a Lua table literal, for the startup `--cmd`,
/// which has no channel to receive arguments over. A pair whose tokens
/// need escaping is left out, the same vetting [`is_spellable`] gives a
/// key, and every compiled-in pair passes it.
pub(crate) fn command_entries_lua() -> String {
    let rows: Vec<String> = command_entries()
        .filter(|(feature, verb)| is_token(feature) && is_token(verb))
        .map(|(feature, verb)| format!("{{ feature = '{feature}', verb = '{verb}' }}"))
        .collect();
    format!("{{ {} }}", rows.join(", "))
}

/// The two keys [`REGISTER_MAPPINGS_CHUNK`] answers under: the claim rows,
/// and its reading of whether `:` carries a user mapping. Pinned against the
/// chunk's own source by
/// `the_mapping_reply_names_the_keys_its_decoder_reads`.
pub(crate) const MAPPINGS_CLAIMS_KEY: &str = "claims";
pub(crate) const MAPPINGS_COLON_KEY: &str = "colon_mapped";
/// The keys the user's own normal-mode mappings answer under, and
/// `'timeoutlen'` beside them, in the same reply.
pub(crate) const MAPPINGS_USER_KEYS_KEY: &str = "user_keys";
pub(crate) const MAPPINGS_TIMEOUT_KEY: &str = "timeoutlen";
/// The user's command-line mappings and abbreviations, in the same reply.
pub(crate) const MAPPINGS_CMDLINE_KEY: &str = "cmdline_maps";

impl super::EngineHandle {
    /// Registers `specs` as real nvim mappings and the `:View` command in
    /// one [`REGISTER_MAPPINGS_CHUNK`] pass, notifying back to `channel_id`
    /// when either is used.
    ///
    /// Async by construction, like
    /// [`probe_default_hl`](Self::probe_default_hl): this issues the
    /// request through [`EngineHandle::request_mappings`] and returns
    /// immediately, and the claim list crosses back as
    /// `Msg::MappingsClaimed` through the connection's pump. The caller is
    /// the runtime loop, which never awaits an RPC reply.
    ///
    /// A request, unlike the other calls the loop emits, which are
    /// notifications: the reply is the claim list, and an error reply is how
    /// a chunk nvim refused is reported at all. Without it the keys would
    /// silently never register.
    ///
    /// The `:View` completion candidates come from [`default_maps`]: the
    /// command is registered whatever the user has turned off, so what it
    /// completes is every entry point this build has, including those this
    /// session mapped no key to.
    ///
    /// A spec whose tokens [cannot be spelled](is_spellable) inside the
    /// mapping the chunk generates is dropped here: this method takes any
    /// `&[MappingSpec]`, and the table's own vetting in `view-core` cannot
    /// speak for a spec a future caller assembles.
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
        self.request_mappings(
            "nvim_exec_lua",
            vec![
                Value::from(REGISTER_MAPPINGS_CHUNK),
                Value::Array(mapping_args(specs, channel_id)),
            ],
        )
    }
}

/// [`REGISTER_MAPPINGS_CHUNK`]'s five arguments, shared by the call that
/// sends it alone and the takeover that batches it.
pub(crate) fn mapping_args(specs: &[MappingSpec], channel_id: u64) -> Vec<Value> {
    let specs = specs
        .iter()
        .filter(|spec| is_spellable(spec))
        .map(|spec| {
            let mut fields = vec![
                (Value::from("feature"), Value::from(spec.feature)),
                (Value::from("lhs"), Value::from(spec.lhs.as_ref())),
                (Value::from("verb"), Value::from(spec.verb)),
            ];
            if let Rhs::Keys(keys) = spec.rhs {
                fields.push((Value::from("keys"), Value::from(keys)));
            }
            Value::Map(fields)
        })
        .collect();
    let entries = command_entries()
        .map(|(feature, verb)| {
            Value::Map(vec![
                (Value::from("feature"), Value::from(feature)),
                (Value::from("verb"), Value::from(verb)),
            ])
        })
        .collect();
    vec![
        Value::from(channel_id),
        Value::Array(specs),
        Value::Array(entries),
        Value::from(COMMAND),
        Value::from(REGISTER_COMMAND_CHUNK),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::borrow::Cow;

    use super::*;

    fn spec(feature: &'static str, verb: &'static str, rhs: Rhs) -> MappingSpec {
        MappingSpec {
            feature,
            lhs: Cow::Borrowed("<M-x>"),
            verb,
            rhs,
        }
    }

    fn arg_map(args: &[Value]) -> &Vec<(Value, Value)> {
        args[1]
            .as_array()
            .expect("the specs must cross as an array")[0]
            .as_map()
            .expect("each spec must cross as a map")
    }

    /// A chord that stands in for a plain nvim keystroke (`<C-w>h` and
    /// its siblings) crosses with its own `keys` field, which is what
    /// `REGISTER_MAPPINGS_CHUNK`'s `if spec.keys then rhs = spec.keys`
    /// branch reads, and no `rpcnotify` bridge call is built for it.
    #[test]
    fn a_keys_row_sets_the_nvim_keys_with_no_remapping() {
        let specs = [spec("window", "focus_left", Rhs::Keys("<C-w>h"))];
        let args = mapping_args(&specs, 7);
        let fields = arg_map(&args);
        assert!(
            fields.contains(&(Value::from("keys"), Value::from("<C-w>h"))),
            "a Keys row must carry its own nvim keys across: {fields:?}"
        );
    }

    /// The startup `--cmd` spells every pair the command completes, so no
    /// pair completes over the channel and not before `VimEnter`.
    #[test]
    fn the_startup_command_completes_every_pair_the_channel_does() {
        let lua = command_entries_lua();
        assert_eq!(
            lua.matches("feature = ").count(),
            command_entries().count(),
            "{lua}"
        );
    }

    /// The default row shape -- a bridge call into view -- crosses with no
    /// `keys` field at all, which is what sends `REGISTER_MAPPINGS_CHUNK`
    /// down its `rpcnotify(channel, 'view_invoke', feature, verb)` branch.
    #[test]
    fn an_invoke_row_sets_the_bridge_call() {
        let specs = [spec("picker", "files", Rhs::Invoke)];
        let args = mapping_args(&specs, 7);
        let fields = arg_map(&args);
        assert!(
            !fields.iter().any(|(k, _)| *k == Value::from("keys")),
            "an Invoke row must carry no keys field, so the chunk falls \
             through to its rpcnotify branch: {fields:?}"
        );
    }
}
