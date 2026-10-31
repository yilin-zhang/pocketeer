//! Staging space for atomic installs, under the card's `.pocketeer/`
//! folder. Nothing is kept here after an operation finishes: the library is
//! the backup, the card is just a card.

use std::path::PathBuf;

use crate::{Pocket, fsutil};

pub fn staging_root(pocket: &Pocket) -> PathBuf {
    pocket.pocketeer_dir().join("staging")
}

/// Where files displaced by one operation wait until it succeeds (then the
/// folder is deleted) or fails (then they are moved back).
pub fn new_undo_dir(pocket: &Pocket, label: &str) -> PathBuf {
    staging_root(pocket).join(format!(
        "{}-{}-{}-undo",
        fsutil::timestamp(),
        sanitize(label),
        std::process::id()
    ))
}

/// A new, unique staging directory path for an operation (not yet created).
pub fn new_staging_dir(pocket: &Pocket, label: &str) -> PathBuf {
    staging_root(pocket).join(format!(
        "{}-{}-{}",
        fsutil::timestamp(),
        sanitize(label),
        std::process::id()
    ))
}

fn sanitize(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(60)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_labels() {
        assert_eq!(sanitize("agg23.Arduboy"), "agg23.Arduboy");
        assert_eq!(sanitize("a b/c"), "a_b_c");
    }
}
