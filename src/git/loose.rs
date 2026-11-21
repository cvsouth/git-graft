//! Blob sizes read from loose object headers.
//!
//! Git's own size and type queries inflate the same header, one object at a
//! time in one process. Reading it here lets several workers share the work.
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{FileExt, OpenOptionsExt},
    path::Path,
};

use flate2::{Decompress, FlushDecompress};

/// Each of `blobs`' size, read from its loose object file in `objects`.
///
/// A blob gets no size if it is not loose there, its header does not name a
/// blob, or `accept` rejects the file once its header has been read.
pub(crate) fn blob_sizes(
    objects: &Path,
    blobs: &[&str],
    accept: impl Fn(&str, &File) -> bool + Sync,
) -> Vec<Option<u64>> {
    const WORKERS: usize = 4;
    let chunk = blobs.len().div_ceil(WORKERS).max(1);
    std::thread::scope(|scope| {
        let workers: Vec<_> = blobs
            .chunks(chunk)
            .map(|batch| {
                let accept = &accept;
                scope.spawn(move || {
                    let mut inflate = Decompress::new(true);
                    batch
                        .iter()
                        .map(|blob| {
                            let (prefix, name) = blob.split_at_checked(2)?;
                            let file = OpenOptions::new()
                                .read(true)
                                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                                .open(objects.join(prefix).join(name))
                                .ok()?;
                            blob_size(&file, &mut inflate).filter(|_| accept(blob, &file))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(crate::workspace::joined)
            .collect()
    })
}

/// The size in a loose object's header if it names a blob: `blob`, a space,
/// the decimal size and a NUL.
fn blob_size(file: &File, inflate: &mut Decompress) -> Option<u64> {
    // The header ends within the first bytes of the zlib stream.
    let mut input = [0; 512];
    let mut header = [0; 32];
    let read = file.read_at(&mut input, 0).ok()?;
    inflate.reset(true);
    inflate
        .decompress(&input[..read], &mut header, FlushDecompress::None)
        .ok()?;
    let header = &header[..usize::try_from(inflate.total_out()).ok()?];
    let end = header.iter().position(|&byte| byte == 0)?;
    let size = header[..end].strip_prefix(b"blob ")?;
    if size.is_empty() || !size.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(size).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn sizes_come_only_from_loose_blob_headers() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git(repo, &["init", "--quiet", "--template="]);
        let empty = repo.join("empty");
        std::fs::write(&empty, b"").unwrap();
        let text = repo.join("text");
        std::fs::write(&text, "line\n".repeat(1000)).unwrap();
        let empty = git(repo, &["hash-object", "-w", empty.to_str().unwrap()]);
        let text = git(repo, &["hash-object", "-w", text.to_str().unwrap()]);
        let tree = git(repo, &["write-tree"]);
        let missing = "0".repeat(40);
        let objects = repo.join(".git/objects");
        let ids = [
            empty.as_str(),
            text.as_str(),
            tree.as_str(),
            missing.as_str(),
            "x",
        ];
        assert_eq!(
            blob_sizes(&objects, &ids, |_, _| true),
            [Some(0), Some(5000), None, None, None]
        );
        // The caller's check runs after the header and can still refuse.
        assert_eq!(
            blob_sizes(&objects, &ids, |id, _| id != text),
            [Some(0), None, None, None, None]
        );
        // Blobs spread across workers keep their order.
        let many: Vec<&str> = std::iter::repeat_n([empty.as_str(), missing.as_str()], 9)
            .flatten()
            .collect();
        let expected: Vec<_> = std::iter::repeat_n([Some(0), None], 9).flatten().collect();
        assert_eq!(blob_sizes(&objects, &many, |_, _| true), expected);
        // Packed blobs are left to Git.
        git(repo, &["add", "--", "empty", "text"]);
        git(repo, &["repack", "-adq"]);
        git(repo, &["prune-packed"]);
        assert_eq!(blob_sizes(&objects, &ids[..2], |_, _| true), [None, None]);
    }
}
