-- WHY: remote-editing.sh's own fixture, scp'd to the remote host for the
-- one recording. The remote-editing tape's subject is SSH, clipboard and
-- config parity, not the remote host's own plugin health, and the daily
-- driver config on a real remote host carries whatever that host's LSP
-- and formatter installs are doing that day -- a Lua traceback and eight
-- "failed to install" toasts filled the frame for the whole recording
-- the last time this tape ran under it. This init carries no plugins, so
-- nothing on it can fail to install, and a built-in colorscheme so the
-- frame is not a bare terminal.
vim.o.termguicolors = true
vim.cmd.colorscheme("habamax")
vim.o.number = true
vim.o.ruler = true
