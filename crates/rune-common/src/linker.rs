//! The built-in linker, hyprlnk. Its cache-key identity is `rune:hyprlnk@<version>` (with, for
//! Linux targets, the C compiler it links through); only when rustc runs does it become
//! `-C linker=<this executable>` with [`EMBEDDED_ENV`] set, plus the linker flavor for targets
//! whose default flavor isn't the linker's own command line.

use std::path::{Path, PathBuf};

pub const EMBEDDED_PREFIX: &str = "rune:hyprlnk";

pub const EMBEDDED_ENV: &str = "RUNE_HYPRLNK";

/// The C compiler hyprlnk runs for rustc's `cc` command line on Linux (it finds the C runtime;
/// hyprlnk is its `ld`). Must match `hyprlnk::CC_ENV`.
pub const DRIVER_ENV: &str = "HYPRLNK_CC";

const DRIVER_SEP: &str = "+cc=";

pub fn is_embedded(linker: &str) -> bool {
    linker.starts_with(EMBEDDED_PREFIX)
}

/// The embedded linker's identity; `driver` is the C compiler it links through, when the target
/// configures one (`target.<triple>.linker = "aarch64-linux-gnu-gcc"`).
pub fn embedded(version: &str, driver: Option<&str>) -> String {
    match driver {
        Some(d) => format!("{EMBEDDED_PREFIX}@{version}{DRIVER_SEP}{d}"),
        None => format!("{EMBEDDED_PREFIX}@{version}"),
    }
}

/// The linker rustc runs, and the environment it needs.
pub fn for_rustc(linker: &str) -> (PathBuf, Vec<(&'static str, String)>) {
    if is_embedded(linker) {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rune"));
        let mut env = vec![(EMBEDDED_ENV, "1".to_string())];
        if let Some((_, driver)) = linker.split_once(DRIVER_SEP) {
            env.push((DRIVER_ENV, driver.to_string()));
        }
        (exe, env)
    } else {
        (PathBuf::from(linker), Vec::new())
    }
}

/// Whether a configured `linker` is a C compiler hyprlnk can link through, not a linker itself
/// (`rust-lld`, `ld.lld`, `mold`…), which hyprlnk replaces.
pub fn is_c_compiler(linker: &Path) -> bool {
    let name = linker
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    !(name == "ld"
        || name.starts_with("ld.")
        || name.ends_with("-ld")
        || name.contains("-ld.")
        || name.contains("lld")
        || matches!(name, "mold" | "wild" | "gold" | "hyprlnk" | "rune"))
}

/// `-C linker-flavor` for the embedded linker on `target`: Apple targets would otherwise get a
/// `cc` command line; with `ld64.lld` rustc passes ld64's (and `-flavor darwin` first).
pub fn rustc_flavor(linker: &str, target: &str) -> Option<&'static str> {
    (is_embedded(linker) && target.contains("-apple-")).then_some("ld64.lld")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_linker_round_trip() {
        let l = embedded("0.1.0", None);
        assert_eq!(l, "rune:hyprlnk@0.1.0");
        assert!(is_embedded(&l));
        assert!(!is_embedded("rust-lld"));
        let (exe, env) = for_rustc(&l);
        assert_eq!(exe, std::env::current_exe().unwrap());
        assert_eq!(env, vec![(EMBEDDED_ENV, "1".to_string())]);
        assert_eq!(for_rustc("rust-lld"), (PathBuf::from("rust-lld"), vec![]));
    }

    #[test]
    fn the_c_compiler_travels_with_the_embedded_linker() {
        let l = embedded("0.1.0", Some("/usr/bin/aarch64-linux-gnu-gcc"));
        assert!(is_embedded(&l));
        assert_ne!(l, embedded("0.1.0", None), "part of the cache key");
        let (_, env) = for_rustc(&l);
        assert_eq!(
            env,
            vec![
                (EMBEDDED_ENV, "1".to_string()),
                (DRIVER_ENV, "/usr/bin/aarch64-linux-gnu-gcc".to_string())
            ]
        );
    }

    #[test]
    fn linkers_are_not_mistaken_for_c_compilers() {
        for c in [
            "cc",
            "gcc",
            "clang",
            "/usr/bin/aarch64-linux-gnu-gcc",
            "x86_64-linux-musl-cc",
        ] {
            assert!(is_c_compiler(Path::new(c)), "{c}");
        }
        for l in [
            "ld",
            "ld.lld",
            "rust-lld",
            "/usr/bin/mold",
            "aarch64-linux-gnu-ld",
            "aarch64-linux-gnu-ld.bfd",
            "wild",
            "lld-link.exe",
        ] {
            assert!(!is_c_compiler(Path::new(l)), "{l}");
        }
    }

    #[test]
    fn apple_targets_get_the_ld64_flavor() {
        let l = embedded("0.1.0", None);
        assert_eq!(rustc_flavor(&l, "aarch64-apple-darwin"), Some("ld64.lld"));
        assert_eq!(rustc_flavor(&l, "x86_64-apple-darwin"), Some("ld64.lld"));
        assert_eq!(rustc_flavor(&l, "x86_64-pc-windows-msvc"), None);
        assert_eq!(rustc_flavor(&l, "x86_64-unknown-linux-gnu"), None);
        assert_eq!(rustc_flavor("cc", "aarch64-apple-darwin"), None);
    }
}
