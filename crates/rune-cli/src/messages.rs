//! `--message-format=json`: the build's events as one JSON object per line on
//! stdout, in the shape Cargo uses (`compiler-message`, `compiler-artifact`,
//! `build-finished`), so rust-analyzer, CI annotators and editors that already
//! understand `cargo --message-format=json` work unchanged.

use rune_common::{CompilationUnit, CrateSource, CrateType, Profile, RuneConfig, TargetKind};
use rune_scheduler::pipeline::PipelineEvent;
use serde_json::{Value, json};
use std::path::PathBuf;

/// Whether a `--message-format` value asks for JSON (`json`,
/// `json-diagnostic-short`, `json-render-diagnostics`, …).
pub fn wants_json(format: &str) -> bool {
    format.starts_with("json")
}

/// Rejects values Rune does not know rather than silently ignoring them.
pub fn validate(format: &str) -> Result<(), String> {
    if format.is_empty() || matches!(format, "human" | "short") || wants_json(format) {
        Ok(())
    } else {
        Err(format!(
            "unknown --message-format `{format}` (expected `human`, `short` or `json`)"
        ))
    }
}

/// Cargo's package id for a unit's package.
pub fn package_id(unit: &CompilationUnit) -> String {
    let (name, version) = (&unit.crate_id.name, &unit.crate_id.version);
    match &unit.crate_id.source {
        // The name is left out when the directory already says it.
        CrateSource::Local
            if unit
                .manifest_dir
                .file_name()
                .is_some_and(|n| n == name.as_str()) =>
        {
            format!("path+file://{}#{version}", unit.manifest_dir.display())
        }
        CrateSource::Local => format!(
            "path+file://{}#{name}@{version}",
            unit.manifest_dir.display()
        ),
        CrateSource::Registry { index, .. } => {
            let index = if index.contains("index.crates.io") {
                "https://github.com/rust-lang/crates.io-index"
            } else {
                index.trim_start_matches("sparse+")
            };
            format!("registry+{index}#{name}@{version}")
        }
        CrateSource::Git { url, .. } => format!("git+{url}#{name}@{version}"),
    }
}

fn target_json(unit: &CompilationUnit) -> Value {
    let kind = if unit.is_build_script {
        "custom-build"
    } else if unit.crate_type == CrateType::ProcMacro {
        "proc-macro"
    } else {
        unit.kind.as_str()
    };
    let name = if unit.target_name.is_empty() {
        unit.crate_id.name.clone()
    } else {
        unit.target_name.clone()
    };
    json!({
        "kind": [kind],
        "crate_types": [unit.crate_type.to_string()],
        "name": name,
        "src_path": unit.source_root,
        "edition": unit.edition.as_str(),
        "doc": unit.kind == TargetKind::Lib || unit.kind == TargetKind::Bin,
        "doctest": unit.kind == TargetKind::Lib,
        "test": unit.harness,
    })
}

fn common(unit: &CompilationUnit, reason: &str) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("reason".into(), json!(reason));
    m.insert("package_id".into(), json!(package_id(unit)));
    m.insert(
        "manifest_path".into(),
        json!(rune_common::manifest_file::manifest_in(&unit.manifest_dir)),
    );
    m.insert("target".into(), target_json(unit));
    m
}

fn compiler_messages(unit: &CompilationUnit, diagnostics: &[String]) -> Vec<String> {
    diagnostics
        .iter()
        .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
        .map(|message| {
            let mut m = common(unit, "compiler-message");
            m.insert("message".into(), message);
            Value::Object(m).to_string()
        })
        .collect()
}

fn artifact_line(
    unit: &CompilationUnit,
    config: &RuneConfig,
    profile: &Profile,
    artifacts: &[PathBuf],
    fresh: bool,
) -> String {
    let unit_profile = unit.effective_profile(profile);
    let mut artifacts = artifacts.to_vec();
    let mut executable = None;
    // Binaries, and every test harness (a library's unit tests included):
    // `cargo test --no-run --message-format json` is how test runners such as
    // nextest find the test executables.
    if (unit.crate_type == CrateType::Bin || unit.harness) && !unit.check_only {
        // The linked program: no extension, or the target's (`.exe`,
        // `.wasm`) — never the debug info next to it (`.pdb`, `.dSYM`).
        if let Some(bin) = artifacts.iter_mut().find(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                None | Some("exe" | "wasm" | "js")
            )
        }) {
            // A binary is reported by its friendly name, as Cargo reports the
            // uplifted `target/debug/app`; a test binary by its hashed file.
            if !unit.harness {
                *bin = rune_scheduler::executor::bin_dir_for(config, profile, unit)
                    .join(unit.executable_name(&unit.crate_id.name));
            }
            executable = Some(bin.clone());
        }
    }
    let mut m = common(unit, "compiler-artifact");
    m.insert(
        "profile".into(),
        json!({
            "opt_level": unit_profile.opt_level.as_str(),
            "debuginfo": match unit_profile.debuginfo.as_str() {
                "0" => Value::Null,
                other => other.parse::<u32>().map(Value::from).unwrap_or(Value::Null),
            },
            "debug_assertions": unit_profile.debug_assertions,
            "overflow_checks": unit_profile.debug_assertions,
            "test": unit.harness,
        }),
    );
    m.insert("features".into(), json!(unit.features));
    m.insert("filenames".into(), json!(artifacts));
    m.insert("executable".into(), json!(executable));
    m.insert("fresh".into(), json!(fresh));
    Value::Object(m).to_string()
}

/// The JSON lines a pipeline event produces (usually none).
pub fn lines_for(
    unit: &CompilationUnit,
    config: &RuneConfig,
    profile: &Profile,
    event: &PipelineEvent,
) -> Vec<String> {
    match event {
        PipelineEvent::BuildFinished {
            artifacts,
            diagnostics,
            ..
        } => {
            let mut lines = compiler_messages(unit, diagnostics);
            lines.push(artifact_line(unit, config, profile, artifacts, false));
            lines
        }
        PipelineEvent::CacheHit {
            artifacts,
            diagnostics,
            ..
        } => {
            let mut lines = compiler_messages(unit, diagnostics);
            lines.push(artifact_line(unit, config, profile, artifacts, true));
            lines
        }
        PipelineEvent::BuildFailed { diagnostics, .. } => compiler_messages(unit, diagnostics),
        _ => Vec::new(),
    }
}

/// Cargo's `build-script-executed` line: what a package's build script
/// reported and where its `OUT_DIR` is — what rust-analyzer reads to resolve
/// `include!(concat!(env!("OUT_DIR"), ...))` and script-set cfgs.
pub fn build_script_executed(
    unit: &CompilationUnit,
    output: &rune_common::BuildScriptOutput,
    out_dir: &std::path::Path,
) -> String {
    let spell = |kind: &Option<String>, value: String| match kind {
        Some(k) => format!("{k}={value}"),
        None => value,
    };
    json!({
        "reason": "build-script-executed",
        "package_id": package_id(unit),
        "linked_libs": output
            .link_libs
            .iter()
            .map(|l| spell(&l.kind, l.name.clone()))
            .collect::<Vec<_>>(),
        "linked_paths": output
            .link_search
            .iter()
            .map(|l| spell(&l.kind, l.path.display().to_string()))
            .collect::<Vec<_>>(),
        "cfgs": output.cfgs,
        "env": output.env_vars.iter().map(|(k, v)| [k, v]).collect::<Vec<_>>(),
        "out_dir": out_dir,
    })
    .to_string()
}

/// The closing line of a build.
pub fn build_finished(success: bool) -> String {
    json!({ "reason": "build-finished", "success": success }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_formats_are_recognised() {
        assert!(wants_json("json"));
        assert!(wants_json("json-diagnostic-rendered-ansi"));
        assert!(!wants_json("human"));
        assert!(validate("human").is_ok() && validate("short").is_ok() && validate("json").is_ok());
        assert!(validate("").is_ok());
        assert!(validate("xml").is_err());
    }

    #[test]
    fn the_closing_line_reports_success() {
        let v: Value = serde_json::from_str(&build_finished(true)).unwrap();
        assert_eq!(v["reason"], "build-finished");
        assert_eq!(v["success"], true);
    }
}
