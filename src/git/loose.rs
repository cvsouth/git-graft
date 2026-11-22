//! Blob sizes read from loose object headers.
//!
//! Git's own size and type queries inflate the same header, one object at a
//! time in one process. Reading it here lets several workers share the work.
use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    os::unix::fs::{FileExt, OpenOptionsExt},
    path::Path,
};

use flate2::{Decompress, FlushDecompress};

/// The size of each of `blobs` that is a loose blob in `objects`.
///
/// A blob gets no size if it is not loose there, its header does not name a
/// blob, or `accept` rejects the file once its header has been read.
pub(crate) fn blob_sizes<'a>(
    objects: &Path,
    blobs: impl IntoIterator<Item = &'a str>,
    accept: impl Fn(&str, &File) -> bool + Sync,
) -> HashMap<String, u64> {
    const WORKERS: usize = 4;
    let blobs: Vec<&str> = blobs
        .into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
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
                        .filter_map(|&blob| {
                            let (prefix, name) = blob.split_at_checked(2)?;
                            let file = OpenOptions::new()
                                .read(true)
                                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                                .open(objects.join(prefix).join(name))
                                .ok()?;
                            let size =
                                blob_size(&file, &mut inflate).filter(|_| accept(blob, &file))?;
                            Some((blob.to_owned(), size))
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

/// The size in a loose object's header if it names a blob, by the rules of
/// Git's `unpack_loose_header` and `parse_loose_header`: the header, with its
/// NUL, fits in Git's 32-byte buffer, and is `blob`, one space and the size in
/// canonical decimal, without a sign or leading zeros, that fits in 64 bits.
fn blob_size(file: &File, inflate: &mut Decompress) -> Option<u64> {
    // The header ends within the first bytes of the zlib stream.
    let mut input = [0; 512];
    let mut header = [0; MAX_HEADER];
    let read = file.read_at(&mut input, 0).ok()?;
    inflate.reset(true);
    inflate
        .decompress(&input[..read], &mut header, FlushDecompress::None)
        .ok()?;
    let header = &header[..usize::try_from(inflate.total_out()).ok()?];
    let end = header.iter().position(|&byte| byte == 0)?;
    let size = header[..end].strip_prefix(b"blob ")?;
    let canonical = match size {
        [b'0'] => true,
        [b'1'..=b'9', rest @ ..] => rest.iter().all(u8::is_ascii_digit),
        _ => false,
    };
    // Git dies on a size that overflows; parsing refuses it here.
    canonical.then(|| std::str::from_utf8(size).ok()?.parse().ok())?
}

/// Git's `MAX_HEADER_LEN`: a loose header without a NUL in this many bytes
/// is too long for Git to read.
const MAX_HEADER: usize = 32;

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
            blob_sizes(&objects, ids, |_, _| true),
            HashMap::from([(empty.clone(), 0), (text.clone(), 5000)])
        );
        // The caller's check runs after the header and can still refuse.
        assert_eq!(
            blob_sizes(&objects, ids, |id, _| id != text),
            HashMap::from([(empty.clone(), 0)])
        );
        // Repeated blobs spread across workers are read once each.
        let many =
            std::iter::repeat_n([empty.as_str(), text.as_str(), missing.as_str()], 9).flatten();
        assert_eq!(blob_sizes(&objects, many, |_, _| true).len(), 2);
        // Packed blobs are left to Git.
        git(repo, &["add", "--", "empty", "text"]);
        git(repo, &["repack", "-adq"]);
        git(repo, &["prune-packed"]);
        assert!(blob_sizes(&objects, ids, |_, _| true).is_empty());
    }

    /// Store `body` under `id` as a loose object exactly as given.
    fn write_loose(objects: &Path, id: &str, body: &[u8]) {
        use std::io::Write;
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(body).unwrap();
        let directory = objects.join(&id[..2]);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(&id[2..]), encoder.finish().unwrap()).unwrap();
    }

    #[test]
    fn headers_follow_gits_loose_header_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git(repo, &["init", "--quiet", "--template="]);
        let objects = repo.join(".git/objects");
        let long = format!("blob {}\0", "1".repeat(30));
        let cases: [(&[u8], Option<u64>); 14] = [
            (b"blob 5\0hello", Some(5)),
            (b"blob 0\0", Some(0)),
            (b"blob 05\0hello", None),
            (b"blob 00\0", None),
            (b"blob +5\0hello", None),
            (b"blob -5\0hello", None),
            (b"blob  5\0hello", None),
            (b"blob 5 \0hello", None),
            (b"blob \0", None),
            (b"blob\0", None),
            (b"Blob 5\0hello", None),
            (b"tree 5\0hello", None),
            (b"blob 99999999999999999999\0", None),
            (long.as_bytes(), None),
        ];
        for (index, (body, expected)) in cases.into_iter().enumerate() {
            // Git refuses the all-zero null ID before reading anything.
            let id = format!("{:040x}", index + 1);
            write_loose(&objects, &id, body);
            assert_eq!(
                blob_sizes(&objects, [id.as_str()], |_, _| true)
                    .get(&id)
                    .copied(),
                expected,
                "{}",
                String::from_utf8_lossy(body)
            );
            // Git reads exactly the blob headers accepted here.
            let read = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["cat-file", "-t", &id])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            let blob = read.status.success() && read.stdout == b"blob\n";
            assert_eq!(
                blob,
                expected.is_some(),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }
}
