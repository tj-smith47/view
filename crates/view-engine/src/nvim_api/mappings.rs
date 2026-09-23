//! The default keys, the desktop chords and the `:View` command: one
//! registration chunk that claims every one of them and answers with what
//! it took.
//!
//! Beside [`super::buffers`] and [`super::window_status`] in shape (a
//! constant Lua chunk plus the [`super::EngineHandle`] method that sends
//! it), and with a method of its own beside the data: the chunk's
//! four arguments are assembled from two different tables
//! ([`default_maps`] and [`command_only_forms`]) and a caller-supplied spec
//! list, which is [`mapping_args`]'s own job and belongs beside the chunk
//! it feeds.

use rmpv::Value;

use crate::handle::EngineError;
use view_core::native::mappings::{
    command_only_forms, default_maps, is_spellable, MappingSpec, Rhs, COMMAND,
};

/// The lua chunk [`EngineHandle::register_mappings`] runs inside nvim,
/// taking view's channel id, the specs to register, every feature/verb pair
/// the command can complete, and the command's own name as its four
/// varargs. Constant by construction for the same reason as
/// [`FEED_KEYS_CHUNK`](super::FEED_KEYS_CHUNK): no caller data is
/// interpolated into the Lua source.
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
/// `maparg` returns for no mapping at all). The cost per fire is those
/// three calls -- the walk it replaces materialised every mapping of three
/// modes, globally and for every loaded buffer, on every event below.
///
/// Re-read on four events, reported on the `view_bridge` `colon_mapped`
/// event only when the answer moved. `User LazyLoad`, so a plugin that
/// loads late and maps `:` closes the gate for the rest of the session --
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
/// The command registers unconditionally, outside the spec loop: a user who
/// turned every default key off, or every feature, still has a way in.
pub(crate) const REGISTER_MAPPINGS_CHUNK: &str = "\
local channel, specs, entries, command = ...
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
local function reread()
  local now = colon_mapped()
  if now ~= colon then
    colon = now
    pcall(vim.rpcnotify, channel, 'view_bridge', 'colon_mapped', now)
  end
end
vim.api.nvim_create_autocmd('User', {
  group = group,
  pattern = 'LazyLoad',
  callback = reread,
})
vim.api.nvim_create_autocmd({ 'BufEnter', 'FileType', 'BufWinEnter' }, {
  group = group,
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
    lhs = spec.lhs,
    had_user_mapping = (taken[resolved] or taken[spec.lhs]) == true,
  }
end
vim.g.view_registered_keys = registered_now
vim.api.nvim_create_user_command(command, function(opts)
  local verb = table.concat(vim.list_slice(opts.fargs, 2), ' ')
  vim.rpcnotify(channel, 'view_invoke', opts.fargs[1] or '', verb)
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
return { claims = claimed, colon_mapped = colon }";

/// The two keys [`REGISTER_MAPPINGS_CHUNK`] answers under: the claim rows,
/// and its reading of whether `:` carries a user mapping. Pinned against the
/// chunk's own source by
/// `the_mapping_reply_names_the_keys_its_decoder_reads`.
pub(crate) const MAPPINGS_CLAIMS_KEY: &str = "claims";
pub(crate) const MAPPINGS_COLON_KEY: &str = "colon_mapped";

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

/// [`REGISTER_MAPPINGS_CHUNK`]'s four arguments, shared by the call that
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
    let entries = default_maps()
        .iter()
        .map(|spec| (spec.feature, spec.verb))
        .chain(
            command_only_forms()
                .iter()
                .map(|form| (form.feature, form.verb)),
        )
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
