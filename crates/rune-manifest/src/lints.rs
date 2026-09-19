//! `[lints]` tables → `rustc` lint flags.
//!
//! Cargo turns `[lints.rust]`, `[lints.clippy]` and `[lints.rustdoc]` (or
//! `[lints] workspace = true`, inheriting `[workspace.lints]`) into
//! `-A/-W/-D/-F` flags. Ignoring them silently changes what a crate accepts —
//! `unsafe_code = "forbid"` would stop being enforced — so they are honored
//! for local packages.

/// Lint flags plus the `--check-cfg` values a `[lints.rust]` table declares.
pub fn lint_args(own: Option<&toml::Value>, workspace: Option<&toml::Value>) -> Vec<String> {
    let table = match own.and_then(|v| v.as_table()) {
        Some(t) if t.get("workspace").and_then(|v| v.as_bool()) == Some(true) => {
            workspace.and_then(|v| v.as_table())
        }
        Some(t) => Some(t),
        None => None,
    };
    let Some(table) = table else {
        return Vec::new();
    };

    struct Lint {
        priority: i64,
        name: String,
        flag: &'static str,
    }
    let mut lints: Vec<Lint> = Vec::new();
    let mut check_cfgs: Vec<String> = Vec::new();

    for (tool, group) in table {
        if tool == "workspace" {
            continue;
        }
        let Some(group) = group.as_table() else {
            continue;
        };
        for (name, spec) in group {
            let (level, priority) = match spec {
                toml::Value::String(level) => (level.as_str(), 0),
                toml::Value::Table(t) => (
                    t.get("level").and_then(|v| v.as_str()).unwrap_or("warn"),
                    t.get("priority").and_then(|v| v.as_integer()).unwrap_or(0),
                ),
                _ => continue,
            };
            if let Some(arr) = spec
                .as_table()
                .and_then(|t| t.get("check-cfg"))
                .and_then(|v| v.as_array())
            {
                check_cfgs.extend(arr.iter().filter_map(|v| v.as_str()).map(str::to_string));
            }
            let flag = match level {
                "allow" => "-A",
                "warn" => "-W",
                "deny" => "-D",
                "forbid" => "-F",
                "force-warn" => "--force-warn",
                _ => continue,
            };
            let full = if tool == "rust" {
                name.clone()
            } else {
                format!("{tool}::{name}")
            };
            lints.push(Lint {
                priority,
                name: full,
                flag,
            });
        }
    }

    // Lower priority first, so higher-priority flags come later and win; name
    // order makes the result deterministic.
    lints.sort_by(|a, b| (a.priority, &a.name).cmp(&(b.priority, &b.name)));
    let mut out: Vec<String> = Vec::new();
    for l in lints {
        out.push(l.flag.to_string());
        out.push(l.name);
    }
    check_cfgs.sort();
    check_cfgs.dedup();
    for c in check_cfgs {
        out.push("--check-cfg".into());
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> toml::Value {
        toml::from_str::<toml::Value>(s).unwrap()
    }

    #[test]
    fn levels_tools_and_priorities() {
        let doc = parse(
            r#"
            [rust]
            unsafe_code = "forbid"
            unused = { level = "warn", priority = -1 }
            [clippy]
            pedantic = { level = "warn", priority = -1 }
            unwrap_used = "deny"
            "#,
        );
        let args = lint_args(Some(&doc), None);
        assert_eq!(
            args,
            [
                "-W",
                "clippy::pedantic",
                "-W",
                "unused", // priority -1, sorted by name
                "-D",
                "clippy::unwrap_used",
                "-F",
                "unsafe_code",
            ]
        );
    }

    #[test]
    fn workspace_inheritance() {
        let ws = parse("[rust]\nmissing_docs = \"warn\"\n");
        let member = parse("workspace = true\n");
        assert_eq!(lint_args(Some(&member), Some(&ws)), ["-W", "missing_docs"]);
        assert!(lint_args(Some(&member), None).is_empty());
        assert!(
            lint_args(None, Some(&ws)).is_empty(),
            "no [lints] table, no inheritance"
        );
    }

    #[test]
    fn check_cfg_is_forwarded() {
        let doc = parse(
            r#"
            [rust]
            unexpected_cfgs = { level = "warn", check-cfg = ['cfg(foo)', 'cfg(bar)'] }
            "#,
        );
        let args = lint_args(Some(&doc), None);
        assert_eq!(
            args,
            [
                "-W",
                "unexpected_cfgs",
                "--check-cfg",
                "cfg(bar)",
                "--check-cfg",
                "cfg(foo)"
            ]
        );
    }
}
