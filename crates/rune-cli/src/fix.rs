//! `rune fix` / `rune clippy --fix`: applies `MachineApplicable` suggestions like rustfix (whole or not
//! at all, overlaps skipped, re-checked until nothing applies).

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

fn suggestions(
    diagnostics: &[String],
    roots: &[PathBuf],
    base: &std::path::Path,
) -> BTreeMap<PathBuf, Vec<Vec<Edit>>> {
    let mut out: BTreeMap<PathBuf, Vec<Vec<Edit>>> = BTreeMap::new();
    let mut seen: BTreeSet<(PathBuf, Vec<Edit>)> = BTreeSet::new();
    for text in diagnostics {
        let Ok(diag) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        let mut stack = vec![&diag];
        while let Some(d) = stack.pop() {
            if let Some(children) = d["children"].as_array() {
                stack.extend(children);
            }
            let spans: Vec<&Value> = d["spans"]
                .as_array()
                .map(|s| {
                    s.iter()
                        .filter(|s| s["suggested_replacement"].is_string())
                        .collect()
                })
                .unwrap_or_default();
            if spans.is_empty()
                || !spans
                    .iter()
                    .all(|s| s["suggestion_applicability"] == "MachineApplicable")
            {
                continue;
            }
            let mut file: Option<PathBuf> = None;
            let mut edits = Vec::new();
            let mut ok = true;
            for s in &spans {
                let path = base.join(s["file_name"].as_str().unwrap_or_default());
                if file.as_ref().is_some_and(|f| *f != path) {
                    ok = false;
                    break;
                }
                let (Some(start), Some(end)) = (s["byte_start"].as_u64(), s["byte_end"].as_u64())
                else {
                    ok = false;
                    break;
                };
                edits.push(Edit {
                    start: start as usize,
                    end: end as usize,
                    text: s["suggested_replacement"]
                        .as_str()
                        .unwrap_or_default()
                        .into(),
                });
                file = Some(path);
            }
            let Some(file) = file.filter(|_| ok) else {
                continue;
            };
            if !roots.iter().any(|r| file.starts_with(r)) {
                continue;
            }
            edits.sort();
            if seen.insert((file.clone(), edits.clone())) {
                out.entry(file).or_default().push(edits);
            }
        }
    }
    out
}

pub fn apply(diagnostics: &[String], roots: &[PathBuf], base: &std::path::Path) -> Result<usize> {
    let mut applied = 0;
    for (file, mut solutions) in suggestions(diagnostics, roots, base) {
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("cannot read {}", file.display()))?;
        solutions.sort_by_key(|s| s.first().map(|e| e.start).unwrap_or(0));
        let mut taken: Vec<(usize, usize)> = Vec::new();
        let mut edits: Vec<Edit> = Vec::new();
        for solution in solutions {
            let overlaps = solution.iter().any(|e| {
                taken
                    .iter()
                    .any(|&(s, t)| e.start < t && s < e.end || (e.start == e.end && e.start == s))
            });
            if overlaps
                || solution
                    .iter()
                    .any(|e| e.end > text.len() || e.start > e.end)
            {
                continue;
            }
            taken.extend(solution.iter().map(|e| (e.start, e.end)));
            edits.extend(solution);
            applied += 1;
        }
        if edits.is_empty() {
            continue;
        }
        edits.sort();
        let mut result = text.clone();
        for e in edits.iter().rev() {
            if !result.is_char_boundary(e.start) || !result.is_char_boundary(e.end) {
                bail!(
                    "a suggestion for {} does not fall on characters",
                    file.display()
                );
            }
            result.replace_range(e.start..e.end, &e.text);
        }
        write_atomic(&file, &result)?;
        crate::ui::action("fixed", display(&file));
    }
    Ok(applied)
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("rune-fix-tmp");
    std::fs::write(&tmp, content).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

fn display(path: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(|p| p.display().to_string()))
        .unwrap_or_else(|| path.display().to_string())
}

pub fn check_vcs(
    root: &Path,
    allow_dirty: bool,
    allow_staged: bool,
    allow_no_vcs: bool,
) -> Result<()> {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output();
    let Some(out) = out.ok().filter(|o| o.status.success()) else {
        if allow_no_vcs {
            return Ok(());
        }
        bail!(
            "no VCS found for this package and `rune fix` can overwrite local code \
             (pass --allow-no-vcs to proceed anyway)"
        );
    };
    if allow_dirty {
        return Ok(());
    }
    let dirty: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| {
            let staged_only = l.as_bytes().get(1) == Some(&b' ');
            !(allow_staged && staged_only)
        })
        .map(|l| format!("  * {}", &l[3.min(l.len())..]))
        .collect();
    if dirty.is_empty() {
        return Ok(());
    }
    bail!(
        "the working directory of this package has uncommitted changes, and `rune fix` can \
         overwrite them:\n{}\ncommit or stash them first, or pass --allow-dirty",
        dirty.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(file: &str, spans: &[(usize, usize, &str)], applicability: &str) -> String {
        let spans: Vec<Value> = spans
            .iter()
            .map(|(s, e, t)| {
                serde_json::json!({
                    "file_name": file,
                    "byte_start": s,
                    "byte_end": e,
                    "suggested_replacement": t,
                    "suggestion_applicability": applicability,
                })
            })
            .collect();
        serde_json::json!({
            "message": "unused",
            "spans": [],
            "children": [{ "message": "remove it", "spans": spans, "children": [] }],
        })
        .to_string()
    }

    #[test]
    fn applies_whole_machine_applicable_suggestions_only() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "use a;\nuse b;\nfn f() {}\n").unwrap();
        let f = file.to_str().unwrap();
        let diags = vec![
            diag(f, &[(0, 7, "")], "MachineApplicable"),
            diag(f, &[(0, 7, "")], "MachineApplicable"),
            diag(f, &[(7, 14, "")], "MaybeIncorrect"),
            diag(f, &[(17, 18, "g"), (19, 19, "x: u8")], "MachineApplicable"),
        ];
        let n = apply(&diags, &[dir.path().to_path_buf()], dir.path()).unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "use b;\nfn g(x: u8) {}\n"
        );
    }

    #[test]
    fn never_touches_files_outside_the_given_roots() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("dep.rs");
        std::fs::write(&file, "use a;\n").unwrap();
        let diags = vec![diag(
            file.to_str().unwrap(),
            &[(0, 7, "")],
            "MachineApplicable",
        )];
        let n = apply(
            &diags,
            &[PathBuf::from("/somewhere/else")],
            std::path::Path::new("/"),
        )
        .unwrap();
        assert_eq!(n, 0);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "use a;\n");
    }

    #[test]
    fn overlapping_suggestions_apply_the_first_only() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.rs");
        std::fs::write(&file, "abcdef").unwrap();
        let f = file.to_str().unwrap();
        let diags = vec![
            diag(f, &[(0, 3, "X")], "MachineApplicable"),
            diag(f, &[(2, 4, "Y")], "MachineApplicable"),
        ];
        assert_eq!(
            apply(&diags, &[dir.path().to_path_buf()], dir.path()).unwrap(),
            1
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "Xdef");
    }
}
