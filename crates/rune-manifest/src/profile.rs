//! Resolution of `[profile.*]` tables into concrete [`Profile`]s, as in Cargo.

use rune_common::{DebugInfo, LtoMode, OptLevel, PanicStrategy, Profile};
use std::collections::HashMap;

use crate::cargo_toml::ProfileOverride;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProfileError {
    #[error(
        "profile `{0}` is not defined; custom profiles need `inherits = \"dev\"` or `\"release\"`"
    )]
    MissingInherits(String),
    #[error("profile `{0}` inherits from itself (directly or through a cycle)")]
    Cycle(String),
    #[error("invalid `{key}` in profile `{profile}`: {message}")]
    Invalid {
        profile: String,
        key: String,
        message: String,
    },
}

pub fn resolve_profile(
    name: &str,
    tables: &HashMap<String, ProfileOverride>,
) -> Result<Profile, ProfileError> {
    resolve_inner(name, tables, &mut Vec::new())
}

fn resolve_inner(
    name: &str,
    tables: &HashMap<String, ProfileOverride>,
    stack: &mut Vec<String>,
) -> Result<Profile, ProfileError> {
    if stack.iter().any(|n| n == name) {
        return Err(ProfileError::Cycle(name.to_string()));
    }
    stack.push(name.to_string());

    let table = tables.get(name);
    let base_name = match name {
        "dev" | "release" => None,
        "test" => Some("dev".to_string()),
        "bench" => Some("release".to_string()),
        _ => {
            let inherits = table
                .and_then(|t| t.extra.get("inherits"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| ProfileError::MissingInherits(name.to_string()))?;
            Some(inherits.to_string())
        }
    };
    let base_name = table
        .and_then(|t| t.extra.get("inherits"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or(base_name);

    let mut profile = match base_name {
        Some(parent) => resolve_inner(&parent, tables, stack)?,
        None if name == "release" => Profile::release(),
        None => Profile::dev(),
    };
    profile.name = name.to_string();

    if let Some(table) = table {
        apply(&mut profile, name, table)?;
    }
    stack.pop();
    Ok(profile)
}

#[derive(Debug, Clone, Copy)]
pub struct UnitRole<'a> {
    pub package: &'a str,
    pub version: &'a semver::Version,
    pub workspace_member: bool,
    pub host: bool,
    /// A registry/git dependency: in dev, use `line-tables-only` debuginfo unless a profile says otherwise.
    pub lean_debuginfo: bool,
}

/// The named profile plus, in Cargo's precedence, `build-override`, `package."*"` and `package.<name>`.
pub fn resolve_unit_profile(
    name: &str,
    tables: &HashMap<String, ProfileOverride>,
    role: UnitRole<'_>,
) -> Result<Profile, ProfileError> {
    let mut profile = resolve_profile(name, tables)?;

    // Cargo's built-in `build-override`: host units are compiled fast, not well.
    if role.host {
        profile.opt_level = OptLevel::O0;
        profile.debuginfo = DebugInfo::None;
        profile.codegen_units = Some(256);
        profile.lto = LtoMode::Off;
        profile.strip = None;
    }

    let mut chain = vec![name.to_string()];
    let mut cur = name.to_string();
    while let Some(parent) = tables
        .get(&cur)
        .and_then(|t| t.extra.get("inherits"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| match cur.as_str() {
            "test" => Some("dev".to_string()),
            "bench" => Some("release".to_string()),
            _ => None,
        })
    {
        if chain.contains(&parent) {
            break;
        }
        chain.insert(0, parent.clone());
        cur = parent;
    }

    let explicit_debug = chain
        .iter()
        .any(|level| tables.get(level).is_some_and(|t| t.debug.is_some()));
    if role.lean_debuginfo
        && !role.host
        && !explicit_debug
        && chain.first().is_some_and(|root| root == "dev")
        && profile.debuginfo == DebugInfo::Full
    {
        profile.debuginfo = DebugInfo::LineTablesOnly;
    }

    let matches_spec = |spec: &str| {
        spec == role.package
            || spec
                .split_once('@')
                .is_some_and(|(n, v)| n == role.package && v == role.version.to_string())
    };
    for level in &chain {
        let Some(table) = tables.get(level) else {
            continue;
        };
        if role.host
            && let Some(over) = &table.build_override
        {
            apply(&mut profile, name, over)?;
        }
        if !role.workspace_member
            && let Some(over) = table.package.get("*")
        {
            apply(&mut profile, name, over)?;
        }
        for (spec, over) in &table.package {
            if spec != "*" && matches_spec(spec) {
                apply(&mut profile, name, over)?;
            }
        }
    }
    Ok(profile)
}

fn apply(profile: &mut Profile, name: &str, table: &ProfileOverride) -> Result<(), ProfileError> {
    let invalid = |key: &str, message: &str| ProfileError::Invalid {
        profile: name.to_string(),
        key: key.to_string(),
        message: message.to_string(),
    };

    if let Some(v) = &table.opt_level {
        profile.opt_level = match v {
            toml::Value::Integer(0) => OptLevel::O0,
            toml::Value::Integer(1) => OptLevel::O1,
            toml::Value::Integer(2) => OptLevel::O2,
            toml::Value::Integer(3) => OptLevel::O3,
            toml::Value::String(s) if s == "s" => OptLevel::Os,
            toml::Value::String(s) if s == "z" => OptLevel::Oz,
            toml::Value::String(s) if s == "0" => OptLevel::O0,
            toml::Value::String(s) if s == "1" => OptLevel::O1,
            toml::Value::String(s) if s == "2" => OptLevel::O2,
            toml::Value::String(s) if s == "3" => OptLevel::O3,
            _ => return Err(invalid("opt-level", "expected 0, 1, 2, 3, \"s\" or \"z\"")),
        };
    }

    if let Some(v) = &table.debug {
        profile.debuginfo = match v {
            toml::Value::Boolean(true) => DebugInfo::Full,
            toml::Value::Boolean(false) => DebugInfo::None,
            toml::Value::Integer(0) => DebugInfo::None,
            toml::Value::Integer(1) => DebugInfo::Limited,
            toml::Value::Integer(2) => DebugInfo::Full,
            toml::Value::String(s) => match s.as_str() {
                "none" => DebugInfo::None,
                "line-tables-only" | "line-directives-only" => DebugInfo::LineTablesOnly,
                "limited" => DebugInfo::Limited,
                "full" => DebugInfo::Full,
                _ => return Err(invalid("debug", "unknown debug level")),
            },
            _ => return Err(invalid("debug", "expected a bool, 0, 1, 2 or a level name")),
        };
    }

    for (key, value) in &table.extra {
        match key.as_str() {
            "debug-assertions" => {
                profile.debug_assertions = value
                    .as_bool()
                    .ok_or_else(|| invalid(key, "expected a bool"))?;
            }
            "overflow-checks" => {
                profile.overflow_checks = value
                    .as_bool()
                    .ok_or_else(|| invalid(key, "expected a bool"))?;
            }
            "incremental" => {
                profile.incremental = value
                    .as_bool()
                    .ok_or_else(|| invalid(key, "expected a bool"))?;
            }
            "rpath" => {
                profile.rpath = value
                    .as_bool()
                    .ok_or_else(|| invalid(key, "expected a bool"))?;
            }
            "lto" => {
                profile.lto = match value {
                    toml::Value::Boolean(true) => LtoMode::Fat,
                    toml::Value::Boolean(false) => LtoMode::Off,
                    toml::Value::String(s) => match s.as_str() {
                        "fat" => LtoMode::Fat,
                        "thin" => LtoMode::Thin,
                        "off" | "false" => LtoMode::Off,
                        _ => {
                            return Err(invalid(
                                key,
                                "expected true, false, \"thin\", \"fat\" or \"off\"",
                            ));
                        }
                    },
                    _ => {
                        return Err(invalid(
                            key,
                            "expected true, false, \"thin\", \"fat\" or \"off\"",
                        ));
                    }
                };
            }
            "panic" => {
                profile.panic = match value.as_str() {
                    Some("unwind") => PanicStrategy::Unwind,
                    Some("abort") => PanicStrategy::Abort,
                    _ => return Err(invalid(key, "expected \"unwind\" or \"abort\"")),
                };
            }
            "codegen-units" => {
                profile.codegen_units = Some(
                    value
                        .as_integer()
                        .and_then(|n| u32::try_from(n).ok())
                        .filter(|n| *n > 0)
                        .ok_or_else(|| invalid(key, "expected a positive integer"))?,
                );
            }
            "strip" => {
                profile.strip = match value {
                    toml::Value::Boolean(true) => Some("symbols".to_string()),
                    toml::Value::Boolean(false) => Some("none".to_string()),
                    toml::Value::String(s)
                        if matches!(s.as_str(), "none" | "debuginfo" | "symbols") =>
                    {
                        Some(s.clone())
                    }
                    _ => {
                        return Err(invalid(
                            key,
                            "expected a bool, \"none\", \"debuginfo\" or \"symbols\"",
                        ));
                    }
                };
            }
            "split-debuginfo" => {
                profile.split_debuginfo = match value.as_str() {
                    Some(s @ ("off" | "packed" | "unpacked")) => Some(s.to_string()),
                    _ => {
                        return Err(invalid(key, "expected \"off\", \"packed\" or \"unpacked\""));
                    }
                };
            }
            "trim-paths" => {
                // Any scope means all of them: stable rustc can't trim only some.
                const SCOPES: [&str; 5] = ["macro", "diagnostics", "object", "all", "none"];
                let on = match value {
                    toml::Value::Boolean(b) => *b,
                    toml::Value::String(s) if SCOPES.contains(&s.as_str()) => s != "none",
                    toml::Value::Array(a)
                        if a.iter()
                            .all(|v| v.as_str().is_some_and(|s| SCOPES.contains(&s))) =>
                    {
                        a.iter().any(|v| v.as_str() != Some("none"))
                    }
                    _ => {
                        return Err(invalid(
                            key,
                            "expected a bool, \"none\", \"macro\", \"diagnostics\", \"object\", \"all\" or a list of them",
                        ));
                    }
                };
                profile.trim_paths = on;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Manifest;
    use std::path::Path;

    fn profiles(toml: &str) -> HashMap<String, ProfileOverride> {
        Manifest::from_str(toml, Path::new("Cargo.toml"))
            .unwrap()
            .profile
    }

    #[test]
    fn builtin_profiles_match_cargo_defaults() {
        let none = HashMap::new();
        let dev = resolve_profile("dev", &none).unwrap();
        assert_eq!(dev.opt_level, OptLevel::O0);
        assert!(dev.debug_assertions && dev.overflow_checks && dev.incremental);

        let rel = resolve_profile("release", &none).unwrap();
        assert_eq!(rel.opt_level, OptLevel::O3);
        assert_eq!(rel.debuginfo, DebugInfo::None);
        assert!(!rel.debug_assertions);

        assert_eq!(resolve_profile("test", &none).unwrap().name, "test");
        assert_eq!(
            resolve_profile("bench", &none).unwrap().opt_level,
            OptLevel::O3
        );
    }

    #[test]
    fn release_overrides_are_applied() {
        let p = profiles(
            r#"
            [profile.release]
            opt-level = "z"
            lto = true
            codegen-units = 1
            panic = "abort"
            strip = true
            debug = 1
            "#,
        );
        let r = resolve_profile("release", &p).unwrap();
        assert_eq!(r.opt_level, OptLevel::Oz);
        assert_eq!(r.lto, LtoMode::Fat);
        assert_eq!(r.codegen_units, Some(1));
        assert_eq!(r.panic, PanicStrategy::Abort);
        assert_eq!(r.strip.as_deref(), Some("symbols"));
        assert_eq!(r.debuginfo, DebugInfo::Limited);
    }

    #[test]
    fn multiline_profile_tables_parse() {
        let p = profiles(
            r#"
            [profile.dev]
            opt-level = 1
            [profile.dev.package."*"]
            opt-level = 3
            "#,
        );
        assert_eq!(resolve_profile("dev", &p).unwrap().opt_level, OptLevel::O1);
    }

    #[test]
    fn custom_profile_inherits() {
        let p = profiles(
            r#"
            [profile.release]
            lto = "thin"
            [profile.profiling]
            inherits = "release"
            debug = true
            "#,
        );
        let prof = resolve_profile("profiling", &p).unwrap();
        assert_eq!(prof.name, "profiling");
        assert_eq!(prof.lto, LtoMode::Thin, "inherits the parent's overrides");
        assert_eq!(prof.debuginfo, DebugInfo::Full);
        assert_eq!(prof.output_dir_name(), "profiling");
    }

    #[test]
    fn package_and_build_overrides_follow_cargo_precedence() {
        let p = profiles(
            r#"
            [profile.dev]
            opt-level = 0
            [profile.dev.package."*"]
            opt-level = 2
            [profile.dev.package.hot]
            opt-level = 3
            [profile.dev.build-override]
            opt-level = 1
            debug = false
            "#,
        );
        let v = semver::Version::new(1, 0, 0);
        let role = |package, workspace_member, host| UnitRole {
            package,
            version: &v,
            workspace_member,
            host,
            lean_debuginfo: false,
        };
        let opt = |r| resolve_unit_profile("dev", &p, r).unwrap().opt_level;
        assert_eq!(opt(role("mine", true, false)), OptLevel::O0);
        assert_eq!(opt(role("dep", false, false)), OptLevel::O2);
        assert_eq!(opt(role("hot", false, false)), OptLevel::O3);
        assert_eq!(opt(role("mine", true, true)), OptLevel::O1);
        let plain = profiles("[profile.release]\nopt-level = 3\n");
        let host = resolve_unit_profile("release", &plain, role("dep", false, true)).unwrap();
        assert_eq!(host.opt_level, OptLevel::O0);
        assert_eq!(host.codegen_units, Some(256));
        let target = resolve_unit_profile("release", &plain, role("dep", false, false)).unwrap();
        assert_eq!(target.opt_level, OptLevel::O3);
        assert_eq!(opt(role("dep", false, true)), OptLevel::O2);
        let built = resolve_unit_profile("dev", &p, role("mine", true, true)).unwrap();
        assert_eq!(built.debuginfo, DebugInfo::None);
        let p2 = profiles("[profile.dev.package.\"dep@2.0.0\"]\nopt-level = 3\n");
        assert_eq!(
            resolve_unit_profile("dev", &p2, role("dep", false, false))
                .unwrap()
                .opt_level,
            OptLevel::O0
        );
    }

    #[test]
    fn split_debuginfo_is_applied_and_part_of_the_fingerprint() {
        let p = profiles("[profile.dev]\nsplit-debuginfo = \"packed\"\n");
        let dev = resolve_profile("dev", &p).unwrap();
        assert_eq!(dev.split_debuginfo.as_deref(), Some("packed"));
        assert_ne!(
            dev.codegen_fingerprint(),
            resolve_profile("dev", &HashMap::new())
                .unwrap()
                .codegen_fingerprint()
        );
        let bad = profiles("[profile.dev]\nsplit-debuginfo = \"sideways\"\n");
        assert!(resolve_profile("dev", &bad).is_err());
    }

    #[test]
    fn errors_are_specific() {
        let p = profiles("[profile.weird]\nopt-level = 2\n");
        assert_eq!(
            resolve_profile("weird", &p),
            Err(ProfileError::MissingInherits("weird".into()))
        );
        assert!(resolve_profile("nope", &HashMap::new()).is_err());

        let p = profiles("[profile.a]\ninherits = \"b\"\n[profile.b]\ninherits = \"a\"\n");
        assert!(matches!(
            resolve_profile("a", &p),
            Err(ProfileError::Cycle(_))
        ));

        let p = profiles("[profile.release]\nlto = 7\n");
        assert!(matches!(
            resolve_profile("release", &p),
            Err(ProfileError::Invalid { .. })
        ));
    }

    #[test]
    fn codegen_fingerprint_distinguishes_lto_and_panic() {
        let base = Profile::release();
        let mut lto = base.clone();
        lto.lto = LtoMode::Fat;
        let mut abort = base.clone();
        abort.panic = PanicStrategy::Abort;
        assert_ne!(base.codegen_fingerprint(), lto.codegen_fingerprint());
        assert_ne!(base.codegen_fingerprint(), abort.codegen_fingerprint());

        let mut renamed = base.clone();
        renamed.name = "custom".into();
        assert_eq!(base.codegen_fingerprint(), renamed.codegen_fingerprint());
    }

    fn debuginfo_of(
        manifest: &str,
        profile: &str,
        lean: bool,
        member: bool,
        host: bool,
    ) -> DebugInfo {
        let p = profiles(manifest);
        let v = semver::Version::new(1, 0, 0);
        let role = UnitRole {
            package: "dep",
            version: &v,
            workspace_member: member,
            host,
            lean_debuginfo: lean,
        };
        resolve_unit_profile(profile, &p, role).unwrap().debuginfo
    }

    #[test]
    fn dependencies_default_to_line_tables_in_dev() {
        assert_eq!(
            debuginfo_of("", "dev", true, false, false),
            DebugInfo::LineTablesOnly
        );
        assert_eq!(
            debuginfo_of("", "test", true, false, false),
            DebugInfo::LineTablesOnly
        );
        assert_eq!(
            debuginfo_of("", "dev", false, false, false),
            DebugInfo::Full
        );
        assert_eq!(
            debuginfo_of("", "release", true, false, false),
            DebugInfo::None
        );
    }

    #[test]
    fn an_explicit_debug_setting_always_wins() {
        for manifest in [
            "[profile.dev]\ndebug = true\n",
            "[profile.dev]\ndebug = 2\n",
            "[profile.dev.package.\"*\"]\ndebug = true\n",
            "[profile.dev.package.dep]\ndebug = \"full\"\n",
        ] {
            assert_eq!(
                debuginfo_of(manifest, "dev", true, false, false),
                DebugInfo::Full,
                "{manifest}"
            );
        }
        assert_eq!(
            debuginfo_of(
                "[profile.dev.package.\"*\"]\ndebug = false\n",
                "dev",
                true,
                false,
                false
            ),
            DebugInfo::None
        );
    }

    #[test]
    fn a_custom_profile_inheriting_dev_is_trimmed_unless_it_sets_debug() {
        let inherits = "[profile.custom]\ninherits = \"dev\"\n";
        assert_eq!(
            debuginfo_of(inherits, "custom", true, false, false),
            DebugInfo::LineTablesOnly
        );
        let explicit = "[profile.custom]\ninherits = \"dev\"\ndebug = true\n";
        assert_eq!(
            debuginfo_of(explicit, "custom", true, false, false),
            DebugInfo::Full
        );
        let dev_explicit = "[profile.dev]\ndebug = true\n[profile.custom]\ninherits = \"dev\"\n";
        assert_eq!(
            debuginfo_of(dev_explicit, "custom", true, false, false),
            DebugInfo::Full
        );
    }

    #[test]
    fn host_units_keep_their_own_defaults() {
        assert_eq!(debuginfo_of("", "dev", true, false, true), DebugInfo::None);
    }
}
