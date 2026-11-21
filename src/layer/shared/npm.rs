//! Validate the supported closed npm lock graph before enrollment.

use crate::api::{Fault, fault};
use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value, json};

/// Keys that make an install depend on more than its registry packages.
const INELIGIBLE: &[&str] = &[
    "workspaces",
    "bundleDependencies",
    "bundledDependencies",
    "overrides",
    "link",
    "inBundle",
];

const SECTIONS: &[&str] = &[
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
];

/// What to do instead of sharing: install in the graft itself.
const FALLBACK: &str = "Install in each graft instead: run npm ci there, or use git graft deps add NAME --lease TOKEN --manager npm. Neither needs a shared layer.";

/// Refuse a project that cannot share its install. `field` is the manifest or
/// lock key at fault and `path` the file or lock entry that holds it.
fn refuse(reason: &str, field: Option<&str>, path: Option<&str>) -> anyhow::Error {
    fault(
        "dependencies_not_shareable",
        format!("{reason}. {FALLBACK}"),
        json!({"reason": reason, "field": field, "path": path, "fallback": FALLBACK}),
    )
}

fn object(value: &Value) -> Result<&Map<String, Value>> {
    value
        .as_object()
        .context("invalid npm manifest or lock graph")
}

fn name_segment(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.~".contains(&b))
}

fn package_name(name: &str) -> bool {
    if let Some(scoped) = name.strip_prefix('@') {
        scoped
            .split_once('/')
            .is_some_and(|(scope, name)| name_segment(scope) && name_segment(name))
    } else {
        name_segment(name)
    }
}

fn package_path(mut path: &str) -> bool {
    // Each nested package is a normal npm package under a node_modules.
    loop {
        let Some(rest) = path.strip_prefix("node_modules/") else {
            return false;
        };
        let Some(boundary) = rest.find("/node_modules/") else {
            return package_name(rest);
        };
        if !package_name(&rest[..boundary]) {
            return false;
        }
        path = &rest[boundary + 1..];
    }
}

fn registry_spec(value: &Value) -> bool {
    let Some(mut spec) = value.as_str() else {
        return false;
    };
    if let Some(alias) = spec.strip_prefix("npm:") {
        if package_name(alias) && !looks_like_archive(alias) {
            return true;
        }
        let Some((name, version)) = alias.rsplit_once('@') else {
            return false;
        };
        if !package_name(name) {
            return false;
        }
        spec = version;
    }
    let spec = spec.trim();
    !spec.is_empty()
        && !spec.starts_with(['.', '-'])
        && !looks_like_archive(spec)
        && spec
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".^~*<>=| +-".contains(&b))
}

fn looks_like_archive(spec: &str) -> bool {
    let spec = spec.as_bytes();
    let ends = |suffix: &[u8]| {
        spec.get(spec.len().saturating_sub(suffix.len())..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
    };
    // npm also treats a bare tarball filename as a local dependency.
    ends(b".tgz")
        || ends(b".tar")
        || (spec.len() >= 7
            && spec[spec.len() - 7..spec.len() - 3].eq_ignore_ascii_case(b".tar")
            && ends(b"gz"))
}

fn dependencies<'a>(
    value: &'a Map<String, Value>,
    section: &str,
) -> Result<Option<&'a Map<String, Value>>> {
    let Some(value) = value.get(section) else {
        return Ok(None);
    };
    let dependencies = object(value)?;
    for (name, spec) in dependencies {
        if !(package_name(name) && registry_spec(spec)) {
            return Err(refuse(
                &format!("{section} entry {name} is a local or non-registry dependency"),
                Some(&format!("{section}.{name}")),
                None,
            ));
        }
    }
    Ok(Some(dependencies))
}

fn integrity(value: &str) -> bool {
    value.split_ascii_whitespace().any(|token| {
        let Some((algorithm, hash)) = token.split_once('-') else {
            return false;
        };
        let (len, pad) = match algorithm {
            "sha256" => (44, "="),
            "sha384" => (64, ""),
            "sha512" => (88, "=="),
            _ => return false,
        };
        hash.len() == len
            && hash.strip_suffix(pad).is_some_and(|body| {
                body.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"+/".contains(&byte))
            })
    })
}

fn remote_package<'a>(entry: &'a Value, at: &str) -> Result<&'a Map<String, Value>> {
    let entry = object(entry)?;
    if let Some(key) = INELIGIBLE.iter().find(|key| entry.contains_key(**key)) {
        return Err(refuse(
            &format!("the lockfile entry {at} is a linked, bundled or overridden package"),
            Some(key),
            Some(at),
        ));
    }
    let resolved = entry
        .get("resolved")
        .and_then(Value::as_str)
        .and_then(|url| url.strip_prefix("https://"))
        .context("shared npm packages require HTTPS registry resolutions")?;
    let (host, path) = resolved
        .split_once('/')
        .context("invalid npm registry resolution")?;
    ensure!(
        !host.is_empty()
            && !host.contains(['@', '?', '#'])
            && !path.is_empty()
            && !resolved
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control() || byte == b'\\'),
        "invalid npm registry resolution"
    );
    ensure!(
        entry
            .get("integrity")
            .and_then(Value::as_str)
            .is_some_and(integrity),
        "shared npm packages require a SHA-256 or stronger integrity pin"
    );
    ensure!(
        entry.get("version").is_some_and(registry_spec),
        "invalid shared npm package version"
    );
    Ok(entry)
}

fn validate_v2_mirror(entries: &Value) -> Result<()> {
    for (name, value) in object(entries)? {
        ensure!(package_name(name), "invalid npm package name");
        let entry = remote_package(value, &format!("dependencies.{name}"))?;
        dependencies(entry, "requires")?;
        if let Some(children) = entry.get("dependencies") {
            validate_v2_mirror(children)?;
        }
    }
    Ok(())
}

fn why(key: &str) -> &'static str {
    match key {
        "workspaces" => {
            "and npm links each workspace package into node_modules with a symlink to its own folder outside node_modules, which a shared layer cannot hold"
        }
        "overrides" => "and overridden versions are not recorded as plain registry packages",
        _ => "and a shared layer holds only plain registry packages",
    }
}

/// Check that the project's install can be shared. Every refusal is a
/// `dependencies_not_shareable` fault that names its reason and a fallback.
pub(super) fn validate(manifest: &[u8], lock: &[u8]) -> Result<()> {
    check(manifest, lock).map_err(|error| {
        if error.is::<Fault>() {
            error
        } else {
            refuse(&format!("{error:#}"), None, None)
        }
    })
}

fn check(manifest: &[u8], lock: &[u8]) -> Result<()> {
    let manifest: Value = serde_json::from_slice(manifest).context("invalid package.json")?;
    let lock: Value = serde_json::from_slice(lock).context("invalid npm lockfile")?;
    let manifest = object(&manifest)?;
    let lock = object(&lock)?;
    let version = lock.get("lockfileVersion").and_then(Value::as_u64);
    ensure!(
        matches!(version, Some(2 | 3)),
        "shared layers require npm lockfile version 2 or 3"
    );
    let packages = object(lock.get("packages").context("missing npm lock packages")?)?;
    let root = object(packages.get("").context("missing npm lock root")?)?;

    for (entry, path) in [
        (manifest, "package.json"),
        (root, "package-lock.json packages[\"\"]"),
    ] {
        if let Some(key) = INELIGIBLE.iter().find(|key| entry.contains_key(**key)) {
            return Err(refuse(
                &format!("{path} declares {key}, {}", why(key)),
                Some(key),
                Some(path),
            ));
        }
        if let Some(manager) = entry.get("packageManager") {
            ensure!(
                manager
                    .as_str()
                    .is_some_and(|manager| manager.starts_with("npm@")),
                "shared layers require an npm project"
            );
        }
    }
    for section in SECTIONS {
        let manifest = dependencies(manifest, section)?.filter(|entries| !entries.is_empty());
        let root = dependencies(root, section)?.filter(|entries| !entries.is_empty());
        ensure!(
            manifest == root,
            "manifest and lockfile dependency sections differ"
        );
        for name in manifest.into_iter().flat_map(|entries| entries.keys()) {
            ensure!(
                packages.contains_key(&format!("node_modules/{name}")),
                "direct dependency is missing a locked package"
            );
        }
    }
    for (path, entry) in packages {
        if !path.is_empty() {
            ensure!(
                package_path(path),
                "invalid package path in shared npm lock"
            );
            let entry = remote_package(entry, path)?;
            for section in SECTIONS {
                dependencies(entry, section)?;
            }
        }
    }
    if let Some(dependencies) = lock.get("dependencies") {
        ensure!(version == Some(2), "unexpected legacy npm dependency graph");
        validate_v2_mirror(dependencies)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn graph() -> (Value, Value) {
        let manifest = json!({"dependencies": {"a": "^1.0.0"}});
        let entry = json!({
            "version": "1.0.0",
            "integrity": format!("sha512-{}==", "A".repeat(86)),
            "resolved": "https://registry.example.org/a/-/a-1.0.0.tgz",
        });
        let lock =
            json!({"lockfileVersion": 3, "packages": {"": manifest, "node_modules/a": entry}});
        (manifest, lock)
    }

    fn check(manifest: &Value, lock: &Value) -> Result<()> {
        validate(&serde_json::to_vec(manifest)?, &serde_json::to_vec(lock)?)
    }

    #[test]
    fn rejects_local_transitive_packages_and_unpinned_resolutions() -> Result<()> {
        let (manifest, lock) = graph();
        check(&manifest, &lock)?;
        for spec in [
            "../local",
            "./local",
            "archive.tgz",
            "archive.tar.gz",
            "workspace:*",
            "file:../local",
            "npm:a@file:../local",
            "npm:archive.tgz",
        ] {
            let mut local = lock.clone();
            local["packages"]["node_modules/a"]["dependencies"] = json!({"embedded": spec});
            assert!(check(&manifest, &local).is_err(), "{spec}");
        }
        for (key, value) in [
            ("link", json!(true)),
            ("resolved", json!("file:archive.tgz")),
            ("integrity", json!("sha512-invalid")),
        ] {
            let mut unsafe_lock = lock.clone();
            unsafe_lock["packages"]["node_modules/a"][key] = value;
            assert!(check(&manifest, &unsafe_lock).is_err());
        }
        let mut unsafe_lock = lock;
        let entry = unsafe_lock["packages"]["node_modules/a"].clone();
        unsafe_lock["packages"]["packages/workspace"] = entry;
        assert!(check(&manifest, &unsafe_lock).is_err());
        Ok(())
    }

    fn refusal(manifest: &Value, lock: &Value) -> Value {
        crate::api::error_json(&check(manifest, lock).unwrap_err())
    }

    #[test]
    fn refusals_are_typed_with_the_field_and_a_fallback() {
        let (manifest, lock) = graph();
        let mut workspaces = manifest.clone();
        workspaces["workspaces"] = json!(["packages/*"]);
        let reported = refusal(&workspaces, &lock);
        assert_eq!(reported["code"], "dependencies_not_shareable", "{reported}");
        assert_eq!(reported["details"]["field"], "workspaces");
        assert_eq!(reported["details"]["path"], "package.json");
        assert!(
            reported["details"]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("workspaces")),
            "{reported}"
        );
        assert!(
            reported["details"]["fallback"]
                .as_str()
                .is_some_and(
                    |fallback| fallback.contains("npm ci") && fallback.contains("deps add")
                ),
            "{reported}"
        );

        let mut linked = lock.clone();
        linked["packages"]["node_modules/a"]["link"] = json!(true);
        let reported = refusal(&manifest, &linked);
        assert_eq!(reported["code"], "dependencies_not_shareable", "{reported}");
        assert_eq!(reported["details"]["field"], "link");
        assert_eq!(reported["details"]["path"], "node_modules/a");

        let mut local = manifest.clone();
        local["dependencies"]["a"] = json!("file:../a");
        let reported = refusal(&local, &lock);
        assert_eq!(reported["code"], "dependencies_not_shareable", "{reported}");
        assert_eq!(reported["details"]["field"], "dependencies.a");

        let mut other = lock.clone();
        other["lockfileVersion"] = json!(1);
        let reported = refusal(&manifest, &other);
        assert_eq!(reported["code"], "dependencies_not_shareable", "{reported}");
        assert!(reported["details"]["field"].is_null());
    }

    #[test]
    fn validates_nested_lock_v2_resolutions() -> Result<()> {
        let (manifest, mut lock) = graph();
        let entry = lock["packages"]["node_modules/a"].clone();
        lock["lockfileVersion"] = json!(2);
        lock["dependencies"] = json!({"a": entry});
        lock["dependencies"]["a"]["dependencies"] = json!({"nested": entry});
        check(&manifest, &lock)?;
        lock["dependencies"]["a"]["dependencies"]["nested"]["version"] = json!("file:foo");
        assert!(check(&manifest, &lock).is_err());
        Ok(())
    }
}
