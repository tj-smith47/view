-- No scenario drives this fixture; it exists so bindings.txt (the omarchy
-- chord table's fixture, read by view-core::native::chords's tests) sits in
-- a directory shaped like every other compat/fixtures entry, which
-- fixture_native_config.rs requires of anything living here. Opens the
-- probe channel the same way every other fixture does -- see
-- crates/view-oracle/src/compat.rs's module docs for why a second RPC
-- channel, rather than pty-screen scraping, is the mechanism.
vim.fn.serverstart(vim.env.VIEW_COMPAT_SOCK)
