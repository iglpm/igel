//! Turns a failed build script into advice.
//!
//! The commonest reason a crate does not build on a fresh machine is a missing
//! system tool or development package (`pkg-config`, OpenSSL headers, `cmake`, a
//! C compiler, `libclang`), and the failure is a page of build-script output
//! that never says which package to install. [`hint_for_failure`] recognises
//! the usual messages and names the package for the distribution at hand.

/// The package manager family, from `/etc/os-release` (or the OS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Debian,
    Fedora,
    Arch,
    Alpine,
    Suse,
    MacOs,
    Unknown,
}

impl Family {
    /// Reads `ID` and `ID_LIKE` from the text of `/etc/os-release`.
    pub fn from_os_release(text: &str) -> Self {
        let mut ids = Vec::new();
        for line in text.lines() {
            if let Some(v) = line
                .strip_prefix("ID=")
                .or_else(|| line.strip_prefix("ID_LIKE="))
            {
                ids.extend(
                    v.trim_matches('"')
                        .split_whitespace()
                        .map(str::to_lowercase),
                );
            }
        }
        let has = |names: &[&str]| ids.iter().any(|i| names.contains(&i.as_str()));
        if has(&["debian", "ubuntu"]) {
            Self::Debian
        } else if has(&["fedora", "rhel", "centos"]) {
            Self::Fedora
        } else if has(&["arch"]) {
            Self::Arch
        } else if has(&["alpine"]) {
            Self::Alpine
        } else if has(&["suse", "opensuse"]) {
            Self::Suse
        } else {
            Self::Unknown
        }
    }

    /// The family of the machine Rune is running on.
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            return Self::MacOs;
        }
        std::fs::read_to_string("/etc/os-release")
            .map(|t| Self::from_os_release(&t))
            .unwrap_or(Self::Unknown)
    }

    fn install(self, packages: &Packages) -> Option<String> {
        let (cmd, pkg) = match self {
            Self::Debian => ("sudo apt install", packages.debian),
            Self::Fedora => ("sudo dnf install", packages.fedora),
            Self::Arch => ("sudo pacman -S", packages.arch),
            Self::Alpine => ("sudo apk add", packages.alpine),
            Self::Suse => ("sudo zypper install", packages.suse),
            Self::MacOs => ("brew install", packages.brew),
            Self::Unknown => return None,
        };
        (!pkg.is_empty()).then(|| format!("{cmd} {pkg}"))
    }
}

struct Packages {
    debian: &'static str,
    fedora: &'static str,
    arch: &'static str,
    alpine: &'static str,
    suse: &'static str,
    brew: &'static str,
}

struct Rule {
    /// What is missing, for the sentence.
    what: &'static str,
    /// Every one of these groups must match; a group matches if any of its
    /// (lowercase) needles occurs in the text.
    when: &'static [&'static [&'static str]],
    packages: Packages,
    /// An extra line, e.g. an environment variable to set.
    note: Option<&'static str>,
}

const RULES: &[Rule] = &[
    Rule {
        what: "a C compiler",
        when: &[&[
            "failed to find tool",
            "linker `cc` not found",
            "error: linker `cc`",
            "is `cc` installed",
            "is `gcc` installed",
            "is `clang` installed",
            "failed to execute command: no such file or directory\n\n    is `cc`",
        ]],
        packages: Packages {
            debian: "build-essential",
            fedora: "gcc",
            arch: "base-devel",
            alpine: "build-base",
            suse: "gcc",
            brew: "",
        },
        note: Some("on macOS: xcode-select --install"),
    },
    Rule {
        what: "`pkg-config`",
        when: &[
            &["pkg-config"],
            &[
                "not found",
                "no such file or directory",
                "not installed",
                "failed to execute",
                "could not run",
            ],
        ],
        packages: Packages {
            debian: "pkg-config",
            fedora: "pkgconf-pkg-config",
            arch: "pkgconf",
            alpine: "pkgconf",
            suse: "pkg-config",
            brew: "pkg-config",
        },
        note: None,
    },
    Rule {
        what: "the OpenSSL development files",
        when: &[&[
            "could not find directory of openssl installation",
            "package openssl was not found",
            "openssl.pc",
            "openssl_dir",
            "failed to find openssl",
        ]],
        packages: Packages {
            debian: "libssl-dev pkg-config",
            fedora: "openssl-devel",
            arch: "openssl pkgconf",
            alpine: "openssl-dev",
            suse: "libopenssl-devel",
            brew: "openssl@3",
        },
        note: Some(
            "or point OPENSSL_DIR at an installation, or use a crate feature that vendors OpenSSL",
        ),
    },
    Rule {
        what: "`cmake`",
        when: &[
            &["cmake"],
            &[
                "is `cmake` not installed",
                "failed to execute command: no such file or directory",
                "cmake: not found",
                "cmake: command not found",
            ],
        ],
        packages: Packages {
            debian: "cmake",
            fedora: "cmake",
            arch: "cmake",
            alpine: "cmake",
            suse: "cmake",
            brew: "cmake",
        },
        note: None,
    },
    Rule {
        what: "`libclang`",
        when: &[&[
            "unable to find libclang",
            "couldn't find any valid shared libraries matching",
            "libclang.so",
        ]],
        packages: Packages {
            debian: "libclang-dev",
            fedora: "clang-devel",
            arch: "clang",
            alpine: "clang-dev",
            suse: "clang-devel",
            brew: "llvm",
        },
        note: Some("or set LIBCLANG_PATH to the directory containing libclang"),
    },
    Rule {
        what: "the Protocol Buffers compiler",
        when: &[
            &["protoc"],
            &["not found", "could not find `protoc`", "no such file"],
        ],
        packages: Packages {
            debian: "protobuf-compiler",
            fedora: "protobuf-compiler",
            arch: "protobuf",
            alpine: "protobuf-dev",
            suse: "protobuf-devel",
            brew: "protobuf",
        },
        note: Some("or set PROTOC to the path of a `protoc` binary"),
    },
];

/// Advice for a failed build script or C compilation, given everything it
/// printed. `None` when nothing recognisable is in it.
pub fn hint_for_failure(output: &str, family: Family) -> Option<String> {
    let lower = output.to_lowercase();

    // The pkg-config search failing for one specific library is a different
    // problem from pkg-config being absent, and the message names the file.
    if let Some(pc) = missing_pkg_config_package(&lower) {
        if !lower.contains("openssl") || !lower.contains("could not find directory of openssl") {
            let mut hint = format!(
                "hint: a system library is missing: pkg-config found no `{pc}.pc`. \
                 Install the development package that provides it"
            );
            hint.push_str(match family {
                Family::Debian => {
                    " (apt-file search {pc}.pc  finds its name; usually `lib<name>-dev`)."
                }
                Family::Fedora => " (dnf provides '*/{pc}.pc'; usually `<name>-devel`).",
                Family::Arch => " (pacman -F {pc}.pc).",
                _ => ".",
            });
            return Some(hint.replace("{pc}", &pc));
        }
    }

    RULES
        .iter()
        .find(|rule| {
            rule.when
                .iter()
                .all(|group| group.iter().any(|needle| lower.contains(needle)))
        })
        .map(|rule| {
            let mut hint = format!("hint: this looks like {} is missing.", rule.what);
            if let Some(cmd) = family.install(&rule.packages) {
                hint.push_str(&format!(" Install it with:\n    {cmd}"));
            }
            if let Some(note) = rule.note {
                hint.push_str(&format!("\n    ({note})"));
            }
            hint
        })
}

/// `Package foo was not found in the pkg-config search path` → `foo`.
fn missing_pkg_config_package(lower: &str) -> Option<String> {
    let start = lower.find("package ")? + "package ".len();
    let rest = &lower[start..];
    let end = rest.find(" was not found")?;
    let name = rest[..end].trim().trim_matches(['\'', '`', '"']);
    (!name.is_empty() && !name.contains(char::is_whitespace)).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEBIAN: Family = Family::Debian;

    #[test]
    fn distributions_are_recognised_from_os_release() {
        assert_eq!(
            Family::from_os_release("ID=ubuntu\nID_LIKE=debian\n"),
            Family::Debian
        );
        assert_eq!(
            Family::from_os_release("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n"),
            Family::Debian
        );
        assert_eq!(Family::from_os_release("ID=fedora\n"), Family::Fedora);
        assert_eq!(
            Family::from_os_release("ID=cachyos\nID_LIKE=arch\n"),
            Family::Arch
        );
        assert_eq!(Family::from_os_release("ID=alpine\n"), Family::Alpine);
        assert_eq!(
            Family::from_os_release("ID=opensuse-tumbleweed\nID_LIKE=\"opensuse suse\"\n"),
            Family::Suse
        );
        assert_eq!(Family::from_os_release("ID=plan9\n"), Family::Unknown);
    }

    #[test]
    fn a_missing_c_compiler_names_the_package_per_distribution() {
        let out = "error occurred: Failed to find tool. Is `cc` installed?";
        assert!(
            hint_for_failure(out, DEBIAN)
                .unwrap()
                .contains("sudo apt install build-essential")
        );
        assert!(
            hint_for_failure(out, Family::Arch)
                .unwrap()
                .contains("pacman -S base-devel")
        );
        assert!(
            hint_for_failure(out, Family::Fedora)
                .unwrap()
                .contains("dnf install gcc")
        );
        assert!(
            hint_for_failure(out, Family::MacOs)
                .unwrap()
                .contains("xcode-select --install")
        );
    }

    #[test]
    fn a_missing_pkg_config_binary_is_recognised() {
        let out = "Could not run `\"pkg-config\" \"--libs\" \"zlib\"`\nThe pkg-config command could not be found.\nNo such file or directory";
        let h = hint_for_failure(out, DEBIAN).unwrap();
        assert!(
            h.contains("`pkg-config`") && h.contains("apt install pkg-config"),
            "{h}"
        );
    }

    #[test]
    fn a_missing_library_is_told_apart_from_a_missing_pkg_config() {
        let out = "Package zlib was not found in the pkg-config search path.\nPerhaps you should add the directory containing `zlib.pc'";
        let h = hint_for_failure(out, DEBIAN).unwrap();
        assert!(h.contains("no `zlib.pc`"), "{h}");
        assert!(h.contains("apt-file search zlib.pc"), "{h}");
    }

    #[test]
    fn openssl_gets_the_development_package_and_the_env_var_escape_hatch() {
        let out = "Could not find directory of OpenSSL installation, and this `-sys` crate cannot proceed without this knowledge.";
        let h = hint_for_failure(out, DEBIAN).unwrap();
        assert!(h.contains("libssl-dev"), "{h}");
        assert!(h.contains("OPENSSL_DIR"), "{h}");
        assert!(
            hint_for_failure(out, Family::Fedora)
                .unwrap()
                .contains("openssl-devel")
        );
    }

    #[test]
    fn cmake_libclang_and_protoc_are_recognised() {
        let cmake = "is `cmake` not installed?";
        assert!(
            hint_for_failure(cmake, DEBIAN)
                .unwrap()
                .contains("apt install cmake")
        );
        let clang = "thread 'main' panicked: Unable to find libclang: \"couldn't find any valid shared libraries matching: ['libclang.so']\"";
        assert!(
            hint_for_failure(clang, DEBIAN)
                .unwrap()
                .contains("libclang-dev")
        );
        let protoc = "Could not find `protoc`. If `protoc` is installed, try setting the `PROTOC` environment variable";
        assert!(
            hint_for_failure(protoc, DEBIAN)
                .unwrap()
                .contains("protobuf-compiler")
        );
    }

    #[test]
    fn unrelated_failures_get_no_hint() {
        assert!(hint_for_failure("error[E0308]: mismatched types", DEBIAN).is_none());
        assert!(hint_for_failure("thread 'main' panicked at build.rs:3: boom", DEBIAN).is_none());
    }

    #[test]
    fn an_unknown_distribution_still_says_what_is_missing() {
        let h = hint_for_failure("Is `cc` installed?", Family::Unknown).unwrap();
        assert!(h.contains("a C compiler"));
        assert!(!h.contains("sudo"));
    }
}
