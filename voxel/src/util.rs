// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Small cross-module helpers (shell quoting, temp dir).

use camino::Utf8PathBuf;

/// Single-quote a string for safe interpolation into a shell command: wrap in
/// `'...'` with any embedded single quote escaped as `'\''`. Path callers pass
/// `&p.to_string()`.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The system temp dir as a UTF-8 path; panics on a non-UTF-8 TMPDIR.
pub(crate) fn temp_dir() -> Utf8PathBuf {
    Utf8PathBuf::try_from(std::env::temp_dir()).expect("TMPDIR is not UTF-8")
}
