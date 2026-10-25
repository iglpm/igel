//! Which file holds a directory's manifest: a `Igel.toml` with `[package]`/`[workspace]` (wins), else
//! `Cargo.toml`. A `Igel.toml` without them is only a settings file.

use std::path::{Path, PathBuf};

pub const CARGO_TOML: &str = "Cargo.toml";
pub const IGEL_TOML: &str = "Igel.toml";

pub fn is_manifest_file_name(name: &str) -> bool {
    name == CARGO_TOML || name == IGEL_TOML
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
    let igl = dir.join(IGEL_TOML);
    if igl.is_file() && std::fs::read_to_string(&igl).is_ok_and(|t| text_is_manifest(&t)) {
        return Some(igl);
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
    fn a_igel_toml_with_a_package_or_workspace_is_the_manifest() {
        let pkg = dir_with(&[("Igel.toml", "[package]\nname = \"a\"\n")]);
        assert_eq!(name_of(&pkg).as_deref(), Some("Igel.toml"));
        let ws = dir_with(&[("Igel.toml", "[workspace]\nmembers = [\"a\"]\n")]);
        assert_eq!(name_of(&ws).as_deref(), Some("Igel.toml"));
    }

    #[test]
    fn a_settings_only_igel_toml_leaves_the_cargo_toml_in_charge() {
        let d = dir_with(&[
            ("Cargo.toml", "[package]\nname = \"a\"\n"),
            ("Igel.toml", "[build]\njobs = 4\n[cas]\nverify = true\n"),
        ]);
        assert_eq!(name_of(&d).as_deref(), Some("Cargo.toml"));
        let only = dir_with(&[("Igel.toml", "[build]\njobs = 4\n")]);
        assert_eq!(name_of(&only), None);
        assert!(!has_manifest(only.path()));
    }

    #[test]
    fn when_both_are_manifests_igel_toml_wins() {
        let d = dir_with(&[
            ("Cargo.toml", "[package]\nname = \"cargo-name\"\n"),
            ("Igel.toml", "[package]\nname = \"igel-name\"\n"),
        ]);
        assert_eq!(name_of(&d).as_deref(), Some("Igel.toml"));
    }

    #[test]
    fn a_broken_manifest_is_still_recognised_as_one() {
        let d = dir_with(&[
            ("Cargo.toml", "[package]\nname = \"a\"\n"),
            ("Igel.toml", "[package]\nname = \n"),
        ]);
        assert_eq!(
            name_of(&d).as_deref(),
            Some("Igel.toml"),
            "so the parse error is reported"
        );
        let settings = dir_with(&[
            ("Cargo.toml", "[package]\nname = \"a\"\n"),
            ("Igel.toml", "[build\n"),
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
        assert!(is_manifest_file_name("Cargo.toml") && is_manifest_file_name("Igel.toml"));
        assert!(!is_manifest_file_name("cargo.toml") && !is_manifest_file_name("Cargo.lock"));
    }
}
