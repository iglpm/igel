//! `--message-format=json`: Cargo's JSON message shapes, so existing tooling works unchanged.

use igel_common::{CompilationUnit, CrateSource, CrateType, IgelConfig, Profile, TargetKind};
use igel_scheduler::pipeline::PipelineEvent;
use serde_json::{Value, json};
use std::path::PathBuf;

pub fn wants_json(format: &str) -> bool {
    format.starts_with("json")
}

pub fn validate(format: &str) -> Result<(), String> {
    if format.is_empty() || matches!(format, "human" | "short") || wants_json(format) {
        Ok(())
    } else {
        Err(format!(
            "unknown --message-format `{format}` (expected `human`, `short` or `json`)"
        ))
    }
}

pub fn package_id(unit: &CompilationUnit) -> String {
    let (name, version) = (&unit.crate_id.name, &unit.crate_id.version);
    match &unit.crate_id.source {
        CrateSource::Local
            if unit
                .manifest_dir
                .file_name()
                .is_some_and(|n| n == name.as_str()) =>
        {
            format!(
                "path+{}#{version}",
                igel_common::fsutil::file_url(&unit.manifest_dir)
            )
        }
        CrateSource::Local => format!(
            "path+{}#{name}@{version}",
            igel_common::fsutil::file_url(&unit.manifest_dir)
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
        json!(igel_common::manifest_file::manifest_in(&unit.manifest_dir)),
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
    config: &IgelConfig,
    profile: &Profile,
    artifacts: &[PathBuf],
    fresh: bool,
) -> String {
    let unit_profile = unit.effective_profile(profile);
    let mut artifacts = artifacts.to_vec();
    let mut executable = None;
    // `cargo test --no-run --message-format json` is how runners like nextest find test executables.
    if (unit.crate_type == CrateType::Bin || unit.harness)
        && !unit.check_only
        && let Some(bin) = artifacts.iter_mut().find(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                None | Some("exe" | "wasm" | "js")
            )
        })
    {
        if !unit.harness {
            *bin = igel_scheduler::executor::bin_dir_for(config, profile, unit)
                .join(unit.executable_name(&unit.crate_id.name));
        }
        executable = Some(bin.clone());
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

pub fn lines_for(
    unit: &CompilationUnit,
    config: &IgelConfig,
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

pub fn build_script_executed(
    unit: &CompilationUnit,
    output: &igel_common::BuildScriptOutput,
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
