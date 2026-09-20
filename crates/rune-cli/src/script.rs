//! Single-file scripts: `rune script hello.rs`.
//!
//! A script is one `.rs` file that may start with a shebang line and a
//! frontmatter block holding a manifest fragment:
//!
//! ```text
//! #!/usr/bin/env -S rune script
//! ---cargo
//! [dependencies]
//! serde_json = "1"
//! ---
//! fn main() { println!("{}", serde_json::json!({"hello": "world"})); }
//! ```
//!
//! Rune turns it into a throwaway package under `$RUNE_HOME/scripts/` and runs
//! that through the normal pipeline, so compiled dependencies come from the
//! content-addressed store: the second run of any script, and the first run of a
//! script whose dependencies another project already built, takes milliseconds.

use std::path::Path;

/// A script split into its manifest fragment and its Rust source.
#[derive(Debug, PartialEq, Eq)]
pub struct Script {
    /// TOML from the frontmatter (empty if there is none).
    pub manifest: String,
    /// The source with the frontmatter blanked out (not removed: line numbers in
    /// errors and panics still match the file).
    pub source: String,
}

/// Splits a script into manifest and source.
pub fn parse(text: &str) -> Result<Script, String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0;
    if lines
        .first()
        .is_some_and(|l| l.starts_with("#!") && !l.starts_with("#!["))
    {
        i = 1;
    }
    // Blank lines may precede the frontmatter.
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    let Some(open) = lines.get(i).map(|l| l.trim_end()) else {
        return Ok(Script {
            manifest: String::new(),
            source: text.to_string(),
        });
    };
    let fence_len = open.chars().take_while(|&c| c == '-').count();
    if fence_len < 3 {
        return Ok(Script {
            manifest: String::new(),
            source: text.to_string(),
        });
    }
    let info = open[fence_len..].trim();
    if !info.is_empty() && info != "cargo" {
        return Err(format!(
            "unsupported frontmatter type `{info}` (only `---cargo`, or a bare `---`, is understood)"
        ));
    }
    let fence = "-".repeat(fence_len);
    let close = lines[i + 1..]
        .iter()
        .position(|l| l.trim_end() == fence)
        .map(|p| p + i + 1)
        .ok_or_else(|| {
            format!("the `{open}` frontmatter is never closed (expected a line with `{fence}`)")
        })?;

    let manifest = lines[i + 1..close].join("\n");
    let mut source: Vec<&str> = lines.clone();
    for line in &mut source[i..=close] {
        *line = "";
    }
    Ok(Script {
        manifest,
        source: source.join("\n"),
    })
}

/// A valid package name derived from the file name.
pub fn package_name(file: &Path) -> String {
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut name: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty()
        || !name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        name.insert_str(0, "script_");
    }
    name
}

/// The `Cargo.toml` for a script: the frontmatter, completed with what a
/// package needs, standing alone (never picked up by an enclosing workspace).
pub fn manifest(file: &Path, frontmatter: &str) -> Result<String, String> {
    let mut table: toml::Table = if frontmatter.trim().is_empty() {
        toml::Table::new()
    } else {
        frontmatter
            .parse()
            .map_err(|e| format!("invalid manifest in the frontmatter: {e}"))?
    };
    let name = package_name(file);
    let package = table
        .entry("package")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or("`package` in the frontmatter must be a table")?;
    package
        .entry("name")
        .or_insert(toml::Value::String(name.clone()));
    package
        .entry("version")
        .or_insert(toml::Value::String("0.0.0".into()));
    package
        .entry("edition")
        .or_insert(toml::Value::String("2021".into()));
    package
        .entry("publish")
        .or_insert(toml::Value::Boolean(false));
    let pkg_name = package["name"].as_str().unwrap_or(&name).to_string();

    let mut bin = toml::Table::new();
    bin.insert("name".into(), toml::Value::String(pkg_name));
    bin.insert("path".into(), toml::Value::String("src/main.rs".into()));
    table.insert(
        "bin".into(),
        toml::Value::Array(vec![toml::Value::Table(bin)]),
    );
    table.insert("workspace".into(), toml::Value::Table(toml::Table::new()));
    toml::to_string(&table).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_script_without_frontmatter_is_just_source() {
        let s = parse("fn main() {}\n").unwrap();
        assert!(s.manifest.is_empty());
        assert_eq!(s.source, "fn main() {}\n");
    }

    #[test]
    fn frontmatter_is_extracted_and_blanked_so_line_numbers_survive() {
        let text = "#!/usr/bin/env -S rune script\n---cargo\n[dependencies]\nserde = \"1\"\n---\nfn main() {\n    panic!();\n}\n";
        let s = parse(text).unwrap();
        assert_eq!(s.manifest, "[dependencies]\nserde = \"1\"");
        assert_eq!(s.source.lines().count(), text.lines().count());
        assert_eq!(
            s.source.lines().nth(5),
            Some("fn main() {"),
            "the code keeps its line number"
        );
        assert_eq!(s.source.lines().nth(2), Some(""));
        assert!(
            s.source.starts_with("#!/usr/bin/env"),
            "the shebang stays; rustc ignores it"
        );
    }

    #[test]
    fn a_bare_fence_and_longer_fences_work() {
        assert_eq!(
            parse("---\n[dependencies]\n---\nfn main(){}")
                .unwrap()
                .manifest,
            "[dependencies]"
        );
        let s = parse("-----cargo\nfoo = 1\n---\n-----\nfn main(){}").unwrap();
        assert_eq!(
            s.manifest, "foo = 1\n---",
            "a shorter fence inside does not close it"
        );
    }

    #[test]
    fn an_inner_attribute_is_not_a_shebang() {
        let s = parse("#![allow(unused)]\nfn main(){}").unwrap();
        assert!(s.manifest.is_empty());
        assert!(s.source.starts_with("#![allow"));
    }

    #[test]
    fn errors_are_specific() {
        assert!(
            parse("---cargo\n[dependencies]\nfn main(){}")
                .unwrap_err()
                .contains("never closed")
        );
        assert!(
            parse("---python\nx\n---\n")
                .unwrap_err()
                .contains("unsupported frontmatter type")
        );
    }

    #[test]
    fn package_names_are_valid_whatever_the_file_is_called() {
        assert_eq!(package_name(Path::new("hello.rs")), "hello");
        assert_eq!(package_name(Path::new("my script.v2.rs")), "my_script_v2");
        assert_eq!(package_name(Path::new("2fast.rs")), "script_2fast");
        assert_eq!(package_name(Path::new("dir/tool-x.rs")), "tool-x");
    }

    #[test]
    fn the_manifest_is_completed_and_stands_alone() {
        let m = manifest(Path::new("hello.rs"), "[dependencies]\nserde = \"1\"").unwrap();
        let t: toml::Table = m.parse().unwrap();
        assert_eq!(t["package"]["name"].as_str(), Some("hello"));
        assert_eq!(t["package"]["edition"].as_str(), Some("2021"));
        assert_eq!(t["dependencies"]["serde"].as_str(), Some("1"));
        assert_eq!(t["bin"][0]["path"].as_str(), Some("src/main.rs"));
        assert!(
            t["workspace"].as_table().unwrap().is_empty(),
            "keeps an outer workspace from adopting it"
        );
    }

    #[test]
    fn the_frontmatter_may_choose_its_own_edition_and_name() {
        let m = manifest(
            Path::new("x.rs"),
            "[package]\nname = \"custom\"\nedition = \"2024\"",
        )
        .unwrap();
        let t: toml::Table = m.parse().unwrap();
        assert_eq!(t["package"]["name"].as_str(), Some("custom"));
        assert_eq!(t["package"]["edition"].as_str(), Some("2024"));
        assert_eq!(t["bin"][0]["name"].as_str(), Some("custom"));
    }

    #[test]
    fn broken_toml_is_reported() {
        assert!(
            manifest(Path::new("x.rs"), "[dependencies\n")
                .unwrap_err()
                .contains("invalid manifest")
        );
    }
}
