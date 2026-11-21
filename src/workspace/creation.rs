//! Recovery of a graft creation that stopped before it finished.
//!
//! Creation publishes the checkout's index only after every checkout file is
//! in place. A reservation without `.git/index` therefore never held a
//! complete checkout. While it is unfinished, Graft's marked
//! `.git/index.lock` makes Git refuse to write an index, so the missing files
//! cannot be committed as deletions. A retry of the same request, or a
//! restore of the archived creation, either builds it again or keeps it
//! guarded.
//!
//! Call these functions only while holding the graft's workspace lock. Every
//! Graft process that creates a graft holds it until it exits, so a marked
//! lock found under it belongs to a process that has exited.
use std::{
    fs,
    io::{self, Write},
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};

use super::registry::MARKER;
use crate::registry::{exists, owned_directory};
use crate::storage::CREATION_LOCK;

/// How far an interrupted creation got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Progress {
    /// Only Git metadata exists, and any index lock is Graft's.
    Unpublished,
    /// Checkout files exist beside the metadata, but no index was published.
    Written,
    /// The checkout's index was published, so every checkout file was written.
    Published,
    /// An index lock Graft did not create is present. A Git process may still
    /// be writing the checkout.
    Busy,
}

/// Classify the reservation at `root` without changing it.
pub(super) fn progress(root: &Path) -> Result<Progress> {
    let admin = root.join(".git");
    owned_directory(&admin)?;
    if exists(&admin.join("index"))? {
        return Ok(Progress::Published);
    }
    let lock = admin.join("index.lock");
    if exists(&lock)? && !graft_lock(&lock)? {
        return Ok(Progress::Busy);
    }
    for entry in fs::read_dir(root)? {
        if entry?.file_name() != ".git" {
            return Ok(Progress::Written);
        }
    }
    Ok(Progress::Unpublished)
}

/// Whether `lock` is the index lock a creation marked as Graft's.
fn graft_lock(lock: &Path) -> Result<bool> {
    match fs::symlink_metadata(lock) {
        Ok(metadata) if metadata.is_file() => Ok(fs::read(lock)? == CREATION_LOCK),
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Return an interrupted creation to its reservation, so it can be built
/// again from `base`. Checkout files are removed only when Git confirms each
/// one matches `base`. Anything else is kept, and the error says why.
pub(super) fn restart(root: &Path, base: Option<&str>) -> Result<()> {
    match progress(root)? {
        Progress::Unpublished => {}
        Progress::Written => {
            let base = base.context("the interrupted graft holds files but has no base commit")?;
            discard_checkout(root, base)?;
        }
        Progress::Published => bail!("the graft's checkout was already published"),
        Progress::Busy => bail!(
            "the graft holds a Git index lock that Graft did not create; a Git process may still be writing its checkout"
        ),
    }
    reclaim(root)
}

/// Remove an unpublished creation's Git metadata, so only Graft's workspace
/// marker remains in `.git`. Nothing outside `.git` exists, and without an
/// index Git never recorded a checkout there.
fn reclaim(root: &Path) -> Result<()> {
    ensure!(
        progress(root)? == Progress::Unpublished,
        "the interrupted graft changed while it was reclaimed"
    );
    let admin = root.join(".git");
    for entry in fs::read_dir(&admin)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == MARKER || name == "index.lock" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    // Release the lock last, so Git refuses to write an index until the
    // reservation holds nothing but the marker.
    let lock = admin.join("index.lock");
    if exists(&lock)? {
        fs::remove_file(lock)?;
    }
    Ok(())
}

/// Remove the files an interrupted checkout wrote. Git compares each present
/// file with `base` as `git status` would. Every file must be tracked by
/// `base` and unchanged, so nothing removed differs from the commit.
fn discard_checkout(root: &Path, base: &str) -> Result<()> {
    let tracked = {
        let scratch = tempfile::Builder::new()
            .prefix(".graft-recover-")
            .tempdir_in(root.join(".git"))?;
        let index = scratch.path().join("index");
        let git = |args: &[&str]| -> Result<Vec<u8>> {
            let output = crate::git::command(root)
                .env("GIT_INDEX_FILE", &index)
                .args(args)
                .output()
                .context("start Git")?;
            ensure!(
                output.status.success(),
                "git {} in the interrupted graft: {}",
                args[0],
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Ok(output.stdout)
        };
        git(&["read-tree", base])?;
        git(&["update-index", "-q", "--ignore-missing", "--refresh"])?;
        ensure!(
            git(&["ls-files", "-z", "--others"])?.is_empty(),
            "the interrupted graft holds files its commit does not track, so it was kept"
        );
        ensure!(
            git(&["diff-files", "-z", "--name-only", "--diff-filter=d"])?.is_empty(),
            "files in the interrupted graft differ from its commit, so it was kept"
        );
        git(&["ls-files", "-z"])?
    };
    for name in tracked
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let relative = git_path(name)?;
        ensure!(
            !beyond_link(root, relative)?,
            "a checkout path in the interrupted graft is beyond a symbolic link, so it was kept"
        );
        let path = root.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_dir() => fs::remove_file(&path)?,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    remove_empty_directories(root, true)?;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        ensure!(
            entry.file_name() == ".git",
            "the interrupted graft still holds {}, so it was kept",
            entry.path().display()
        );
    }
    Ok(())
}

/// A path Git printed, relative to the graft root.
fn git_path(name: &[u8]) -> Result<&Path> {
    #[cfg(unix)]
    let path = Path::new(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(name));
    #[cfg(not(unix))]
    let path = Path::new(std::str::from_utf8(name).context("checkout path is not UTF-8")?);
    ensure!(
        path.components()
            .all(|part| matches!(part, std::path::Component::Normal(_))),
        "Git printed an unexpected checkout path"
    );
    Ok(path)
}

/// Whether a parent directory of `relative` under `root` is a symbolic link.
fn beyond_link(root: &Path, relative: &Path) -> Result<bool> {
    let mut path = root.to_path_buf();
    let Some(parent) = relative.parent() else {
        return Ok(false);
    };
    for part in parent.components() {
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

/// Remove directories under `dir` that hold nothing once emptied. The graft
/// root's `.git` is skipped. Symbolic links are never followed.
fn remove_empty_directories(dir: &Path, root: bool) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if root && entry.file_name() == ".git" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            let path = entry.path();
            remove_empty_directories(&path, false)?;
            if fs::read_dir(&path)?.next().is_none() {
                fs::remove_dir(&path)?;
            }
        }
    }
    Ok(())
}

/// Keep Git from writing the index of a creation that stopped before it
/// published one. An existing index lock already does that and is kept.
pub(super) fn guard(root: &Path) -> Result<()> {
    let admin = root.join(".git");
    owned_directory(&admin)?;
    if exists(&admin.join("index"))? {
        return Ok(());
    }
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(admin.join("index.lock"))
    {
        Ok(mut file) => Ok(file.write_all(CREATION_LOCK)?),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Release Graft's lock from a creation whose index was published before it
/// stopped. A lock Graft did not create is kept for Git to report.
pub(super) fn release(root: &Path) -> Result<()> {
    let admin = root.join(".git");
    owned_directory(&admin)?;
    ensure!(
        exists(&admin.join("index"))?,
        "the graft's checkout was never published"
    );
    let lock = admin.join("index.lock");
    if graft_lock(&lock)? {
        fs::remove_file(lock)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn admin_entries(root: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = fs::read_dir(root.join(".git"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    }

    /// A graft root whose creation was killed after its checkout started:
    /// a committed repository with Graft's marker and index lock, no index,
    /// and the given checkout files still in place. Returns the commit.
    fn interrupted(root: &Path, written: &[&str]) -> String {
        git(root, &["init", "-q", "--initial-branch=main"]);
        git(root, &["config", "core.autocrlf", "false"]);
        fs::create_dir(root.join("dir")).unwrap();
        for (name, content) in [("a.txt", "a\n"), ("dir/b.txt", "b\n")] {
            fs::write(root.join(name), content).unwrap();
        }
        git(root, &["add", "."]);
        git(
            root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "base",
            ],
        );
        let base = git(root, &["rev-parse", "HEAD"]);
        fs::remove_file(root.join(".git/index")).unwrap();
        for name in ["a.txt", "dir/b.txt"] {
            if !written.contains(&name) {
                fs::remove_file(root.join(name)).unwrap();
            }
        }
        if written.iter().all(|name| !name.starts_with("dir/")) {
            fs::remove_dir(root.join("dir")).unwrap();
        }
        fs::write(root.join(".git").join(MARKER), "{}").unwrap();
        fs::write(root.join(".git/index.lock"), CREATION_LOCK).unwrap();
        base
    }

    #[test]
    fn a_creation_killed_before_writing_files_restarts_from_its_reservation() {
        let root = tempfile::tempdir().unwrap();
        interrupted(root.path(), &[]);
        assert_eq!(progress(root.path()).unwrap(), Progress::Unpublished);
        restart(root.path(), None).unwrap();
        assert_eq!(admin_entries(root.path()), [MARKER]);
        assert_eq!(progress(root.path()).unwrap(), Progress::Unpublished);
    }

    #[test]
    fn checkout_files_that_match_the_commit_are_discarded_before_a_restart() {
        let root = tempfile::tempdir().unwrap();
        let base = interrupted(root.path(), &["dir/b.txt"]);
        assert_eq!(progress(root.path()).unwrap(), Progress::Written);
        assert!(restart(root.path(), None).is_err());
        restart(root.path(), Some(&base)).unwrap();
        let left: Vec<_> = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, [".git"]);
        assert_eq!(admin_entries(root.path()), [MARKER]);
    }

    #[test]
    fn changed_or_untracked_files_keep_the_interrupted_creation() {
        for (name, content) in [("a.txt", "edited\n"), ("note.txt", "keep me\n")] {
            let root = tempfile::tempdir().unwrap();
            let base = interrupted(root.path(), &["a.txt", "dir/b.txt"]);
            fs::write(root.path().join(name), content).unwrap();
            assert!(restart(root.path(), Some(&base)).is_err());
            assert_eq!(fs::read_to_string(root.path().join(name)).unwrap(), content);
            assert_eq!(
                fs::read_to_string(root.path().join("dir/b.txt")).unwrap(),
                "b\n"
            );
            assert!(root.path().join(".git/HEAD").exists());
            assert_eq!(
                fs::read(root.path().join(".git/index.lock")).unwrap(),
                CREATION_LOCK
            );
        }
    }

    #[test]
    fn a_lock_graft_did_not_create_keeps_the_interrupted_creation() {
        // Git's own lock is empty while it checks out files.
        let root = tempfile::tempdir().unwrap();
        interrupted(root.path(), &[]);
        fs::write(root.path().join(".git/index.lock"), "").unwrap();
        assert_eq!(progress(root.path()).unwrap(), Progress::Busy);
        assert!(restart(root.path(), None).is_err());
        assert!(root.path().join(".git/HEAD").exists());
        assert!(root.path().join(".git/index.lock").exists());
    }

    #[test]
    fn a_published_index_is_kept_and_only_graft_locks_are_released() {
        let root = tempfile::tempdir().unwrap();
        let base = interrupted(root.path(), &["a.txt", "dir/b.txt"]);
        // The creation was killed after publishing its index, before it
        // released its lock. Git cannot write an index while the lock exists.
        let lock = root.path().join(".git/index.lock");
        fs::remove_file(&lock).unwrap();
        git(root.path(), &["read-tree", &base]);
        fs::write(&lock, CREATION_LOCK).unwrap();
        assert_eq!(progress(root.path()).unwrap(), Progress::Published);
        assert!(restart(root.path(), Some(&base)).is_err());
        release(root.path()).unwrap();
        assert!(!root.path().join(".git/index.lock").exists());
        assert!(root.path().join(".git/index").exists());

        fs::write(root.path().join(".git/index.lock"), "").unwrap();
        release(root.path()).unwrap();
        assert!(root.path().join(".git/index.lock").exists());
    }

    #[test]
    fn a_guard_blocks_git_until_the_creation_is_restarted() {
        let root = tempfile::tempdir().unwrap();
        interrupted(root.path(), &["a.txt"]);
        fs::remove_file(root.path().join(".git/index.lock")).unwrap();
        guard(root.path()).unwrap();
        assert_eq!(
            fs::read(root.path().join(".git/index.lock")).unwrap(),
            CREATION_LOCK
        );
        // An existing lock, Graft's or Git's, is left as it is.
        fs::write(root.path().join(".git/index.lock"), "").unwrap();
        guard(root.path()).unwrap();
        assert!(
            fs::read(root.path().join(".git/index.lock"))
                .unwrap()
                .is_empty()
        );
        // A published checkout needs no guard.
        let root = tempfile::tempdir().unwrap();
        let base = interrupted(root.path(), &["a.txt", "dir/b.txt"]);
        fs::remove_file(root.path().join(".git/index.lock")).unwrap();
        git(root.path(), &["read-tree", &base]);
        guard(root.path()).unwrap();
        assert!(!root.path().join(".git/index.lock").exists());
    }
}
