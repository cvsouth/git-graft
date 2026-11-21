//! Detect checkout paths that the filesystem resolves to an ignored entry or
//! to a person's uncommitted edit.
//!
//! Comparing path bytes cannot follow every filesystem's folding rules: APFS
//! and NTFS fold case beyond ASCII, and APFS also treats NFC and NFD spellings
//! as the same name. Instead of reimplementing those rules, ask the filesystem
//! which directory entry each incoming path resolves to and compare file
//! identities with the entries Git reported.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;

/// A file's device and file number, read without following a final symlink.
type Identity = (u64, u64);

/// Return each changed path that the filesystem resolves onto an ignored
/// entry, onto a path inside one, or onto a directory holding one, in
/// `changed` order.
///
/// `ignored` and `changed` are slash-separated paths relative to `root`, as
/// Git prints them with `-z`.
pub(crate) fn ignored_collisions<'a>(
    root: &Path,
    ignored: &[&[u8]],
    changed: &[&'a [u8]],
) -> Result<Vec<&'a [u8]>> {
    let ignored: Vec<_> = ignored
        .iter()
        .map(|path| path.strip_suffix(b"/").unwrap_or(path))
        .collect();
    let mut found: Vec<&[u8]> = resolved_collisions(root, &ignored, changed)?
        .into_iter()
        .map(|(_, changed)| changed)
        .collect();
    found.dedup();
    Ok(found)
}

/// The edited paths an update would write over: each edited path that is a
/// changed path, lies inside one or holds one, compared both as Git path
/// bytes and as the files the filesystem resolves them to. Byte comparison
/// covers an edit that removed its file; the filesystem covers a case-only or
/// Unicode spelling of an existing file. An edited path may end in `/`, as
/// Git names an untracked nested repository. Returned in `edited` order.
pub(crate) fn overlapping<'e>(
    root: &Path,
    edited: &[&'e [u8]],
    changed: &[&[u8]],
) -> Result<Vec<&'e [u8]>> {
    if edited.is_empty() || changed.is_empty() {
        return Ok(Vec::new());
    }
    let changes: BTreeSet<&[u8]> = changed.iter().copied().collect();
    let paths: Vec<&[u8]> = edited
        .iter()
        .map(|path| path.strip_suffix(b"/").unwrap_or(path))
        .collect();
    let mut found = BTreeSet::new();
    for (index, path) in paths.iter().enumerate() {
        let mut inside = path.to_vec();
        inside.push(b'/');
        let holds = changes
            .range(&inside[..]..)
            .next()
            .is_some_and(|change| change.starts_with(&inside));
        if changes.contains(path) || holds || prefixes(path).any(|p| changes.contains(p)) {
            found.insert(index);
        }
    }
    for (index, _) in resolved_collisions(root, &paths, changed)? {
        found.insert(index);
    }
    Ok(found.into_iter().map(|index| edited[index]).collect())
}

/// Pairs of an entry's index and a changed path where the filesystem resolves the
/// changed path onto the entry, onto a path inside it, or onto a directory
/// holding it, in `changed` order. Entries that do not exist on disk never
/// match.
fn resolved_collisions<'c>(
    root: &Path,
    entries: &[&[u8]],
    changed: &[&'c [u8]],
) -> Result<Vec<(usize, &'c [u8])>> {
    let mut cache = HashMap::new();
    // The entries each file identity names, and the entries inside each
    // directory identity.
    let mut named: HashMap<Identity, Vec<usize>> = HashMap::new();
    let mut holding: HashMap<Identity, Vec<usize>> = HashMap::new();
    for (index, path) in entries.iter().enumerate() {
        if let Some(identity) = identity(root, path, &mut cache)? {
            named.entry(identity).or_default().push(index);
        }
        for prefix in prefixes(path) {
            if let Some(identity) = identity(root, prefix, &mut cache)? {
                holding.entry(identity).or_default().push(index);
            }
        }
    }
    let mut found = Vec::new();
    if named.is_empty() {
        return Ok(found);
    }
    for path in changed {
        let mut matches: Vec<usize> = Vec::new();
        if let Some(id) = identity(root, path, &mut cache)? {
            matches.extend(named.get(&id).into_iter().flatten());
            matches.extend(holding.get(&id).into_iter().flatten());
        }
        for prefix in prefixes(path) {
            if let Some(id) = identity(root, prefix, &mut cache)? {
                matches.extend(named.get(&id).into_iter().flatten());
            }
        }
        found.extend(matches.into_iter().map(|entry| (entry, *path)));
    }
    Ok(found)
}

/// The proper ancestors of a relative path, shortest first.
fn prefixes(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'/')
        .map(move |(index, _)| &path[..index])
        .filter(|prefix| !prefix.is_empty())
}

fn identity(
    root: &Path,
    path: &[u8],
    cache: &mut HashMap<Vec<u8>, Option<Identity>>,
) -> Result<Option<Identity>> {
    if let Some(known) = cache.get(path) {
        return Ok(*known);
    }
    let found = match read_identity(&join(root, path)?) {
        Ok(identity) => Some(identity),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            None
        }
        Err(error) => return Err(error.into()),
    };
    cache.insert(path.to_vec(), found);
    Ok(found)
}

fn join(root: &Path, path: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(root.join(std::ffi::OsStr::from_bytes(path)))
    }
    #[cfg(windows)]
    {
        Ok(root.join(std::str::from_utf8(path)?))
    }
}

#[cfg(unix)]
fn read_identity(path: &Path) -> io::Result<Identity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn read_identity(path: &Path) -> io::Result<Identity> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileInformationByHandle,
    };
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // The handle and output storage are live through this call.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Whether `root` folds non-ASCII case, as APFS and NTFS do by default.
    fn folds_unicode_case(root: &Path) -> Result<bool> {
        fs::write(root.join("probe-\u{c9}"), "")?;
        let folded = root.join("probe-\u{e9}").exists();
        fs::remove_file(root.join("probe-\u{c9}"))?;
        Ok(folded)
    }

    #[test]
    fn exact_ancestor_and_descendant_collisions_are_found() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        fs::create_dir_all(root.join("build/out"))?;
        fs::write(root.join("build/out/app"), "ignored build output\n")?;
        fs::create_dir_all(root.join("src"))?;
        fs::write(root.join("src/.env"), "SECRET=1\n")?;
        fs::write(root.join("src/main.rs"), "tracked\n")?;
        let ignored: &[&[u8]] = &[b"build/", b"src/.env"];
        let find = |changed: &'static [u8]| -> Result<Option<&[u8]>> {
            Ok(ignored_collisions(root, ignored, &[changed])?
                .first()
                .copied())
        };
        assert_eq!(find(b"build/out/app")?, Some(&b"build/out/app"[..]));
        assert_eq!(find(b"build")?, Some(&b"build"[..]));
        assert_eq!(find(b"src/.env")?, Some(&b"src/.env"[..]));
        assert_eq!(find(b"src")?, Some(&b"src"[..]));
        assert_eq!(find(b"src/main.rs")?, None);
        assert_eq!(find(b"new/file")?, None);
        let all: &[&[u8]] = &[b"src", b"src/main.rs", b"build/out/app"];
        assert_eq!(
            ignored_collisions(root, ignored, all)?,
            [&b"src"[..], b"build/out/app"]
        );
        Ok(())
    }

    #[test]
    fn edits_overlap_a_change_at_inside_or_around_its_path() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        fs::create_dir_all(root.join("dir"))?;
        fs::write(root.join("dir/mine"), "untracked\n")?;
        fs::write(root.join("edited.txt"), "edited\n")?;
        fs::write(root.join("beside.txt"), "edited\n")?;
        let changed: &[&[u8]] = &[b"edited.txt", b"dir", b"gone.txt", b"tree/leaf"];
        let edited: &[&[u8]] = &[
            b"beside.txt",
            b"dir/mine",
            b"edited.txt",
            // Deleted files exist only as Git paths.
            b"gone.txt",
            b"tree",
            b"treehouse",
            // An untracked nested repository, as Git names it.
            b"nested/",
        ];
        let changed = [changed, &[b"nested/file"]].concat();
        assert_eq!(
            overlapping(root, edited, &changed)?,
            [
                &b"dir/mine"[..],
                b"edited.txt",
                b"gone.txt",
                b"tree",
                b"nested/"
            ]
        );
        assert!(overlapping(root, &[], &changed)?.is_empty());
        Ok(())
    }

    #[test]
    fn edits_overlap_a_spelling_the_filesystem_folds_onto_them() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        // Decomposed (NFD) uppercase, edited by the person.
        let edited = "CAFE\u{301}.txt";
        fs::write(root.join(edited), "mine\n")?;
        // Precomposed (NFC) lowercase, added by the merge.
        let incoming = "caf\u{e9}.txt";
        let found = overlapping(root, &[edited.as_bytes()], &[incoming.as_bytes()])?;
        let same = root.join(incoming).exists();
        assert_eq!(found.is_empty(), !same);
        if folds_unicode_case(root)? && cfg!(target_os = "macos") {
            assert_eq!(found, [edited.as_bytes()]);
        }
        Ok(())
    }

    #[test]
    fn folded_unicode_spellings_match_where_the_filesystem_folds_them() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        // An uppercase E with acute accent, written decomposed (NFD).
        fs::create_dir(root.join("CAFE\u{301}"))?;
        fs::write(root.join("CAFE\u{301}/secret"), "keep\n")?;
        let ignored: &[&[u8]] = &["CAFE\u{301}/".as_bytes()];
        // Lowercase and precomposed (NFC).
        let incoming = "caf\u{e9}/secret".as_bytes();
        let found = ignored_collisions(root, ignored, &[incoming])?;
        let same = root.join("caf\u{e9}").exists();
        assert_eq!(!found.is_empty(), same);
        if folds_unicode_case(root)? && cfg!(target_os = "macos") {
            assert_eq!(found, [incoming]);
        }
        Ok(())
    }
}
