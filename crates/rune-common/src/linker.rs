//! The linker built into Rune: hyprlnk (`build.linker = "hyprlnk"`).
//!
//! A unit linked by it has the linker `rune:hyprlnk@<version>`, which is what
//! its cache key records: the same on every machine and in every directory,
//! and different for every hyprlnk version. Only when the compiler runs is it
//! turned into `-C linker=<this executable>` with [`EMBEDDED_ENV`] set, which
//! makes the Rune executable act as the linker when `rustc` starts it.

use std::path::PathBuf;

/// How a linker setting names the built-in linker.
pub const EMBEDDED_PREFIX: &str = "rune:hyprlnk";

/// Set on the compiler so that the Rune executable it runs as its linker
/// links instead of reading a Rune command line (`hyprlnk::EMBEDDED_ENV`).
pub const EMBEDDED_ENV: &str = "RUNE_HYPRLNK";

/// Whether `linker` (a unit's linker setting) is the built-in one.
pub fn is_embedded(linker: &str) -> bool {
    linker.starts_with(EMBEDDED_PREFIX)
}

/// The linker setting for the built-in linker at `version`.
pub fn embedded(version: &str) -> String {
    format!("{EMBEDDED_PREFIX}@{version}")
}

/// What to pass as `-C linker=` for a unit's linker setting, and the variable
/// to set on the compiler, if any.
pub fn for_rustc(linker: &str) -> (PathBuf, Option<(&'static str, &'static str)>) {
    if is_embedded(linker) {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rune"));
        (exe, Some((EMBEDDED_ENV, "1")))
    } else {
        (PathBuf::from(linker), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_linker_round_trip() {
        let l = embedded("0.1.0");
        assert_eq!(l, "rune:hyprlnk@0.1.0");
        assert!(is_embedded(&l));
        assert!(!is_embedded("rust-lld"));
        let (exe, env) = for_rustc(&l);
        assert_eq!(exe, std::env::current_exe().unwrap());
        assert_eq!(env, Some((EMBEDDED_ENV, "1")));
        assert_eq!(for_rustc("rust-lld"), (PathBuf::from("rust-lld"), None));
    }
}
