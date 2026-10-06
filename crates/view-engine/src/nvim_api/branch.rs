//! What a DVR branch asks of the engine it replaces and of the one that
//! replays into: the swaps of the unsaved text, written out first, and
//! the answer that says the replay has run.

use rmpv::Value;

use crate::handle::{EngineError, EngineHandle};

/// Schedules the replay's answer behind every callback already queued, so
/// an invocation a replayed key deferred with `vim.schedule` reaches view
/// ahead of it. Takes view's channel id and the flush's generation.
const REPLAY_FLUSH_CHUNK: &str = "\
local channel, generation = ...
vim.schedule(function()
  pcall(vim.rpcnotify, channel, 'view_bridge', 'replay_flushed', generation)
end)";

/// Writes the swap of every loaded buffer with unsaved changes, since
/// `:preserve` writes the current buffer's alone.
const PRESERVE_CHANGED_CHUNK: &str = "\
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(buf) and vim.bo[buf].modified then
    vim.api.nvim_buf_call(buf, function()
      vim.cmd('silent! preserve')
    end)
  end
end";

impl EngineHandle {
    /// Asks nvim to answer once it has run every input sent ahead of this
    /// request and every callback that input scheduled. nvim reads pending
    /// input before it takes a request that is not `fast`, and runs this
    /// request's own callback behind those already queued. The answer
    /// crosses back as `Msg::ReplayFlushed` tagged `generation`, as does a
    /// request nvim refuses. Async: the caller is the runtime loop.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited.
    pub fn flush_replay(&self, generation: u64) -> Result<(), EngineError> {
        let waiter = crate::handle::Waiter::ReplayFlush { generation };
        let args = vec![Value::from(self.channel_id), Value::from(generation)];
        self.request_async(
            "nvim_exec_lua",
            vec![Value::from(REPLAY_FLUSH_CHUNK), Value::Array(args)],
            waiter,
        )
    }

    /// Writes the swap file of every loaded buffer with unsaved changes,
    /// waiting for nvim to finish.
    ///
    /// # Errors
    ///
    /// Returns the `EngineError` of the request: a closed connection, a
    /// Lua error, or no reply within the eval timeout.
    pub fn preserve_changed(&self) -> Result<(), EngineError> {
        self.request_timeout(
            "nvim_exec_lua",
            vec![Value::from(PRESERVE_CHANGED_CHUNK), Value::Array(vec![])],
            super::EVAL_TIMEOUT,
        )?;
        Ok(())
    }
}
