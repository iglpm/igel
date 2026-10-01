//! What gets built: generated workspaces of known shape, or a copy of a real
//! project.
//!
//! Generated workspaces are layered: every crate of a layer depends on two
//! crates of the layer below, every crate of layer 1 on the `core` crate,
//! and an `app` package (library, binary, integration test) on the whole top
//! layer. Editing `core` therefore rebuilds everything; editing `app` only
//! `app`. The code is plain functions and small generic types, so compile
//! times grow with size the way real crates' do. Generation is deterministic:
//! the same name always gives the same sources.

use anyhow::{Context, Result, bail};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Workload {
    pub name: String,
    pub description: String,
    pub source: Source,
    /// A file whose change rebuilds one crate at the top of the graph.
    pub leaf_edit: Option<PathBuf>,
    /// A file whose change rebuilds (nearly) everything.
    pub core_edit: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum Source {
    Generated(Spec),
    Project(PathBuf),
}

#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub layers: usize,
    pub width: usize,
    /// Functions per crate.
    pub fns: usize,
    /// Unit tests per crate.
    pub tests: usize,
}

impl Spec {
    pub fn crates(&self) -> usize {
        self.layers * self.width + 1
    }
}

/// The built-in workloads, smallest first.
pub fn builtin() -> Vec<Workload> {
    let generated = |name: &str, spec: Spec, what: &str| Workload {
        name: name.to_string(),
        description: format!("{} crates, {what}", spec.crates()),
        source: Source::Generated(spec),
        leaf_edit: Some(PathBuf::from("app/src/edit.rs")),
        core_edit: Some(PathBuf::from("c0_0/src/edit.rs")),
    };
    vec![
        generated(
            "tiny",
            Spec {
                layers: 1,
                width: 1,
                fns: 40,
                tests: 4,
            },
            "a library and an app: where tool overhead dominates",
        ),
        generated(
            "medium",
            Spec {
                layers: 4,
                width: 6,
                fns: 120,
                tests: 3,
            },
            "a layered workspace the size of a typical service",
        ),
        generated(
            "large",
            Spec {
                layers: 6,
                width: 12,
                fns: 200,
                tests: 3,
            },
            "a large layered workspace",
        ),
    ]
}

/// A real project at `path`, copied before every use so it is never touched.
pub fn project(path: &Path, leaf: Option<PathBuf>, core: Option<PathBuf>) -> Result<Workload> {
    let path = path
        .canonicalize()
        .with_context(|| format!("no project at {}", path.display()))?;
    if !path.join("Cargo.toml").is_file() {
        bail!("{} has no Cargo.toml", path.display());
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".into());
    // Without a file to edit, the top package's own sources stand in for the
    // leaf: a change there is what an edit-compile loop looks like.
    let leaf = leaf.or_else(|| {
        ["src/main.rs", "src/lib.rs"]
            .into_iter()
            .map(PathBuf::from)
            .find(|f| path.join(f).is_file())
    });
    Ok(Workload {
        description: format!("the project at {}", path.display()),
        name,
        source: Source::Project(path),
        leaf_edit: leaf,
        core_edit: core,
    })
}

impl Workload {
    /// Writes the workload's sources into `dest` (which must not exist yet).
    pub fn materialize(&self, dest: &Path) -> Result<()> {
        std::fs::create_dir_all(dest)?;
        match &self.source {
            Source::Generated(spec) => generate(spec, dest),
            Source::Project(src) => copy_project(src, dest),
        }
    }
}

// ---------------------------------------------------------------------------
// Edits
// ---------------------------------------------------------------------------

/// Puts `file` (relative to `dir`) into edit state `n`: generated files hold
/// `pub const EDIT: u64 = <n>;`, other files get a trailing comment. The
/// same `n` always gives the same content, so a state can be returned to
/// (a branch switched back).
pub fn apply_edit(dir: &Path, file: &Path, n: u64) -> Result<()> {
    let path = dir.join(file);
    let pristine = pristine_path(&path);
    if !pristine.exists() {
        std::fs::copy(&path, &pristine)
            .with_context(|| format!("cannot edit {}", path.display()))?;
    }
    let original = std::fs::read_to_string(&pristine)?;
    let edited = if original.contains("pub const EDIT: u64 = 0;") {
        original.replace(
            "pub const EDIT: u64 = 0;",
            &format!("pub const EDIT: u64 = {n};"),
        )
    } else {
        format!("{original}\n// rune-bench edit {n}\n")
    };
    std::fs::write(&path, edited)?;
    Ok(())
}

/// Where the untouched content of an edited file is kept: beside the
/// project, not in it (a file in the package would be a source).
fn pristine_path(path: &Path) -> PathBuf {
    let key: String = path
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    std::env::temp_dir().join(format!("rune-bench-pristine-{}-{key}", std::process::id()))
}

/// Deletes the pristine copies this process kept of edited files.
pub fn forget_pristine() {
    let prefix = format!("rune-bench-pristine-{}-", std::process::id());
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

fn crate_name(layer: usize, i: usize) -> String {
    format!("c{layer}_{i}")
}

fn generate(spec: &Spec, dest: &Path) -> Result<()> {
    let mut members: Vec<String> = Vec::new();
    for layer in 0..spec.layers {
        for i in 0..spec.width {
            let name = crate_name(layer, i);
            let deps: Vec<String> = if layer == 0 {
                Vec::new()
            } else {
                let mut d = vec![
                    crate_name(layer - 1, i),
                    crate_name(layer - 1, (i + 1) % spec.width),
                ];
                if layer == 1 {
                    d.push(crate_name(0, 0));
                }
                d.sort();
                d.dedup();
                d
            };
            write_lib(dest, &name, &deps, spec, layer * spec.width + i)?;
            members.push(name);
        }
    }
    let top: Vec<String> = (0..spec.width)
        .map(|i| crate_name(spec.layers - 1, i))
        .collect();
    write_app(dest, &top, spec)?;
    members.push("app".into());

    let mut root = String::from("[workspace]\nresolver = \"2\"\nmembers = [\n");
    for m in &members {
        let _ = writeln!(root, "    \"{m}\",");
    }
    root.push_str("]\n");
    std::fs::write(dest.join("Cargo.toml"), root)?;
    Ok(())
}

fn manifest(name: &str, deps: &[String]) -> String {
    let mut m = format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\npublish = false\n\n[dependencies]\n"
    );
    for d in deps {
        let _ = writeln!(m, "{d} = {{ path = \"../{d}\" }}");
    }
    m
}

fn write_lib(dest: &Path, name: &str, deps: &[String], spec: &Spec, seed: usize) -> Result<()> {
    let dir = dest.join(name);
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::write(dir.join("Cargo.toml"), manifest(name, deps))?;
    std::fs::write(
        dir.join("src/edit.rs"),
        "/// Changed by the benchmarks to make an edit.\npub const EDIT: u64 = 0;\n",
    )?;
    let mut src =
        String::from("//! Generated by rune-bench.\n\npub mod edit;\npub use edit::EDIT;\n\n");
    for f in 0..spec.fns {
        let k = (seed * 7919 + f * 104_729) % 1_000_003;
        let rounds = f % 5 + 1;
        let _ = write!(
            src,
            "pub fn f{f}(x: u64) -> u64 {{\n    let mut a = x ^ {k};\n    for i in 0..{rounds} {{\n        a = a.rotate_left(5).wrapping_mul({m}).wrapping_add(i ^ {f});\n    }}\n    a\n}}\n\n",
            m = 2 * k + 1
        );
    }
    for t in 0..spec.fns / 10 {
        let _ = write!(
            src,
            "#[derive(Debug, Clone, Default, PartialEq)]\npub struct Item{t}<T> {{\n    pub value: T,\n    pub count: u64,\n}}\n\nimpl<T: Clone + std::fmt::Debug> Item{t}<T> {{\n    pub fn bump(&mut self, by: u64) -> u64 {{\n        self.count = self.count.wrapping_add(by * {m});\n        self.count\n    }}\n    pub fn describe(&self) -> String {{\n        format!(\"{{:?}}/{{}}\", self.value, self.count)\n    }}\n}}\n\n",
            m = t + 3
        );
    }
    let _ = write!(src, "pub fn items() -> usize {{\n    let mut n = 0;\n");
    for t in 0..spec.fns / 10 {
        let _ = writeln!(
            src,
            "    let mut i{t} = Item{t}::<u32>::default();\n    n += i{t}.bump({t}) as usize + Item{t}::<String>::default().describe().len();"
        );
    }
    // (`mut` only where something is folded in: generated code compiles
    // without warnings, which both tools would replay on every run.)
    let binding = if deps.is_empty() { "let" } else { "let mut" };
    let _ = write!(
        src,
        "    n\n}}\n\npub fn combined(x: u64) -> u64 {{\n    {binding} a = f0(x) ^ EDIT;\n"
    );
    for d in deps {
        let _ = writeln!(src, "    a ^= {d}::combined(x.wrapping_add(1));");
    }
    src.push_str("    a.wrapping_add(items() as u64)\n}\n\n#[cfg(test)]\nmod tests {\n");
    for t in 0..spec.tests {
        let f = t % spec.fns.max(1);
        let _ = writeln!(
            src,
            "    #[test]\n    fn deterministic_{t}() {{\n        assert_eq!(super::f{f}({t}), super::f{f}({t}));\n    }}"
        );
    }
    src.push_str("}\n");
    std::fs::write(dir.join("src/lib.rs"), src)?;
    Ok(())
}

fn write_app(dest: &Path, top: &[String], spec: &Spec) -> Result<()> {
    let dir = dest.join("app");
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::create_dir_all(dir.join("tests"))?;
    std::fs::write(dir.join("Cargo.toml"), manifest("app", top))?;
    std::fs::write(
        dir.join("src/edit.rs"),
        "/// Changed by the benchmarks to make an edit.\npub const EDIT: u64 = 0;\n",
    )?;
    let mut lib = String::from(
        "//! Generated by rune-bench.\n\npub mod edit;\n\npub fn run(x: u64) -> u64 {\n    let mut a = edit::EDIT;\n",
    );
    for t in top {
        let _ = writeln!(lib, "    a ^= {t}::combined(x);");
    }
    lib.push_str("    a\n}\n\n#[cfg(test)]\nmod tests {\n");
    for t in 0..spec.tests {
        let _ = writeln!(
            lib,
            "    #[test]\n    fn runs_{t}() {{\n        assert_eq!(super::run({t}), super::run({t}));\n    }}"
        );
    }
    lib.push_str("}\n");
    std::fs::write(dir.join("src/lib.rs"), lib)?;
    std::fs::write(
        dir.join("src/main.rs"),
        "fn main() {\n    println!(\"{}\", app::run(std::env::args().count() as u64));\n}\n",
    )?;
    std::fs::write(
        dir.join("tests/app.rs"),
        "#[test]\nfn the_program_runs() {\n    let out = std::process::Command::new(env!(\"CARGO_BIN_EXE_app\")).output().unwrap();\n    assert!(out.status.success());\n}\n\n#[test]\nfn the_library_answers() {\n    assert_eq!(app::run(1), app::run(1));\n}\n",
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Copying a real project
// ---------------------------------------------------------------------------

/// Directories never copied: build output, VCS data, editors' and tools' own.
const SKIP: &[&str] = &[
    "target",
    ".rune",
    ".git",
    ".hg",
    "node_modules",
    ".idea",
    ".vscode",
];

fn copy_project(src: &Path, dest: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(src).into_iter().filter_entry(|e| {
        e.depth() == 0 || !SKIP.contains(&e.file_name().to_string_lossy().as_ref())
    }) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        let to = dest.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&to)?;
        } else if entry.file_type().is_file() {
            std::fs::copy(entry.path(), &to)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        } else if entry.file_type().is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rune-bench-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_generated_workspace_has_the_promised_shape() {
        let dir = scratch("gen");
        let w = &builtin()[1];
        w.materialize(&dir).unwrap();
        let Source::Generated(spec) = w.source else {
            panic!()
        };
        let root = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert_eq!(root.matches("\",\n").count(), spec.crates());
        // Layer 1 depends on the core crate, so a core edit reaches everything.
        let l1 = std::fs::read_to_string(dir.join("c1_3/Cargo.toml")).unwrap();
        assert!(l1.contains("c0_0 = "), "{l1}");
        let app = std::fs::read_to_string(dir.join("app/Cargo.toml")).unwrap();
        assert!(app.contains(&format!("c{}_0", spec.layers - 1)));
        // Deterministic.
        let again = scratch("gen2");
        w.materialize(&again).unwrap();
        assert_eq!(
            std::fs::read(dir.join("c2_1/src/lib.rs")).unwrap(),
            std::fs::read(again.join("c2_1/src/lib.rs")).unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&again);
    }

    #[test]
    fn edits_are_repeatable_states() {
        let dir = scratch("edit");
        builtin()[0].materialize(&dir).unwrap();
        let file = Path::new("c0_0/src/edit.rs");
        apply_edit(&dir, file, 7).unwrap();
        let seven = std::fs::read_to_string(dir.join(file)).unwrap();
        assert!(seven.contains("EDIT: u64 = 7;"));
        apply_edit(&dir, file, 8).unwrap();
        apply_edit(&dir, file, 7).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(file)).unwrap(), seven);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
