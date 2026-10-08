//! `rune sbom`: CycloneDX 1.5 or SPDX 2.3 JSON.

use anyhow::Result;
use std::collections::{BTreeSet, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::args::SbomArgs;
use crate::graph::{self, EdgeKind, InventoryPackage};
use crate::ui;
use crate::workspace::Workspace;

pub fn purl(p: &InventoryPackage) -> String {
    let base = format!("pkg:cargo/{}@{}", p.name, p.version);
    let encode = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z'
                | b'a'..=b'z'
                | b'0'..=b'9'
                | b'-'
                | b'.'
                | b'_'
                | b'~'
                | b'/'
                | b':' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    if let Some((url, commit)) = &p.git {
        return format!("{base}?vcs_url={}", encode(&format!("git+{url}@{commit}")));
    }
    match p.registry.as_deref() {
        Some(index) if index != rune_common::CRATES_IO_INDEX => {
            format!(
                "{base}?repository_url={}",
                encode(index.trim_start_matches("sparse+"))
            )
        }
        _ => base,
    }
}

pub fn rfc3339(secs: u64) -> String {
    rune_common::time::format_rfc3339(secs)
}

fn uuid(seed: &str) -> String {
    let h = blake3::hash(seed.as_bytes());
    let mut b: [u8; 16] = h.as_bytes()[..16].try_into().unwrap();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

pub(crate) fn reachable(pkgs: &[InventoryPackage], roots: &[usize], dev: bool) -> Vec<usize> {
    let mut seen: BTreeSet<usize> = BTreeSet::new();
    let mut stack = roots.to_vec();
    while let Some(i) = stack.pop() {
        if !seen.insert(i) {
            continue;
        }
        for &(j, kind) in &pkgs[i].deps {
            if kind != EdgeKind::Dev || (dev && pkgs[i].member) {
                stack.push(j);
            }
        }
    }
    seen.into_iter().collect()
}

fn build_only(pkgs: &[InventoryPackage], roots: &[usize], listed: &[usize]) -> HashSet<usize> {
    let mut runtime: HashSet<usize> = HashSet::new();
    let mut stack = roots.to_vec();
    while let Some(i) = stack.pop() {
        if !runtime.insert(i) {
            continue;
        }
        for &(j, kind) in &pkgs[i].deps {
            if kind == EdgeKind::Normal {
                stack.push(j);
            }
        }
    }
    listed
        .iter()
        .copied()
        .filter(|i| !runtime.contains(i))
        .collect()
}

fn licence(p: &InventoryPackage) -> Option<&str> {
    Some(p.meta.license.trim()).filter(|l| !l.is_empty())
}

fn cyclonedx(
    pkgs: &[InventoryPackage],
    roots: &[usize],
    listed: &[usize],
    build: &HashSet<usize>,
    stamp: &str,
    serial: &str,
) -> serde_json::Value {
    let component = |i: usize| {
        let p = &pkgs[i];
        let mut c = serde_json::json!({
            "type": if p.member { "application" } else { "library" },
            "bom-ref": purl(p),
            "name": p.name,
            "version": p.version.to_string(),
            "purl": purl(p),
        });
        if !p.meta.description.trim().is_empty() {
            c["description"] = p.meta.description.trim().into();
        }
        if !p.meta.authors.is_empty() {
            c["author"] = p.meta.authors.replace(':', ", ").into();
        }
        if let Some(l) = licence(p) {
            c["licenses"] = serde_json::json!([{ "expression": l }]);
        }
        if let Some(sum) = &p.checksum {
            c["hashes"] = serde_json::json!([{ "alg": "SHA-256", "content": sum }]);
        }
        let mut refs = Vec::new();
        if !p.meta.repository.is_empty() {
            refs.push(serde_json::json!({ "type": "vcs", "url": p.meta.repository }));
        }
        if !p.meta.homepage.is_empty() {
            refs.push(serde_json::json!({ "type": "website", "url": p.meta.homepage }));
        }
        if !refs.is_empty() {
            c["externalReferences"] = refs.into();
        }
        if build.contains(&i) {
            c["properties"] =
                serde_json::json!([{ "name": "rune:dependency-kind", "value": "build" }]);
        }
        c
    };
    let dependencies: Vec<serde_json::Value> = listed
        .iter()
        .map(|&i| {
            let on: BTreeSet<String> = pkgs[i]
                .deps
                .iter()
                .filter(|(j, _)| listed.contains(j))
                .map(|(j, _)| purl(&pkgs[*j]))
                .collect();
            serde_json::json!({ "ref": purl(&pkgs[i]), "dependsOn": on })
        })
        .collect();
    let mut doc = serde_json::json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": format!("urn:uuid:{serial}"),
        "version": 1,
        "metadata": {
            "timestamp": stamp,
            "tools": { "components": [{
                "type": "application",
                "name": "rune",
                "version": env!("CARGO_PKG_VERSION"),
            }]},
        },
        "components": listed
            .iter()
            .filter(|i| !(roots.len() == 1 && roots[0] == **i))
            .map(|&i| component(i))
            .collect::<Vec<_>>(),
        "dependencies": dependencies,
    });
    if roots.len() == 1 {
        doc["metadata"]["component"] = component(roots[0]);
    }
    doc
}

fn spdx_id(p: &InventoryPackage) -> String {
    let clean = |s: String| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect::<String>()
    };
    clean(format!("SPDXRef-Package-{}-{}", p.name, p.version))
}

fn spdx(
    pkgs: &[InventoryPackage],
    roots: &[usize],
    listed: &[usize],
    build: &HashSet<usize>,
    stamp: &str,
    serial: &str,
    name: &str,
) -> serde_json::Value {
    let packages: Vec<serde_json::Value> = listed
        .iter()
        .map(|&i| {
            let p = &pkgs[i];
            let download = match (&p.git, &p.registry) {
                (Some((url, commit)), _) => format!("git+{url}@{commit}"),
                (None, Some(index)) if index == rune_common::CRATES_IO_INDEX => format!(
                    "https://crates.io/api/v1/crates/{}/{}/download",
                    p.name, p.version
                ),
                _ => "NOASSERTION".to_string(),
            };
            let mut pkg = serde_json::json!({
                "SPDXID": spdx_id(p),
                "name": p.name,
                "versionInfo": p.version.to_string(),
                "downloadLocation": download,
                "filesAnalyzed": false,
                "licenseConcluded": "NOASSERTION",
                "licenseDeclared": licence(p).unwrap_or("NOASSERTION"),
                "copyrightText": "NOASSERTION",
                "externalRefs": [{
                    "referenceCategory": "PACKAGE-MANAGER",
                    "referenceType": "purl",
                    "referenceLocator": purl(p),
                }],
            });
            if let Some(sum) = &p.checksum {
                pkg["checksums"] =
                    serde_json::json!([{ "algorithm": "SHA256", "checksumValue": sum }]);
            }
            if !p.meta.description.trim().is_empty() {
                pkg["description"] = p.meta.description.trim().into();
            }
            if !p.meta.homepage.is_empty() {
                pkg["homepage"] = p.meta.homepage.clone().into();
            }
            pkg
        })
        .collect();
    let mut relationships: Vec<serde_json::Value> = roots
        .iter()
        .map(|&r| {
            serde_json::json!({
                "spdxElementId": "SPDXRef-DOCUMENT",
                "relationshipType": "DESCRIBES",
                "relatedSpdxElement": spdx_id(&pkgs[r]),
            })
        })
        .collect();
    for &i in listed {
        for &(j, _) in &pkgs[i].deps {
            if !listed.contains(&j) {
                continue;
            }
            relationships.push(serde_json::json!({
                "spdxElementId": spdx_id(&pkgs[i]),
                "relationshipType": if build.contains(&j) { "BUILD_DEPENDENCY_OF" } else { "DEPENDS_ON" },
                "relatedSpdxElement": spdx_id(&pkgs[j]),
            }));
        }
    }
    // SPDX says the dependency is *of* the depender for BUILD_DEPENDENCY_OF.
    for r in relationships.iter_mut() {
        if r["relationshipType"] == "BUILD_DEPENDENCY_OF" {
            let a = r["spdxElementId"].take();
            let b = r["relatedSpdxElement"].take();
            r["spdxElementId"] = b;
            r["relatedSpdxElement"] = a;
        }
    }
    serde_json::json!({
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": name,
        "documentNamespace": format!("https://spdx.org/spdxdocs/{name}-{serial}"),
        "creationInfo": {
            "created": stamp,
            "creators": [format!("Tool: rune-{}", env!("CARGO_PKG_VERSION"))],
        },
        "packages": packages,
        "relationships": relationships,
    })
}

pub fn sbom(args: SbomArgs) -> Result<()> {
    let ws = Workspace::load(args.manifest.manifest_path.as_deref())?;
    let offline = args.manifest.offline || ws.config.settings.offline();
    let pkgs = graph::inventory(&ws, offline)?;
    let selected: Vec<&String> = if args.package.is_empty() {
        ws.members.iter().map(|m| &m.name).collect()
    } else {
        for p in &args.package {
            if ws.member_named(p).is_none() {
                anyhow::bail!("package `{p}` is not a member of this workspace");
            }
        }
        args.package.iter().collect()
    };
    let roots: Vec<usize> = pkgs
        .iter()
        .enumerate()
        .filter(|(_, p)| p.member && selected.contains(&&p.name))
        .map(|(i, _)| i)
        .collect();
    let listed = reachable(&pkgs, &roots, args.dev);
    let build = build_only(&pkgs, &roots, &listed);

    // `SOURCE_DATE_EPOCH` makes the document reproducible.
    let secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        });
    let stamp = rfc3339(secs);
    let name = if roots.len() == 1 {
        pkgs[roots[0]].name.clone()
    } else {
        ws.root()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "workspace".into())
    };
    let serial = uuid(&format!(
        "{name}\0{secs}\0{}",
        listed
            .iter()
            .map(|&i| purl(&pkgs[i]))
            .collect::<Vec<_>>()
            .join("\0")
    ));
    let doc = match args.format.as_str() {
        "spdx" => spdx(&pkgs, &roots, &listed, &build, &stamp, &serial, &name),
        _ => cyclonedx(&pkgs, &roots, &listed, &build, &stamp, &serial),
    };
    let text = serde_json::to_string_pretty(&doc)?;
    match &args.output {
        Some(path) => {
            std::fs::write(path, text + "\n")?;
            ui::summary(format!(
                "{} packages written to {} ({})",
                listed.len(),
                path.display(),
                if args.format == "spdx" {
                    "SPDX 2.3"
                } else {
                    "CycloneDX 1.5"
                }
            ));
        }
        None => println!("{text}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, version: &str) -> InventoryPackage {
        InventoryPackage {
            name: name.into(),
            version: semver::Version::parse(version).unwrap(),
            source: None,
            checksum: None,
            meta: Default::default(),
            member: false,
            git: None,
            registry: Some(rune_common::CRATES_IO_INDEX.into()),
            deps: Vec::new(),
        }
    }

    #[test]
    fn purls_name_the_source_when_it_is_not_crates_io() {
        assert_eq!(purl(&pkg("serde", "1.0.228")), "pkg:cargo/serde@1.0.228");
        let mut g = pkg("x", "0.1.0");
        g.registry = None;
        g.git = Some(("https://github.com/a/x".into(), "abc123".into()));
        assert_eq!(
            purl(&g),
            "pkg:cargo/x@0.1.0?vcs_url=git%2Bhttps://github.com/a/x%40abc123"
        );
        let mut r = pkg("y", "2.0.0");
        r.registry = Some("sparse+https://my.registry/index/".into());
        assert_eq!(
            purl(&r),
            "pkg:cargo/y@2.0.0?repository_url=https://my.registry/index/"
        );
    }

    #[test]
    fn timestamps_are_utc_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_790_596_799), "2026-09-28T11:59:59Z");
    }

    #[test]
    fn dev_dependencies_of_members_are_left_out_unless_asked_for() {
        let mut app = pkg("app", "0.1.0");
        app.member = true;
        app.deps = vec![
            (1, EdgeKind::Normal),
            (2, EdgeKind::Dev),
            (3, EdgeKind::Build),
        ];
        let pkgs = vec![
            app,
            pkg("lib", "1.0.0"),
            pkg("testkit", "1.0.0"),
            pkg("cc", "1.0.0"),
        ];
        assert_eq!(reachable(&pkgs, &[0], false), [0, 1, 3]);
        assert_eq!(reachable(&pkgs, &[0], true), [0, 1, 2, 3]);
        let listed = reachable(&pkgs, &[0], false);
        assert_eq!(build_only(&pkgs, &[0], &listed), HashSet::from([3]));
    }
}
