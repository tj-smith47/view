//! The listed buffers, as the bridge's `buffers` trigger reports them.
//!
//! nvim answers the trigger with the whole set, so the model holds the
//! whole set: a diff kept here would be a second reading of a fact nvim
//! already stated in full, and the two would part the first time an event
//! was missed.

/// One listed buffer the pill can name.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BufferEntry {
    /// The buffer handle a click on the name selects.
    pub buf: u64,
    /// The buffer's tail name, empty for one that has never been named.
    pub name: String,
    /// Whether the buffer has unsaved changes.
    pub modified: bool,
    /// Whether this is the buffer the session is on.
    pub current: bool,
    /// The file's full path, empty for a buffer that holds no file.
    pub path: String,
}

impl BufferEntry {
    /// One reported buffer, whole.
    ///
    /// The struct is `#[non_exhaustive]`, so a caller outside this crate
    /// cannot write it as a literal and would otherwise build one field by
    /// field off a default it does not mean.
    #[must_use]
    pub fn new(buf: u64, name: String, modified: bool, current: bool) -> Self {
        Self {
            buf,
            name,
            modified,
            current,
            path: String::new(),
        }
    }

    /// This entry with the file's full path.
    #[must_use]
    pub fn with_path(mut self, path: String) -> Self {
        self.path = path;
        self
    }
}

/// The files a replacement engine opens to bring the session back: every
/// listed buffer that holds a file, in list order, with the current one
/// last, which leaves it the buffer on screen.
#[must_use]
pub fn reopen_order(buffers: &[BufferEntry]) -> Vec<String> {
    let files = buffers.iter().filter(|entry| !entry.path.is_empty());
    let (current, others): (Vec<&BufferEntry>, Vec<&BufferEntry>) =
        files.partition(|entry| entry.current);
    others
        .into_iter()
        .chain(current)
        .map(|entry| entry.path.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reopen_order_keeps_the_list_and_ends_on_the_current_file() {
        let entry = |buf: u64, path: &str, current: bool| {
            BufferEntry::new(buf, String::new(), false, current).with_path(path.to_string())
        };
        let buffers = [
            entry(1, "/w/a.rs", false),
            entry(2, "/w/b.rs", true),
            entry(3, "", false),
            entry(4, "/w/c.rs", false),
        ];
        assert_eq!(reopen_order(&buffers), ["/w/a.rs", "/w/c.rs", "/w/b.rs"]);
        assert!(reopen_order(&buffers[2..3]).is_empty());
    }
}
