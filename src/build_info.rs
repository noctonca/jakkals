//! Which build is running: the version, and the commit it was built
//! from when `build.rs` could tell. See "Which build ran" in
//! docs/ARCHITECTURE.md.

/// A build of Jakkals, as the `start` event records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Build {
    pub version: &'static str,
    /// The full git hash; `None` when unknown.
    pub commit: Option<&'static str>,
    /// Whether the build's files differed from `commit`; `None` when
    /// unknown.
    pub dirty: Option<bool>,
}

/// This binary's build.
pub const THIS: Build = Build {
    version: env!("CARGO_PKG_VERSION"),
    // build.rs always sets both, empty when unknown, so a variable of
    // the same name in the builder's environment can't stand in.
    commit: if env!("JAKKALS_COMMIT").is_empty() {
        None
    } else {
        Some(env!("JAKKALS_COMMIT"))
    },
    dirty: match env!("JAKKALS_DIRTY").as_bytes() {
        [b'1'] => Some(true),
        [b'0'] => Some(false),
        _ => None,
    },
};

/// What `--version` shows: the version, then the short commit and
/// whether it was dirty, when known.
pub const VERSION_TEXT: &str = env!("JAKKALS_VERSION_TEXT");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_commit_comes_with_its_dirty_flag() {
        assert_eq!(THIS.commit.is_some(), THIS.dirty.is_some());
        if let Some(commit) = THIS.commit {
            assert_eq!(commit.len(), 40);
            assert!(VERSION_TEXT.contains(&commit[..12]));
        }
        assert!(VERSION_TEXT.starts_with(THIS.version));
    }
}
