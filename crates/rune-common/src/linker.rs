//! The built-in linker, hyprlnk. Its cache-key identity is `rune:hyprlnk@<version>`; only when rustc runs
//! does it become `-C linker=<this executable>` with [`EMBEDDED_ENV`] set.

use std::path::PathBuf;

pub const EMBEDDED_PREFIX: &str = "rune:hyprlnk";

pub const EMBEDDED_ENV: &str = "RUNE_HYPRLNK";

pub fn is_embedded(linker: &str) -> bool {
    linker.starts_with(EMBEDDED_PREFIX)
}

pub fn embedded(version: &str) -> String {
    format!("{EMBEDDED_PREFIX}@{version}")
}

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
