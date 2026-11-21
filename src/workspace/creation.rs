//! Recovery of a graft creation that stopped before it finished.
//!
//! Creation publishes the checkout's index only after every checkout file is
//! in place. A reservation without `.git/index` therefore never held a
//! complete checkout. While it is unfinished, Graft's marked
//! `.git/index.lock` makes Git refuse to write an index, so the missing files
//! cannot be committed as deletions. A retry of the same request, or a
//! restore of the archived creation, either finishes it or keeps that guard.
//!
//! Call these functions only while holding the graft's workspace lock. Every
//! Graft process that creates a graft holds it until it exits, so a marked
//! lock found under it belongs to a process that has exited.
use std::{
    fs,
    io::{self, Write},
    path::Path,
};

use anyhow::{Result, ensure};

use super::registry::MARKER;
use crate::registry::{exists, owned_directory};
use crate::storage::CREATION_LOCK;

/// How far an interrupted creation got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Progress {
    /// Only Git metadata exists, and any index lock is Graft's. The creation
    /// can start again from its reservation.
    Unpublished,
    /// The checkout's index was published, so every checkout file was written.
    Published,
    /// Checkout files, or an index lock Graft did not create, are present.
    /// They are kept, and the creation cannot be finished in place.
    Partial,
}

/// Classify the reservation at `root` without changing it.
pub(super) fn progress(root: &Path) -> Result<Progress> {
    let admin = root.join(".git");
    owned_directory(&admin)?;
    if exists(&admin.join("index"))? {
        return Ok(Progress::Published);
    }
    for entry in fs::read_dir(root)? {
        if entry?.file_name() != ".git" {
            return Ok(Progress::Partial);
        }
    }
    let lock = admin.join("index.lock");
    if exists(&lock)? && !graft_lock(&lock)? {
        // A Git process may still be writing this checkout.
        return Ok(Progress::Partial);
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

/// Return an unpublished creation to its reservation, so only Graft's
/// workspace marker remains in `.git`. Nothing outside `.git` exists, and
/// without an index Git never recorded a checkout there.
pub(super) fn reclaim(root: &Path) -> Result<()> {
    ensure!(
        progress(root)? == Progress::Unpublished,
        "the interrupted graft holds checkout files or a Git lock, so it was kept"
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

    /// A reservation: the graft root with `.git` holding only the marker.
    fn reservation() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::write(root.path().join(".git").join(MARKER), "{}").unwrap();
        root
    }

    /// What a creation killed during its checkout leaves: Git metadata, a
    /// private scratch checkout and Graft's index lock, but no index.
    fn interrupted() -> tempfile::TempDir {
        let root = reservation();
        let admin = root.path().join(".git");
        fs::write(admin.join("HEAD"), "ref: refs/heads/graft/x\n").unwrap();
        fs::create_dir_all(admin.join("objects/info")).unwrap();
        fs::create_dir_all(admin.join(".graft-local-abc/tree")).unwrap();
        fs::write(admin.join(".graft-local-abc/tree/payload.dat"), "x").unwrap();
        fs::write(admin.join("index.lock"), CREATION_LOCK).unwrap();
        root
    }

    #[test]
    fn a_killed_checkout_is_reclaimed_to_its_reservation() {
        let root = interrupted();
        assert_eq!(progress(root.path()).unwrap(), Progress::Unpublished);
        reclaim(root.path()).unwrap();
        let left: Vec<_> = fs::read_dir(root.path().join(".git"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, [MARKER]);
        assert_eq!(progress(root.path()).unwrap(), Progress::Unpublished);
    }

    #[test]
    fn checkout_files_or_a_foreign_lock_are_kept() {
        let root = interrupted();
        fs::write(root.path().join("payload.dat"), "partial").unwrap();
        assert_eq!(progress(root.path()).unwrap(), Progress::Partial);
        assert!(reclaim(root.path()).is_err());
        assert!(root.path().join(".git/HEAD").exists());

        // Git's own lock is empty while it checks out files.
        let root = interrupted();
        fs::write(root.path().join(".git/index.lock"), "").unwrap();
        assert_eq!(progress(root.path()).unwrap(), Progress::Partial);
        assert!(reclaim(root.path()).is_err());
        assert!(root.path().join(".git/HEAD").exists());
    }

    #[test]
    fn a_published_index_is_kept_and_only_graft_locks_are_released() {
        let root = interrupted();
        fs::write(root.path().join(".git/index"), "DIRC").unwrap();
        assert_eq!(progress(root.path()).unwrap(), Progress::Published);
        assert!(reclaim(root.path()).is_err());
        release(root.path()).unwrap();
        assert!(!root.path().join(".git/index.lock").exists());
        assert!(root.path().join(".git/index").exists());

        fs::write(root.path().join(".git/index.lock"), "").unwrap();
        release(root.path()).unwrap();
        assert!(root.path().join(".git/index.lock").exists());
    }

    #[test]
    fn a_guard_blocks_git_until_the_creation_is_reclaimed() {
        let root = interrupted();
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
        let root = interrupted();
        fs::remove_file(root.path().join(".git/index.lock")).unwrap();
        fs::write(root.path().join(".git/index"), "DIRC").unwrap();
        guard(root.path()).unwrap();
        assert!(!root.path().join(".git/index.lock").exists());
    }
}
