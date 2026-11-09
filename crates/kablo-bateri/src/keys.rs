//! The keys a machine trusts for a program without being asked: the programs bateri makes itself.
//!
//! Any other key is the person's to approve, for one program at a time, and the program's
//! directory keeps the answer ([`crate::paths::APP_KEY`]). Nothing a peer sends adds to this list;
//! a key enters it by a release of kablo.

/// bateri's own key: the one the programs bateri releases (the text editor) are signed with.
pub const BATERI: &str = "ed25519:59e8IXkv6FP4tD4uJ/fvw9/pSV+1YgEpXtXQzWLwOpU=";

/// Every key trusted without asking.
pub const EMBEDDED: &[&str] = &[BATERI];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_written_the_way_an_app_json_writes_it() {
        for key in EMBEDDED {
            let body = key.strip_prefix("ed25519:").expect("a prefix");
            // 32 bytes in base64 with its padding.
            assert_eq!(body.len(), 44, "{key}");
            assert!(body.ends_with('='), "{key}");
        }
        assert!(EMBEDDED.contains(&BATERI));
    }
}
