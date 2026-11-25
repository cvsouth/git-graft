//! Copy untracked and ignored files from the invoking checkout into a new
//! graft, and recognise those copies later while they are unchanged.
//!
//! Nothing is copied by default. The caller names `.gitignore`-style
//! patterns when it adds a graft, such as `.env` or `*` for every file Git
//! does not track. When it names none, the `include` lists of the project's
//! `.graftconfig` and `.graftconfig.local` supply them (see
//! [`crate::config`]); without either, nothing is copied.
//!
//! Copies are clones where the filesystem supports them. A copy never
//! replaces a file of the checkout, never writes through a symlink, and is
//! listed in the graft's `.git/info/exclude` when the graft would otherwise
//! see it as untracked, so `merge --commit` leaves it out. Each copy's file
//! identity is recorded, and `merge` and `remove` do not count a copy as
//! local work until it changes.
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use crate::config::Config;
use crate::git;

/// Where a graft records the copies it was created with, inside its `.git`.
const MANIFEST: &str = "graft-included";
/// Most skipped paths a receipt names.
const LISTED_SKIPS: usize = 50;
/// Most entries `local-files` lists.
const LISTED_ENTRIES: usize = 500;

/// The patterns that choose what a new graft copies.
#[derive(Debug)]
pub(crate) enum Patterns {
    /// Named by the request.
    Requested(Vec<String>),
    /// Read from the project's configuration files, which are named.
    Config {
        patterns: Vec<String>,
        files: Vec<&'static str>,
    },
}

impl Patterns {
    fn list(&self) -> &[String] {
        match self {
            Self::Requested(list) | Self::Config { patterns: list, .. } => list,
        }
    }
}

/// Reject patterns Git cannot take as one exclude line.
pub(crate) fn validate(requested: &[String]) -> Result<()> {
    for pattern in requested {
        ensure!(
            !pattern.contains(['\n', '\r', '\0']),
            "include patterns must each be a single line"
        );
    }
    Ok(())
}

/// The patterns a new graft copies with: the request's when it names any,
/// otherwise the project configuration's, otherwise none. An explicitly
/// empty request copies nothing.
pub(crate) fn select(requested: Option<&[String]>, config: &Config) -> Result<Option<Patterns>> {
    if let Some(list) = requested {
        validate(list)?;
        let list: Vec<String> = list
            .iter()
            .filter(|pattern| !pattern.trim().is_empty())
            .cloned()
            .collect();
        return Ok((!list.is_empty()).then_some(Patterns::Requested(list)));
    }
    Ok((!config.include.is_empty()).then(|| Patterns::Config {
        patterns: config.include.clone(),
        files: config.include_files.clone(),
    }))
}

fn git_output(dir: &Path, args: &[OsString]) -> Result<Vec<u8>> {
    let out = git::command(dir)
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .output()
        .context("start Git")?;
    ensure!(
        out.status.success(),
        "git ls-files in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr).trim_end()
    );
    Ok(out.stdout)
}

fn records(raw: &[u8]) -> impl Iterator<Item = &[u8]> {
    raw.split(|&byte| byte == 0).filter(|name| !name.is_empty())
}

/// Every file in `origin` that Git does not track and `patterns` match, as
/// Git prints it. A nested repository is one entry ending in `/`.
fn candidates(origin: &Path, patterns: &Patterns) -> Result<Vec<Vec<u8>>> {
    let mut args: Vec<OsString> = ["ls-files", "-z", "--others", "--ignored"]
        .map(OsString::from)
        .into();
    // Git takes the patterns in order and the last that matches decides,
    // so a `!pattern` re-includes what an earlier pattern matched.
    args.extend(
        patterns
            .list()
            .iter()
            .map(|pattern| format!("--exclude={pattern}").into()),
    );
    args.push("--".into());
    let raw = git_output(origin, &args)?;
    Ok(records(&raw).map(<[u8]>::to_vec).collect())
}

fn relative(name: &[u8]) -> Option<PathBuf> {
    #[cfg(unix)]
    let name = {
        use std::os::unix::ffi::OsStrExt;
        OsStr::from_bytes(name)
    };
    #[cfg(not(unix))]
    let name = OsStr::new(std::str::from_utf8(name).ok()?);
    let path = Path::new(name);
    let normal = path.components().next().is_some()
        && path.components().all(|c| matches!(c, Component::Normal(_)));
    normal.then(|| path.to_path_buf())
}

fn name_bytes(path: &Path) -> Option<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(path.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        // Git prints paths with forward slashes on every platform.
        Some(path.to_str()?.replace('\\', "/").into_bytes())
    }
}

/// The identity of an installed copy. Any write changes it: on Unix the
/// change time moves with every write, and a replaced file has a new inode.
fn stamp(meta: &fs::Metadata) -> String {
    let kind = if meta.file_type().is_symlink() {
        'l'
    } else {
        'f'
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!(
            "{kind}:{}:{}:{}.{}:{}.{}",
            meta.len(),
            meta.ino(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec()
        )
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        format!(
            "{kind}:{}:{}:{}",
            meta.len(),
            meta.last_write_time(),
            meta.creation_time()
        )
    }
}

enum Place {
    Free,
    Exists,
    Blocked,
}

/// Whether `rel` can be created in `root`: every existing ancestor is a real
/// directory, never a symlink or file, and nothing is at `rel` yet.
fn place(root: &Path, rel: &Path) -> io::Result<Place> {
    let mut dir = root.to_path_buf();
    let parts: Vec<_> = rel.components().collect();
    for part in &parts[..parts.len() - 1] {
        dir.push(part);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => return Ok(Place::Blocked),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Place::Free),
            Err(error) => return Err(error),
        }
    }
    match fs::symlink_metadata(root.join(rel)) {
        Ok(_) => Ok(Place::Exists),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Place::Free),
        Err(error) => Err(error),
    }
}

/// Clone `src` to the new file `dst`, or copy it where cloning is unsupported.
fn transfer(src: &Path, dst: &Path) -> io::Result<()> {
    match crate::storage::clone_file(src, dst) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            Err(error)
        }
        Err(_) => {
            if fs::symlink_metadata(dst).is_ok() {
                fs::remove_file(dst)?;
            }
            fs::copy(src, dst).map(drop)
        }
    }
}

#[cfg(unix)]
fn link(src: &Path, dst: &Path) -> io::Result<bool> {
    std::os::unix::fs::symlink(fs::read_link(src)?, dst).map(|()| true)
}
#[cfg(not(unix))]
fn link(_src: &Path, _dst: &Path) -> io::Result<bool> {
    Ok(false)
}

/// The names Git ignores in `graft` among `names`.
fn ignored(graft: &Path, names: &[Vec<u8>]) -> Result<HashSet<Vec<u8>>> {
    let mut child = git::command(graft)
        .args(["check-ignore", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start Git")?;
    let mut stdin = child.stdin.take().context("Git stdin")?;
    let out = std::thread::scope(|scope| {
        let writer = scope.spawn(move || -> io::Result<()> {
            for name in names {
                stdin.write_all(name)?;
                stdin.write_all(b"\0")?;
            }
            Ok(())
        });
        let out = child.wait_with_output();
        (crate::workspace::joined(writer), out)
    });
    let (written, out) = out;
    written.context("write paths to git check-ignore")?;
    let out = out.context("wait for git check-ignore")?;
    // Exit status 1 means no path is ignored.
    ensure!(
        matches!(out.status.code(), Some(0 | 1)),
        "git check-ignore in {}: {}",
        graft.display(),
        String::from_utf8_lossy(&out.stderr).trim_end()
    );
    Ok(records(&out.stdout).map(<[u8]>::to_vec).collect())
}

/// `name` as an anchored `.gitignore` line matching only that path, or
/// `None` when the syntax cannot express it.
fn exclude_line(name: &[u8]) -> Option<Vec<u8>> {
    if name.iter().any(|&byte| matches!(byte, b'\n' | b'\r')) {
        return None;
    }
    let mut line = vec![b'/'];
    for &byte in name {
        if matches!(byte, b'\\' | b'*' | b'?' | b'[' | b']' | b'!' | b' ' | b'#') {
            line.push(b'\\');
        }
        line.push(byte);
    }
    line.push(b'\n');
    Some(line)
}

/// Add `names` to the graft's `.git/info/exclude`.
fn exclude(graft: &Path, names: &[Vec<u8>]) -> Result<()> {
    let lines: Vec<u8> = names
        .iter()
        .filter_map(|name| exclude_line(name))
        .flatten()
        .collect();
    if lines.is_empty() {
        return Ok(());
    }
    let info = graft.join(".git").join("info");
    fs::create_dir_all(&info)?;
    let path = info.join("exclude");
    let existing = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)?;
    if !existing.is_empty() && !existing.ends_with(b"\n") {
        file.write_all(b"\n")?;
    }
    file.write_all(b"# Untracked files git-graft copied from the checkout\n")?;
    file.write_all(&lines)?;
    file.sync_all()?;
    Ok(())
}

/// Copy what `patterns` match from `origin` into the new graft at `graft`,
/// returning the receipt recorded as `storage.included`. With a sparse
/// checkout, paths the graft's index tracks are left out too. Nothing is
/// copied into an `attached` install directory: the primary's install can
/// hold caches or extra packages the attached one lacks, and adding them
/// would make the install count as the graft's own changes.
pub(crate) fn copy(
    origin: &Path,
    graft: &Path,
    patterns: &Patterns,
    sparse: bool,
    attached: &[&str],
) -> Result<Value> {
    let names = candidates(origin, patterns)?;
    let tracked: HashSet<Vec<u8>> = if sparse && !names.is_empty() {
        let raw = git_output(graft, &["ls-files".into(), "-z".into()])?;
        records(&raw).map(<[u8]>::to_vec).collect()
    } else {
        HashSet::new()
    };
    let mut manifest = Vec::new();
    let mut copied = Vec::new();
    let mut bytes = 0u64;
    let mut skipped = Vec::new();
    let mut skip = |name: &[u8], reason: &str| {
        skipped.push(json!({"path": String::from_utf8_lossy(name), "reason": reason}));
    };
    let mut reported = HashSet::new();
    for name in names {
        if let Some(directory) = attached.iter().find(|directory| {
            name.strip_prefix(directory.as_bytes())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(b"/"))
        }) {
            // One entry per install, not one per file inside it.
            if reported.insert(*directory) {
                skip(format!("{directory}/").as_bytes(), "attached_install");
            }
            continue;
        }
        if name.ends_with(b"/") {
            skip(&name, "nested_repository");
            continue;
        }
        let Some(rel) = relative(&name) else {
            skip(&name, "invalid_path");
            continue;
        };
        if tracked.contains(&name) {
            skip(&name, "tracked");
            continue;
        }
        match place(graft, &rel)? {
            Place::Free => {}
            Place::Exists => {
                skip(&name, "exists");
                continue;
            }
            Place::Blocked => {
                skip(&name, "blocked");
                continue;
            }
        }
        let src = origin.join(&rel);
        let dst = graft.join(&rel);
        let meta = match fs::symlink_metadata(&src) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                skip(&name, "vanished");
                continue;
            }
            Err(error) => return Err(error).with_context(|| format!("inspect {}", src.display())),
        };
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let kind = meta.file_type();
        let result = if kind.is_file() {
            transfer(&src, &dst).map(|()| true)
        } else if kind.is_symlink() {
            link(&src, &dst)
        } else {
            Ok(false)
        };
        match result {
            Ok(true) => {}
            Ok(false) => {
                skip(
                    &name,
                    if kind.is_symlink() {
                        "symlink"
                    } else {
                        "special"
                    },
                );
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                skip(&name, "vanished");
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                skip(&name, "unreadable");
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("copy {}", src.display()));
            }
        }
        let installed = fs::symlink_metadata(&dst)?;
        bytes += installed.len();
        manifest.extend_from_slice(stamp(&installed).as_bytes());
        manifest.push(b'\t');
        manifest.extend_from_slice(&name);
        manifest.push(0);
        copied.push(name);
    }
    if !copied.is_empty() {
        let ignored = ignored(graft, &copied)?;
        let untracked: Vec<_> = copied
            .iter()
            .filter(|name| !ignored.contains(*name))
            .cloned()
            .collect();
        exclude(graft, &untracked)?;
        // Record the copies only once they are excluded, so a manifest
        // never vouches for a file `merge --commit` would stage.
        let path = graft.join(".git").join(MANIFEST);
        let mut file = fs::File::create(&path)?;
        file.write_all(&manifest)?;
        file.sync_all()?;
    }
    let mut receipt = match patterns {
        Patterns::Requested(list) => json!({"source": "request", "patterns": list}),
        Patterns::Config { patterns, files } => {
            json!({"source": "config", "config_files": files, "patterns": patterns})
        }
    };
    receipt["files"] = json!(copied.len());
    receipt["bytes"] = json!(bytes);
    receipt["skipped_count"] = json!(skipped.len());
    skipped.truncate(LISTED_SKIPS);
    receipt["skipped"] = json!(skipped);
    Ok(receipt)
}

fn manifest(graft: &Path) -> Option<HashMap<Vec<u8>, Vec<u8>>> {
    let raw = fs::read(graft.join(".git").join(MANIFEST)).ok()?;
    records(&raw)
        .map(|record| {
            let tab = record.iter().position(|&byte| byte == b'\t')?;
            Some((record[tab + 1..].to_vec(), record[..tab].to_vec()))
        })
        .collect()
}

fn unchanged_file(graft: &Path, copies: &HashMap<Vec<u8>, Vec<u8>>, name: &[u8]) -> bool {
    let Some(recorded) = copies.get(name) else {
        return false;
    };
    relative(name)
        .and_then(|rel| fs::symlink_metadata(graft.join(rel)).ok())
        .is_some_and(|meta| stamp(&meta).as_bytes() == recorded.as_slice())
}

/// Whether every file below the ignored directory `name` is an unchanged copy.
fn unchanged_dir(graft: &Path, copies: &HashMap<Vec<u8>, Vec<u8>>, name: &[u8]) -> bool {
    let Some(rel) = relative(name.strip_suffix(b"/").unwrap_or(name)) else {
        return false;
    };
    let mut pending = vec![rel];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(graft.join(&dir)) else {
            return false;
        };
        for entry in entries {
            let Ok(entry) = entry else { return false };
            let rel = dir.join(entry.file_name());
            let Ok(kind) = entry.file_type() else {
                return false;
            };
            if kind.is_dir() {
                pending.push(rel);
            } else if !name_bytes(&rel).is_some_and(|name| unchanged_file(graft, copies, &name)) {
                return false;
            }
        }
    }
    true
}

/// Drop from `found` the ignored files the graft was created with while each
/// is still the installed copy. Every other change, and any copy the graft
/// changed, replaced or deleted, still counts as local work.
pub(crate) fn without_unchanged_copies(
    graft: &Path,
    storage: &Value,
    found: Vec<git::Change>,
) -> Vec<git::Change> {
    let recorded = storage["included"]["files"].as_u64().is_some_and(|n| n > 0);
    if !recorded || !found.iter().any(|change| change.status == "!!") {
        return found;
    }
    let Some(copies) = manifest(graft) else {
        return found;
    };
    found
        .into_iter()
        .filter(|change| {
            change.status != "!!"
                || !if change.name.ends_with(b"/") {
                    unchanged_dir(graft, &copies, &change.name)
                } else {
                    unchanged_file(graft, &copies, &change.name)
                }
        })
        .collect()
}

/// The untracked and ignored files and directories in `origin` that `add
/// --include` can copy, with the default patterns `config` sets.
pub(crate) fn overview(origin: &Path, config: &Config) -> Result<Value> {
    ensure!(
        origin.join(".git").symlink_metadata().is_ok(),
        "this repository is bare, so it has no untracked or ignored files"
    );
    let listing = |ignored: bool| {
        let mut args: Vec<OsString> = ["ls-files", "-z", "--others", "--directory"]
            .map(OsString::from)
            .into();
        if ignored {
            args.push("--ignored".into());
        }
        args.extend(["--exclude-standard".into(), "--".into()]);
        git_output(origin, &args)
    };
    let (untracked, ignored) = std::thread::scope(|scope| {
        let untracked = scope.spawn(|| listing(false));
        let ignored = listing(true);
        (crate::workspace::joined(untracked), ignored)
    });
    let (untracked, ignored) = (untracked?, ignored?);
    let mut entries: Vec<(&[u8], &str)> = records(&untracked)
        .map(|name| (name, "untracked"))
        .chain(records(&ignored).map(|name| (name, "ignored")))
        .collect();
    entries.sort_unstable();
    let listed: Vec<Value> = entries
        .iter()
        .take(LISTED_ENTRIES)
        .map(|(name, status)| {
            let kind = if name.ends_with(b"/") {
                "directory"
            } else {
                "file"
            };
            json!({"path": String::from_utf8_lossy(name), "status": status, "kind": kind})
        })
        .collect();
    Ok(json!({
        "checkout": origin,
        "entries": listed,
        "count": entries.len(),
        "truncated": entries.len() > LISTED_ENTRIES,
        "defaults": config.include_defaults(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = git::command(dir).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    /// A primary with a committed `.gitignore`, ignored and untracked files,
    /// and a fresh clone standing in for the graft.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let primary = root.path().join("primary");
        fs::create_dir(&primary).unwrap();
        git(&primary, &["init", "-q", "-b", "main"]);
        git(&primary, &["config", "user.name", "t"]);
        git(&primary, &["config", "user.email", "t@example.invalid"]);
        git(&primary, &["config", "core.autocrlf", "false"]);
        write(&primary, ".gitignore", ".env\nbuild/\n*.log\n");
        write(&primary, "src/main.txt", "tracked\n");
        git(&primary, &["add", "."]);
        git(&primary, &["commit", "-qm", "initial"]);
        write(&primary, ".env", "SECRET=1\n");
        write(&primary, "build/out/a.bin", "built\n");
        write(&primary, "debug.log", "log\n");
        write(&primary, "notes.txt", "scratch\n");
        write(&primary, "src/main.txt", "edited in the primary\n");
        let graft = root.path().join("graft");
        let out = std::process::Command::new("git")
            .args(["clone", "-q", "-c", "core.autocrlf=false"])
            .arg(&primary)
            .arg(&graft)
            .output()
            .unwrap();
        assert!(out.status.success());
        (root, primary, graft)
    }

    fn requested(list: &[&str]) -> Patterns {
        Patterns::Requested(list.iter().map(|s| (*s).to_owned()).collect())
    }

    #[test]
    fn nothing_is_selected_without_a_request_or_configured_patterns() {
        let (_root, primary, _) = fixture();
        let none = Config::read(&primary).unwrap();
        assert!(select(None, &none).unwrap().is_none());
        assert!(select(Some(&[]), &none).unwrap().is_none());
        assert!(select(Some(&[" ".into()]), &none).unwrap().is_none());
        assert!(select(Some(&["a\nb".into()]), &none).is_err());
        write(&primary, crate::config::SHARED, r#"{"include": [".env"]}"#);
        let config = Config::read(&primary).unwrap();
        assert!(matches!(
            select(None, &config).unwrap(),
            Some(Patterns::Config { patterns, files }) if patterns == [".env"] && files == [".graftconfig"]
        ));
        // A request replaces the configured patterns, and an empty one copies nothing.
        assert!(select(Some(&[]), &config).unwrap().is_none());
        assert!(matches!(
            select(Some(&["*.log".into()]), &config).unwrap(),
            Some(Patterns::Requested(list)) if list == ["*.log"]
        ));
    }

    #[test]
    fn copies_only_matching_untracked_files_and_never_tracked_ones() {
        let (_root, primary, graft) = fixture();
        let receipt = copy(
            &primary,
            &graft,
            &requested(&[".env", "notes.txt", "src/*"]),
            false,
            &[],
        )
        .unwrap();
        assert_eq!(receipt["files"], 2, "{receipt}");
        assert_eq!(
            fs::read_to_string(graft.join(".env")).unwrap(),
            "SECRET=1\n"
        );
        assert_eq!(
            fs::read_to_string(graft.join("notes.txt")).unwrap(),
            "scratch\n"
        );
        // The tracked file keeps the graft's committed contents.
        assert_eq!(
            fs::read_to_string(graft.join("src/main.txt")).unwrap(),
            "tracked\n"
        );
        assert!(!graft.join("debug.log").exists());
        // The untracked copy is excluded, so status is clean apart from ignored files.
        let status = git::changes(&graft).unwrap();
        let statuses: Vec<_> = status
            .iter()
            .map(|c| (c.status.as_str(), c.path.as_str()))
            .collect();
        assert!(
            statuses.iter().all(|(status, _)| *status == "!!"),
            "{statuses:?}"
        );
    }

    #[test]
    fn everything_copies_with_a_star_and_unchanged_copies_are_not_local_work() {
        let (_root, primary, graft) = fixture();
        let receipt = copy(&primary, &graft, &requested(&["*"]), false, &[]).unwrap();
        assert_eq!(receipt["files"], 4, "{receipt}");
        let storage = json!({"included": receipt});
        let found = git::changes(&graft).unwrap();
        assert!(!found.is_empty());
        assert!(without_unchanged_copies(&graft, &storage, found).is_empty());

        // An edited copy and a new ignored file count again.
        fs::write(graft.join(".env"), "SECRET=2\n").unwrap();
        write(&graft, "build/out/new.bin", "new\n");
        let left = without_unchanged_copies(&graft, &storage, git::changes(&graft).unwrap());
        let paths: Vec<_> = left.iter().map(|change| change.path.as_str()).collect();
        assert_eq!(paths, [".env", "build/"]);
    }

    #[test]
    fn nothing_is_copied_into_an_attached_install() {
        let (_root, primary, graft) = fixture();
        write(&primary, "node_modules/pkg/index.js", "primary\n");
        write(&primary, "node_modules/.cache/entry", "cache\n");
        write(&primary, "node_modules_notes.txt", "kept\n");
        // The graft's attached install has the package but not the cache.
        write(&graft, "node_modules/pkg/index.js", "attached\n");
        let receipt = copy(
            &primary,
            &graft,
            &requested(&["*"]),
            false,
            &["node_modules"],
        )
        .unwrap();
        assert!(!graft.join("node_modules/.cache").exists(), "{receipt}");
        assert_eq!(
            fs::read_to_string(graft.join("node_modules/pkg/index.js")).unwrap(),
            "attached\n"
        );
        assert!(graft.join("node_modules_notes.txt").exists());
        let attached: Vec<_> = receipt["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["reason"] == "attached_install")
            .collect();
        assert_eq!(attached.len(), 1, "{receipt}");
        assert_eq!(attached[0]["path"], "node_modules/");
    }

    #[test]
    fn configured_patterns_supply_the_default_and_a_local_bang_reincludes() {
        let (_root, primary, graft) = fixture();
        write(&primary, "keep.env", "KEEP=1\n");
        write(
            &primary,
            crate::config::SHARED,
            r#"{"include": ["*.log", "*.env", ".env"]}"#,
        );
        write(
            &primary,
            crate::config::LOCAL,
            r#"{"include": ["!*.log", "notes.txt", "!keep.env"]}"#,
        );
        let config = Config::read(&primary).unwrap();
        let patterns = select(None, &config).unwrap().unwrap();
        let receipt = copy(&primary, &graft, &patterns, false, &[]).unwrap();
        assert_eq!(receipt["source"], "config");
        assert_eq!(
            receipt["config_files"],
            json!([".graftconfig", ".graftconfig.local"])
        );
        assert_eq!(
            receipt["patterns"],
            json!(["*.env", ".env", "notes.txt", "!keep.env"])
        );
        // .env and notes.txt; *.log was removed and keep.env re-excluded.
        assert_eq!(receipt["files"], 2, "{receipt}");
        assert!(graft.join(".env").is_file() && graft.join("notes.txt").is_file());
        assert!(!graft.join("debug.log").exists() && !graft.join("keep.env").exists());
    }

    #[cfg(unix)]
    #[test]
    fn never_writes_through_a_symlink_in_the_graft() {
        let (root, primary, graft) = fixture();
        let outside = root.path().join("outside");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, graft.join("build")).unwrap();
        let receipt = copy(&primary, &graft, &requested(&["build/"]), false, &[]).unwrap();
        assert_eq!(receipt["files"], 0);
        assert_eq!(receipt["skipped"][0]["reason"], "blocked");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn overview_lists_untracked_and_ignored_entries() {
        let (_root, primary, _) = fixture();
        let listed = overview(&primary, &Config::read(&primary).unwrap()).unwrap();
        let entries: Vec<_> = listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["path"].as_str().unwrap(),
                    entry["status"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            entries,
            [
                (".env", "ignored"),
                ("build/", "ignored"),
                ("debug.log", "ignored"),
                ("notes.txt", "untracked"),
            ]
        );
        assert_eq!(listed["defaults"], json!({"patterns": [], "files": []}));
    }

    #[test]
    fn exclude_lines_match_only_their_path() {
        assert_eq!(exclude_line(b"a b/*.env").unwrap(), b"/a\\ b/\\*.env\n");
        assert!(exclude_line(b"a\nb").is_none());
    }
}
