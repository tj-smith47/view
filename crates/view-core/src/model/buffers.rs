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
        }
    }
}
