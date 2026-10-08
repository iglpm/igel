//! Which file holds a directory's manifest: a `Rune.toml` with `[package]`/`[workspace]` (wins), else
//! `Cargo.toml`. A `Rune.toml` without them is only a settings file.

use std::path::{Path, PathBuf};

pub const CARGO_TOML: &str = "Cargo.toml";
pub const RUNE_TOML: &str = "Rune.toml";

pub fn is_manifest_file_name(name: &str) -> bool {
    name == CARGO_TOML || name == RUNE_TOML
}

/// A file that doesn't parse is judged by its headers, so a broken manifest is reported, not skipped.
pub fn text_is_manifest(text: &str) -> bool {
    match text.parse::<toml::Table>() {
        Ok(table) => table.contains_key("package") || table.contains_key("workspace"),
        Err(_) => text.lines().any(|l| {
            let l = l.trim_start();
            l.starts_with("[package") || l.starts_with("[workspace")
        }),
    }
}

pub fn find_manifest(dir: &Path) -> Option<PathBuf> {
    let rune = dir.join(RUNE_TOML);
    if rune.is_file() && std::fs::read_to_string(&rune).is_ok_and(|t| text_is_manifest(&t)) {
        return Some(rune);
    }
    let cargo = dir.join(CARGO_TOML);
    cargo.is_file().then_some(cargo)
}

pub fn has_manifest(dir: &Path) -> bool {
    find_manifest(dir).is_some()
}

pub fn manifest_in(dir: &Path) -> PathBuf {
    find_manifest(dir).unwrap_or_else(|| dir.join(CARGO_TOML))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (name, content) in files {
            std::fs::write(d.path().join(name), content).unwrap();
        }
        d
    }

    fn name_of(d: &tempfile::TempDir) -> Option<String> {
        find_manifest(d.path()).map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
    }

    #[test]
    fn a_plain_cargo_project_is_unchanged() {
        let d = dir_with(&[("Cargo.toml", "[package]\nname = \"a\"\n")]);
        assert_eq!(name_of(&d).as_deref(), Some("Cargo.toml"));
    }

    #[test]
    fn a_rune_toml_with_a_package_or_workspace_is_the_manifest() {
        let pkg = dir_with(&[("Rune.toml", "[package]\nname = \"a\"\n")]);
        assert_eq!(name_of(&pkg).as_deref(), Some("Rune.toml"));
        let ws = dir_with(&[("Rune.toml", "[workspace]\nmembers = [\"a\"]\n")]);
        assert_eq!(name_of(&ws).as_deref(), Some("Rune.toml"));
    }

    #[test]
    fn a_settings_only_rune_toml_leaves_the_cargo_toml_in_charge() {
        let d = dir_with(&[
            ("Cargo.toml", "[package]\nname = \"a\"\n"),
            ("Rune.toml", "[build]\njobs = 4\n[cas]\nverify = true\n"),
        ]);
        assert_eq!(name_of(&d).as_deref(), Some("Cargo.toml"));
        let only = dir_with(&[("Rune.toml", "[build]\njobs = 4\n")]);
        assert_eq!(name_of(&only), None);
        assert!(!has_manifest(only.path()));
    }

    #[test]
    fn when_both_are_manifests_rune_toml_wins() {
        let d = dir_with(&[
            ("Cargo.toml", "[package]\nname = \"cargo-name\"\n"),
            ("Rune.toml", "[package]\nname = \"rune-name\"\n"),
        ]);
        assert_eq!(name_of(&d).as_deref(), Some("Rune.toml"));
    }

    #[test]
    fn a_broken_manifest_is_still_recognised_as_one() {
        let d = dir_with(&[
            ("Cargo.toml", "[package]\nname = \"a\"\n"),
            ("Rune.toml", "[package]\nname = \n"),
        ]);
        assert_eq!(
            name_of(&d).as_deref(),
            Some("Rune.toml"),
            "so the parse error is reported"
        );
        let settings = dir_with(&[
            ("Cargo.toml", "[package]\nname = \"a\"\n"),
            ("Rune.toml", "[build\n"),
        ]);
        assert_eq!(
            name_of(&settings).as_deref(),
            Some("Cargo.toml"),
            "broken settings are not a manifest"
        );
    }

    #[test]
    fn manifest_in_falls_back_to_where_a_cargo_toml_would_be() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(manifest_in(d.path()), d.path().join("Cargo.toml"));
    }

    #[test]
    fn file_names() {
        assert!(is_manifest_file_name("Cargo.toml") && is_manifest_file_name("Rune.toml"));
        assert!(!is_manifest_file_name("cargo.toml") && !is_manifest_file_name("Cargo.lock"));
    }
}
