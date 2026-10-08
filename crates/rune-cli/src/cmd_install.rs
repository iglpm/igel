//! `rune install`/`uninstall`, recorded in Cargo's tracking files so `cargo install --list` sees them.

use crate::args::{InstallArgs, UninstallArgs};
use crate::cmd_build::plan_for;
use crate::graph::{Mode, TargetSelection};
use crate::session::{Session, drive, ensure_success};
use crate::ui;
use anyhow::{Context, Result, bail};
use rune_common::TargetKind;
use rune_scheduler::executor::bin_dir_for;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const CRATES_IO_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";

pub fn install(args: InstallArgs) -> Result<()> {
    let root = install_root(args.root.as_deref())?;
    if args.list {
        return list(&root);
    }
    if args.git.is_none() && args.path.is_none() && !args.crates.is_empty() {
        for spec in &args.crates {
            install_from_registry(&args, spec, &root)?;
        }
        return Ok(());
    }
    if args.crates.len() > 1 {
        bail!("only one package can be installed from a git repository or a path at a time");
    }
    match &args.git {
        Some(url) => install_from_git(&args, url, &root),
        None => {
            let path = args.path.clone().unwrap_or_else(|| PathBuf::from("."));
            let dir = rune_common::fsutil::canonicalize(&path)
                .with_context(|| format!("`{}` does not exist", path.display()))?;
            let manifest = rune_common::manifest_file::manifest_in(&dir);
            let name = package_name(&manifest)?;
            let source = format!("path+{}", rune_common::fsutil::file_url(&dir));
            build_and_install(&args, &manifest, &name, None, &source, &root)
        }
    }
}

fn install_from_registry(args: &InstallArgs, spec: &str, root: &Path) -> Result<()> {
    configure_registries();
    let (name, inline_req) = match spec.split_once('@') {
        Some((n, v)) => (n, Some(v)),
        None => (spec, None),
    };
    if inline_req.is_some() && args.version.is_some() {
        bail!(
            "cannot specify both `@{}` and `--version`",
            inline_req.unwrap_or_default()
        );
    }
    let req = version_req(inline_req.or(args.version.as_deref()))?;
    let offline = args.common.offline || args.common.frozen;
    let defs = rune_registry::source::defs();
    let index = defs.index_for_name(args.registry.as_deref())?;
    let client = rune_registry::source::IndexClient::for_registry(&index, offline)?;
    let entries = if offline {
        client.fetch_crate(name)
    } else {
        client.refresh_crate(name)
    }
    .with_context(|| format!("could not find `{name}` in the registry"))?;

    let record = pick_version(&entries, &req, name)?;
    let version = record.vers.clone();
    let source = if index == rune_common::CRATES_IO_INDEX {
        CRATES_IO_SOURCE.to_string()
    } else {
        index.clone()
    };
    if !args.force && installed_version(root, name, &source).as_deref() == Some(&version) {
        ui::action(
            "ignored",
            format!("{name} v{version} is already installed (use --force to reinstall)"),
        );
        return Ok(());
    }

    ui::action("downloading", format!("{name} v{version}"));
    let downloader = rune_registry::CrateDownloader::for_registry(&index)?.offline(offline);
    let src = downloader.download_and_extract(name, &version, Some(&record.cksum))?;
    let scratch = scratch_copy(&src, &format!("{name}-{version}"))?;
    // A published `Cargo.lock` is only honoured with `--locked`, as with Cargo.
    if !args.common.locked {
        let _ = std::fs::remove_file(scratch.join("Cargo.lock"));
    }
    let manifest = rune_common::manifest_file::manifest_in(&scratch);
    let result = build_and_install(args, &manifest, name, Some(&version), &source, root);
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn install_from_git(args: &InstallArgs, url: &str, root: &Path) -> Result<()> {
    configure_registries();
    let reference = rune_registry::git::GitRef::from_spec(
        args.branch.as_deref(),
        args.tag.as_deref(),
        args.rev.as_deref(),
    );
    ui::action("fetching", url);
    let checkout = rune_registry::git::checkout(url, &reference)?;
    let package_dir = match args.crates.first() {
        Some(name) => rune_registry::git::find_package(&checkout, name)?,
        None => checkout.dir.clone(),
    };
    let rel = package_dir
        .strip_prefix(&checkout.dir)
        .unwrap_or(Path::new(""))
        .to_path_buf();
    let scratch = scratch_copy(&checkout.dir, &format!("git-{}", &checkout.commit[..12]))?;
    let manifest = rune_common::manifest_file::manifest_in(&scratch.join(&rel));
    let name = package_name(&manifest)?;
    let source = checkout.lock_source();
    let result = build_and_install(args, &manifest, &name, None, &source, root);
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn build_and_install(
    args: &InstallArgs,
    manifest: &Path,
    name: &str,
    version: Option<&str>,
    source: &str,
    root: &Path,
) -> Result<()> {
    let mut common = args.common.clone();
    common.release = common.profile.is_none();
    common.manifest_path = Some(manifest.to_path_buf());
    if common.package.is_empty() {
        common.package = vec![name.to_string()];
    }
    let sess = Session::new(&common, "release")?;
    let version = version
        .map(str::to_string)
        .or_else(|| {
            sess.ws
                .members
                .iter()
                .find(|m| m.name == name)
                .map(|m| m.version.to_string())
        })
        .unwrap_or_default();

    let targets = TargetSelection {
        bins: args.bin.is_empty(),
        bin: args.bin.clone(),
        ..Default::default()
    };
    let plan = plan_for(&sess, Mode::Build, &targets)?;
    let bins: Vec<_> = plan
        .requested
        .iter()
        .filter(|r| r.kind == TargetKind::Bin && !r.harness)
        .collect();
    if bins.is_empty() {
        bail!("there is nothing to install in `{name} v{version}`: it has no binaries");
    }

    let bin_dir = root.join("bin");
    let mut tracking = Tracking::load(root);
    let exe = |n: &str| format!("{n}{}", std::env::consts::EXE_SUFFIX);
    if !args.force {
        for r in &bins {
            let dest = bin_dir.join(exe(&r.target));
            if !dest.exists() {
                continue;
            }
            match tracking.owner_of(&r.target) {
                Some(owner) if package_of(&owner) == name => {}
                Some(owner) => bail!(
                    "binary `{}` already exists in `{}` as part of `{owner}` (use --force to replace it)",
                    r.target,
                    bin_dir.display()
                ),
                None => bail!(
                    "binary `{}` already exists in `{}` (use --force to overwrite)",
                    r.target,
                    bin_dir.display()
                ),
            }
        }
    }

    ui::action("installing", format!("{name} v{version}"));
    sess.ws.config.ensure_dirs(sess.profile.output_dir_name())?;
    let (result, _) = drive(&sess, &plan.graph, |_| {})?;
    ensure_success(&result)?;

    std::fs::create_dir_all(&bin_dir)
        .with_context(|| format!("cannot create {}", bin_dir.display()))?;
    let mut installed = Vec::new();
    for r in &bins {
        let unit = &plan.graph.node(r.node).unit;
        let src = bin_dir_for(sess.config(), &sess.profile, unit).join(exe(&r.target));
        let dest = bin_dir.join(exe(&r.target));
        // Temp name + rename: a running old binary keeps working.
        let tmp = bin_dir.join(format!(".{}.rune-install", exe(&r.target)));
        let _ = remove_read_only(&tmp);
        std::fs::copy(&src, &tmp)
            .with_context(|| format!("failed to install {}", dest.display()))?;
        make_writable(&tmp).with_context(|| format!("failed to install {}", dest.display()))?;
        rune_common::fsutil::rename_over(&tmp, &dest)
            .with_context(|| format!("failed to install {}", dest.display()))?;
        ui::action("installed", dest.display());
        installed.push(r.target.clone());
    }
    installed.sort();
    tracking.record(name, &version, source, installed, &sess);
    tracking.save(root)?;
    warn_if_not_on_path(&bin_dir);
    Ok(())
}

pub fn uninstall(args: UninstallArgs) -> Result<()> {
    if args.packages.is_empty() {
        bail!("name a package to uninstall (see `rune install --list`)");
    }
    let root = install_root(args.root.as_deref())?;
    let mut tracking = Tracking::load(&root);
    for name in &args.packages {
        let keys: Vec<String> = tracking
            .v1
            .keys()
            .filter(|k| package_of(k) == name)
            .cloned()
            .collect();
        if keys.is_empty() {
            bail!("package `{name}` is not installed in `{}`", root.display());
        }
        for key in keys {
            let bins = tracking.v1.get(&key).cloned().unwrap_or_default();
            let remove: Vec<String> = if args.bin.is_empty() {
                bins.clone()
            } else {
                for b in &args.bin {
                    if !bins.contains(b) {
                        bail!("binary `{b}` is not part of `{key}`");
                    }
                }
                args.bin.clone()
            };
            for b in &remove {
                let path = root
                    .join("bin")
                    .join(format!("{b}{}", std::env::consts::EXE_SUFFIX));
                match std::fs::remove_file(&path) {
                    Ok(()) => ui::action("removed", path.display()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(e).with_context(|| format!("cannot remove {}", path.display()));
                    }
                }
            }
            let left: Vec<String> = bins.into_iter().filter(|b| !remove.contains(b)).collect();
            tracking.set_bins(&key, left);
        }
    }
    tracking.save(&root)
}

fn list(root: &Path) -> Result<()> {
    let tracking = Tracking::load(root);
    for (key, bins) in &tracking.v1 {
        let mut parts = key.splitn(3, ' ');
        let (name, version, source) = (
            parts.next().unwrap_or(""),
            parts.next().unwrap_or(""),
            parts.next().unwrap_or(""),
        );
        if source.contains(CRATES_IO_SOURCE) || source.is_empty() {
            println!("{name} v{version}:");
        } else {
            println!("{name} v{version} {source}:");
        }
        for b in bins {
            println!("    {b}");
        }
    }
    Ok(())
}

struct Tracking {
    v1: BTreeMap<String, Vec<String>>,
    v2: serde_json::Map<String, serde_json::Value>,
}

impl Tracking {
    fn load(root: &Path) -> Self {
        let v1 = std::fs::read_to_string(root.join(".crates.toml"))
            .ok()
            .and_then(|t| t.parse::<toml::Table>().ok())
            .and_then(|t| t.get("v1").and_then(|v| v.as_table()).cloned())
            .map(|t| {
                t.into_iter()
                    .map(|(k, v)| {
                        let bins = v
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|b| b.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default();
                        (k, bins)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let v2 = std::fs::read_to_string(root.join(".crates2.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.get("installs").and_then(|i| i.as_object()).cloned())
            .unwrap_or_default();
        Self { v1, v2 }
    }

    fn owner_of(&self, bin: &str) -> Option<String> {
        self.v1
            .iter()
            .find(|(_, bins)| bins.iter().any(|b| b == bin))
            .map(|(k, _)| k.clone())
    }

    fn record(
        &mut self,
        name: &str,
        version: &str,
        source: &str,
        bins: Vec<String>,
        sess: &Session,
    ) {
        let old: Vec<String> = self
            .v1
            .keys()
            .filter(|k| package_of(k) == name)
            .cloned()
            .collect();
        for k in old {
            self.v1.remove(&k);
            self.v2.remove(&k);
        }
        for list in self.v1.values_mut() {
            list.retain(|b| !bins.contains(b));
        }
        self.v1.retain(|_, list| !list.is_empty());
        let key = format!("{name} {version} ({source})");
        let tc = rune_common::toolchain::global();
        let details = serde_json::json!({
            "version_req": null,
            "bins": bins,
            "features": sess.features.features,
            "all_features": sess.features.all_features,
            "no_default_features": sess.features.no_default_features,
            "profile": sess.profile.name,
            "target": sess.target.clone().unwrap_or_else(|| tc.host.clone()),
            "rustc": format!("rustc {}", tc.version),
        });
        self.v1.insert(key.clone(), bins);
        self.v2.insert(key, details);
    }

    fn set_bins(&mut self, key: &str, bins: Vec<String>) {
        if bins.is_empty() {
            self.v1.remove(key);
            self.v2.remove(key);
        } else {
            if let Some(v) = self.v2.get_mut(key).and_then(|v| v.as_object_mut()) {
                v.insert("bins".into(), serde_json::json!(bins));
            }
            self.v1.insert(key.to_string(), bins);
        }
    }

    fn save(&self, root: &Path) -> Result<()> {
        std::fs::create_dir_all(root)?;
        let mut v1 = toml::Table::new();
        for (k, bins) in &self.v1 {
            v1.insert(k.clone(), toml::Value::from(bins.clone()));
        }
        let mut doc = toml::Table::new();
        doc.insert("v1".into(), toml::Value::Table(v1));
        write_atomic(&root.join(".crates.toml"), &toml::to_string(&doc)?)?;
        let v2 = serde_json::json!({ "installs": self.v2 });
        write_atomic(&root.join(".crates2.json"), &serde_json::to_string(&v2)?)?;
        Ok(())
    }

    fn version_of(&self, name: &str, source: &str) -> Option<String> {
        self.v1.keys().find_map(|k| {
            let mut parts = k.splitn(3, ' ');
            let (n, v, s) = (parts.next()?, parts.next()?, parts.next()?);
            (n == name && s.trim_matches(|c| c == '(' || c == ')') == source).then(|| v.to_string())
        })
    }
}

fn installed_version(root: &Path, name: &str, source: &str) -> Option<String> {
    Tracking::load(root).version_of(name, source)
}

fn package_of(key: &str) -> &str {
    key.split(' ').next().unwrap_or(key)
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("rune-tmp");
    std::fs::write(&tmp, content).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

pub fn install_root(root: Option<&Path>) -> Result<PathBuf> {
    if let Some(root) = root {
        return Ok(root.to_path_buf());
    }
    if let Some(root) = std::env::var_os("CARGO_INSTALL_ROOT") {
        return Ok(PathBuf::from(root));
    }
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home::home_dir().map(|h| h.join(".cargo")))
        .context("could not determine the install directory; pass --root")
}

fn configure_registries() {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config = rune_common::cargo_config::CargoConfig::load(&cwd);
    if let Some(e) = config.error() {
        ui::warn(e);
    }
    rune_registry::source::configure(rune_registry::source::RegistryDefs::from_config(
        config.root(),
    ));
}

fn version_req(spec: Option<&str>) -> Result<semver::VersionReq> {
    let Some(spec) = spec.map(str::trim) else {
        return Ok(semver::VersionReq::STAR);
    };
    if semver::Version::parse(spec).is_ok() {
        return Ok(semver::VersionReq::parse(&format!("={spec}"))?);
    }
    semver::VersionReq::parse(spec).with_context(|| format!("invalid version requirement `{spec}`"))
}

fn pick_version<'a>(
    entries: &'a rune_registry::CrateIndex,
    req: &semver::VersionReq,
    name: &str,
) -> Result<&'a rune_registry::IndexRecord> {
    let wants_pre = req.comparators.iter().any(|c| !c.pre.is_empty());
    let mut candidates: Vec<(semver::Version, &rune_registry::IndexRecord)> = entries
        .available_versions()
        .into_iter()
        .filter(|r| !r.yanked)
        .filter_map(|r| Some((semver::Version::parse(&r.vers).ok()?, r)))
        .filter(|(v, _)| req.matches(v) && (wants_pre || v.pre.is_empty()))
        .collect();
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    let tc = rune_common::toolchain::global();
    let fits = |r: &rune_registry::IndexRecord| {
        r.rust_version
            .as_deref()
            .is_none_or(|rv| tc.satisfies_msrv(rv))
    };
    if let Some((_, r)) = candidates.iter().find(|(_, r)| fits(r)) {
        return Ok(r);
    }
    match candidates.first() {
        Some((v, r)) => {
            ui::warn(format!(
                "`{name} v{v}` needs Rust {} (this is {}); installing it anyway",
                r.rust_version.as_deref().unwrap_or("?"),
                tc.version
            ));
            Ok(r)
        }
        None => bail!("could not find a version of `{name}` matching `{req}`"),
    }
}

fn scratch_copy(src: &Path, label: &str) -> Result<PathBuf> {
    let base = rune_common::settings::rune_home().join("install");
    let dir = base.join(format!("{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy_tree(src, &dir).with_context(|| format!("cannot copy {} to build it", src.display()))?;
    Ok(dir)
}

fn copy_tree(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" || name == ".rune" || name == "target" {
            continue;
        }
        let from = entry.path();
        let to = dest.join(&name);
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_tree(&from, &to)?;
        } else if ty.is_symlink() {
            let target = std::fs::read_link(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, &to)?;
            #[cfg(not(unix))]
            std::fs::copy(from.parent().unwrap_or(src).join(target), &to).map(|_| ())?;
        } else {
            std::fs::copy(&from, &to)?;
            make_writable(&to)?;
        }
    }
    Ok(())
}

fn make_writable(path: &Path) -> std::io::Result<()> {
    let mut perms = std::fs::metadata(path)?.permissions();
    if perms.readonly() {
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn remove_read_only(path: &Path) -> std::io::Result<()> {
    make_writable(path)?;
    std::fs::remove_file(path)
}

fn package_name(manifest: &Path) -> Result<String> {
    let text = std::fs::read_to_string(manifest)
        .with_context(|| format!("cannot read {}", manifest.display()))?;
    let table: toml::Table = text
        .parse()
        .with_context(|| format!("cannot parse {}", manifest.display()))?;
    table
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_string)
        .with_context(|| {
            format!(
                "{} is a virtual workspace; name the package to install",
                manifest.display()
            )
        })
}

fn warn_if_not_on_path(bin_dir: &Path) {
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d == bin_dir))
        .unwrap_or(false);
    if !on_path {
        ui::warn(format!(
            "be sure to add `{}` to your PATH to be able to run the installed binaries",
            bin_dir.display()
        ));
    }
}
