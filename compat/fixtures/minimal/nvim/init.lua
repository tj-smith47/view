-- The compat harness's no-plugins fixture: proves the driver mechanics
-- (spawn, step execution, probe channel, zero-error epilogue) with nothing
-- else in play. Opens the probe channel the same way every other fixture
-- does -- see crates/view-oracle/src/compat.rs's module docs for why a
-- second RPC channel, rather than pty-screen scraping, is the mechanism.
vim.fn.serverstart(vim.env.VIEW_COMPAT_SOCK)
