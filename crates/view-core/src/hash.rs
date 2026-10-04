//! The FNV-1a hash every crate of view shares. Its algorithm is fixed and
//! documented, so a hash written to disk names the same bytes after a
//! toolchain upgrade.

/// The FNV-1a offset basis, the hash of no bytes.
pub const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

const FNV_PRIME: u64 = 0x0100_0000_01b3;

/// `hash` with `word` folded in by one FNV-1a step.
#[must_use]
pub fn fnv1a_step(hash: u64, word: u64) -> u64 {
    (hash ^ word).wrapping_mul(FNV_PRIME)
}

/// `hash` with every byte of `bytes` folded in, so bytes hashed in pieces
/// hash as they do whole.
#[must_use]
pub fn fnv1a_extend(hash: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(hash, |hash, &byte| fnv1a_step(hash, u64::from(byte)))
}

/// The FNV-1a hash of `bytes`.
#[must_use]
pub fn fnv1a(bytes: &[u8]) -> u64 {
    fnv1a_extend(FNV_OFFSET, bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn fnv1a_matches_the_published_vectors_whole_and_in_pieces() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
        assert_eq!(fnv1a_extend(fnv1a(b"foo"), b"bar"), fnv1a(b"foobar"));
    }

    /// No source file of the workspace outside this one spells the FNV-1a
    /// offset or prime, in hex or decimal, so every hash goes through the
    /// functions here.
    #[test]
    fn no_other_source_carries_its_own_fnv1a() {
        fn walk(dir: &std::path::Path, found: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("a readable source directory") {
                let path = entry.expect("a readable directory entry").path();
                if path.is_dir() {
                    if !path.ends_with("target") {
                        walk(&path, found);
                    }
                } else if path.extension().is_some_and(|ext| ext == "rs")
                    && !path.ends_with("view-core/src/hash.rs")
                {
                    let source = std::fs::read_to_string(&path).expect("a readable source");
                    let digits = source.to_lowercase().replace('_', "");
                    let constants = [
                        "cbf29ce484222325",
                        "14695981039346656037",
                        "100000001b3",
                        "1099511628211",
                    ];
                    if constants.iter().any(|c| digits.contains(c)) {
                        found.push(path.display().to_string());
                    }
                }
            }
        }
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut found = Vec::new();
        walk(&crates, &mut found);
        assert!(found.is_empty(), "a private FNV-1a copy in {found:?}");
    }
}
