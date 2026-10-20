// SPDX-License-Identifier: GPL-3.0-only
//! Report unavailable configured modules without preventing startup.

use std::collections::BTreeSet;
use std::path::Path;

fn names(text: &str) -> BTreeSet<&str> {
    text.lines()
        .map(|line| line.trim_end_matches([' ', '\t', '\r']))
        .filter(|line| {
            !line.is_empty()
                && line
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                && (!cfg!(feature = "wireless") || *line != "wireless")
                && !matches!(
                    *line,
                    "cluster" | "sensor" | "input" | "ui" | "launcher" | "aauplink"
                )
        })
        .collect()
}

pub fn report(path: &str) {
    // This bounded read describes the file at startup. Module admission still
    // uses the settings reader's pinned snapshot if the file changes concurrently.
    match extender_settings::store::read_bounded(Path::new(path)) {
        Ok(text) => {
            for name in names(&text) {
                crate::ldr_loge!(
                    "[GalLoader] mono_ms={} wall_ms={} feature '{}': unavailable in this build; continuing",
                    crate::sys::mono_ms(),
                    crate::sys::wall_ms(),
                    name
                );
            }
        }
        Err(error) => crate::ldr_loge!(
            "[GalLoader] mono_ms={} wall_ms={} unavailable-feature check failed: {}; continuing",
            crate::sys::mono_ms(),
            crate::sys::wall_ms(),
            error
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_each_unavailable_name_once_without_changing_enabled_modules() {
        assert_eq!(
            names("cluster\nsensor\ninput\ninput \r\nwireless\nfuture-module\n"),
            if cfg!(feature = "wireless") {
                BTreeSet::from(["future-module"])
            } else {
                BTreeSet::from(["wireless", "future-module"])
            }
        );
    }

    #[test]
    fn matches_bare_feature_lines_not_keys_comments_or_leading_space() {
        assert!(names("cluster\nsensor\n# input\n input\ncodec=1\n\n").is_empty());
    }
}
