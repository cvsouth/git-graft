//! Bind package cache roots to the registry and quarantine verified cache entries.

use crate::{
    recovery::rename_exclusive,
    registry::{self, Project, check_unmounted_directory, exists, is_reparse_point},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub(crate) const PACKAGES: &[&str] = &["npm", "pnpm", "bun", "cargo", "uv", "sccache"];
const OWNER: &str = ".git-graft-cache-id";
pub(crate) const QUARANTINE_PREFIX: &str = ".purge-";
const OWNER_LIMIT: u64 = 1024;

/// The identity of a directory. Within one command it compares in full. A
/// copy written to disk is checked later with [`DirectoryIdentity::names`].
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectoryIdentity {
    device: u64,
    file: u64,
    /// The volume's UUID on macOS, where the filesystem reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    volume_uuid: Option<VolumeId>,
    /// The birth time earlier versions recorded (see [`Legacy`]).
    #[serde(default, rename = "created", skip_serializing)]
    legacy: Legacy,
}

impl DirectoryIdentity {
    /// Whether this identity, read from disk, names the directory whose
    /// identity is `current`. See [`persisted_match`].
    pub(crate) fn names(&self, current: &Self) -> bool {
        persisted_match(
            (self.device, self.file, self.volume_uuid),
            (current.device, current.file, current.volume_uuid),
        )
    }

    /// The device number when this identity was read. On Unix it can change
    /// across a reboot or remount.
    #[cfg(unix)]
    pub(crate) fn device(&self) -> u64 {
        self.device
    }
}

/// Whether a file identity written to disk, `(volume, file, volume UUID)`,
/// names the file whose identity was just read as `current`.
///
/// A Unix device number is not stable: macOS APFS renumbers volumes across a
/// reboot, and on Linux device-mapper, NVMe enumeration order, NFS and btrfs
/// can renumber one across a reboot or remount. So on Unix the device is
/// recorded but never compared. Birth times are not compared either: setting
/// a file's modification time before its birth time moves the birth time on
/// macOS, as `touch -t`, `rsync -a`, `cp -a` and `tar` do.
///
/// A persisted identity names a file by inode, and on macOS also by the UUID
/// of its volume, which survives reboots: a copy or replacement on the same
/// volume has a new inode, and one on another volume has another UUID. Where
/// either side has no volume UUID, the inode is compared alone. That covers
/// Linux and other Unix systems, which have no stable and cheap filesystem
/// UUID, identities written before Graft recorded one, and macOS filesystems
/// that report none, such as some network and FUSE filesystems: refusing
/// those would stop every command, while the fixed path, the project ID and
/// the ownership markers inside each directory still have to match. Windows
/// volume serial numbers survive reboots and are compared.
fn persisted_match(
    stored: (u64, u64, Option<VolumeId>),
    current: (u64, u64, Option<VolumeId>),
) -> bool {
    let device = cfg!(unix) || stored.0 == current.0;
    let volume = match (stored.2, current.2) {
        (Some(stored), Some(current)) => stored == current,
        _ => true,
    };
    device && stored.1 == current.1 && volume
}

/// The UUID of a volume, as macOS reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VolumeId(Uuid);

impl Serialize for VolumeId {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for VolumeId {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Uuid::parse_str(&text)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// A field earlier versions wrote and Graft now reads and ignores: the birth
/// time, which does not identify a file (see [`persisted_match`]). It records
/// only whether the field was there, so that the identity is rewritten.
#[derive(Debug, Clone, Copy, Default)]
struct Legacy(bool);

impl PartialEq for Legacy {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Legacy {}

impl<'de> Deserialize<'de> for Legacy {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(deserializer)?;
        Ok(Self(true))
    }
}

/// The UUID of the volume holding `file`, or `None` where the filesystem
/// reports none, such as some network and FUSE filesystems, or the call
/// fails. Identities then compare inodes alone (see [`persisted_match`]).
#[cfg(target_os = "macos")]
fn volume_uuid(file: &fs::File) -> Option<VolumeId> {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct Reply {
        length: u32,
        returned: libc::attribute_set_t,
        uuid: [u8; 16],
    }
    let mut request = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut reply = std::mem::MaybeUninit::<Reply>::zeroed();
    // The descriptor, request and reply buffer are live through this call,
    // which writes at most the reply's size.
    let status = unsafe {
        libc::fgetattrlist(
            file.as_raw_fd(),
            (&raw mut request).cast(),
            reply.as_mut_ptr().cast(),
            std::mem::size_of::<Reply>(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    // Zeroed and then written by the call: every bit pattern is valid.
    let reply = unsafe { reply.assume_init() };
    (reply.length as usize >= std::mem::size_of::<Reply>()
        && reply.returned.volattr & libc::ATTR_VOL_UUID != 0
        && reply.uuid != [0; 16])
        .then(|| VolumeId(Uuid::from_bytes(reply.uuid)))
}

/// The volume UUID of the file or directory at `path` whose metadata was
/// just read as `metadata`, read on an open descriptor of that same file. A
/// file that cannot be opened has none, as on a filesystem without UUIDs.
#[cfg(target_os = "macos")]
fn volume_uuid_at(path: &Path, metadata: &fs::Metadata) -> Result<Option<VolumeId>> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let Ok(file) = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    else {
        return Ok(None);
    };
    let opened = file.metadata()?;
    ensure!(
        (opened.dev(), opened.ino()) == (metadata.dev(), metadata.ino()),
        "{} changed while its identity was read",
        path.display()
    );
    Ok(volume_uuid(&file))
}

/// The identity of a file or directory as Graft persists it in its own
/// records and markers, which must keep naming it after a reboot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileIdentity {
    /// On Unix, the device number; recorded but not compared on disk (see
    /// [`persisted_match`]). On Windows, the volume serial number.
    volume: u64,
    file: u64,
    /// The volume's UUID on macOS, where the filesystem reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    volume_uuid: Option<VolumeId>,
    /// The birth time an earlier build recorded (see [`Legacy`]).
    #[serde(default, rename = "created", skip_serializing)]
    legacy: Legacy,
}

impl FileIdentity {
    /// The identity of the file or directory at `path`, whose metadata was
    /// just read as `metadata`.
    #[cfg(unix)]
    pub(crate) fn at(path: &Path, metadata: &fs::Metadata) -> Result<Self> {
        #[cfg(target_os = "macos")]
        let volume_uuid = volume_uuid_at(path, metadata)?;
        #[cfg(not(target_os = "macos"))]
        let volume_uuid = {
            let _ = path;
            None
        };
        Ok(Self::unix(metadata, volume_uuid))
    }

    /// The identity of the open `file`, whose metadata is `metadata`.
    #[cfg(unix)]
    pub(crate) fn of(file: &fs::File, metadata: &fs::Metadata) -> Self {
        #[cfg(target_os = "macos")]
        let volume_uuid = volume_uuid(file);
        #[cfg(not(target_os = "macos"))]
        let volume_uuid = {
            let _ = file;
            None
        };
        Self::unix(metadata, volume_uuid)
    }

    #[cfg(unix)]
    fn unix(metadata: &fs::Metadata, volume_uuid: Option<VolumeId>) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            volume: metadata.dev(),
            file: metadata.ino(),
            volume_uuid,
            legacy: Legacy::default(),
        }
    }

    #[cfg(windows)]
    pub(crate) fn windows(volume: u64, file: u64) -> Self {
        Self {
            volume,
            file,
            volume_uuid: None,
            legacy: Legacy::default(),
        }
    }

    /// Whether this identity, read from disk, names the file whose identity
    /// is `current`. See [`persisted_match`].
    pub(crate) fn names(&self, current: &Self) -> bool {
        persisted_match(
            (self.volume, self.file, self.volume_uuid),
            (current.volume, current.file, current.volume_uuid),
        )
    }

    /// Whether this identity is in the form Graft writes now. One an earlier
    /// version wrote has a birth time, or, on macOS, no volume UUID.
    pub(crate) fn is_current(&self) -> bool {
        !self.legacy.0 && (self.volume_uuid.is_some() || !cfg!(target_os = "macos"))
    }

    /// Whether this identity names `current` and is in an earlier form that
    /// writing `current` in its place replaces: it drops a birth time, and
    /// adds the volume UUID `current` has and this one lacks.
    pub(crate) fn upgrades_to(&self, current: &Self) -> bool {
        self.names(current)
            && (self.legacy.0 || self.volume_uuid.is_none() && current.volume_uuid.is_some())
    }

    /// Whether both identities, read in this command, are on one volume.
    pub(crate) fn same_volume(&self, other: &Self) -> bool {
        self.volume == other.volume
    }
}

/// The volume serial number and file index of an ordinary Windows directory,
/// opened without following a reparse point. `what` names it in errors.
#[cfg(windows)]
pub(crate) fn windows_directory_id(path: &Path, what: &str) -> Result<(u64, u64)> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, GetFileInformationByHandle,
    };
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // The directory handle and output storage are live through this call.
    if unsafe { GetFileInformationByHandle(directory.as_raw_handle(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    ensure!(
        info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
            == FILE_ATTRIBUTE_DIRECTORY,
        "expected an ordinary {what} at {}",
        path.display()
    );
    Ok((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

pub(crate) fn directory_identity(path: &Path) -> Result<DirectoryIdentity> {
    let metadata = check_unmounted_directory(path, false)?;
    #[cfg(unix)]
    {
        let identity = FileIdentity::at(path, &metadata)?;
        Ok(DirectoryIdentity {
            device: identity.volume,
            file: identity.file,
            volume_uuid: identity.volume_uuid,
            legacy: Legacy::default(),
        })
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        let (device, file) = windows_directory_id(path, "cache directory")?;
        Ok(DirectoryIdentity {
            device,
            file,
            volume_uuid: None,
            legacy: Legacy::default(),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        anyhow::bail!("package cache directory identity is unavailable");
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackageBinding {
    project: String,
    package: String,
    nonce: String,
    directory: DirectoryIdentity,
}

fn private_directory(path: &Path) -> Result<()> {
    let metadata = check_unmounted_directory(path, false)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "cache directory is not private: {}",
            path.display()
        );
    }
    #[cfg(not(unix))]
    let _ = metadata;
    Ok(())
}

/// Create a temporary directory that only the current user can enter.
pub(crate) fn private_stage(parent: &Path, prefix: &str) -> Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    Ok(builder.tempdir_in(parent)?)
}

fn owner_record(project: &Project, package: &str) -> PathBuf {
    project
        .state
        .join(format!("package-cache-{package}-v1.json"))
}

/// Open an entry for reading without following a final symlink or reparse point,
/// and without blocking on a FIFO. Callers must check the opened file's type.
pub(crate) fn open_no_follow(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

pub(crate) fn read_private_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = open_no_follow(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && !is_reparse_point(&metadata) && metadata.len() <= limit,
        "invalid private record at {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.nlink() == 1
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "Graft state record is not private: {}",
            path.display()
        );
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "private record is too large");
    Ok(bytes)
}

fn read_binding(project: &Project, package: &str) -> Result<PackageBinding> {
    ensure!(
        PACKAGES.contains(&package)
            || package
                .strip_prefix("registered-")
                .is_some_and(|id| id.len() == 64
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())),
        "unrecognized package cache root"
    );
    let binding: PackageBinding = serde_json::from_slice(&read_private_file(
        &owner_record(project, package),
        OWNER_LIMIT,
    )?)?;
    ensure!(
        binding.project == project.id()
            && binding.package == package
            && Uuid::parse_str(&binding.nonce).is_ok_and(
                |nonce| nonce.get_version_num() == 4 && nonce.to_string() == binding.nonce
            ),
        "invalid package cache registry binding"
    );
    Ok(binding)
}

pub(crate) fn verify_package(
    project: &Project,
    package: &str,
    path: &Path,
) -> Result<PackageBinding> {
    (|| {
        private_directory(path)?;
        let expected = read_binding(project, package)?;
        ensure!(
            read_private_file(&path.join(OWNER), OWNER_LIMIT)? == expected.nonce.as_bytes()
                && expected.directory.names(&directory_identity(path)?),
            "package cache binding does not match this directory"
        );
        Ok(expected)
    })()
    .with_context(|| {
        format!(
            "package cache ownership is unverified at {}",
            path.display()
        )
    })
}

/// Pin every package root exported by the runtime. The project lock must be held.
/// Existing directories are accepted only with their original registry binding.
pub(crate) fn prepare_package_roots(project: &Project) -> Result<()> {
    prepare_roots(project, PACKAGES)
}

pub(crate) fn registered_cache_path(project: &Project, manager: &str) -> PathBuf {
    project
        .state
        .join("cache")
        .join(format!("registered-{}", blake3::hash(manager.as_bytes())))
}

pub(crate) fn prepare_registered_cache(project: &Project, manager: &str) -> Result<PathBuf> {
    let path = registered_cache_path(project, manager);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid cache identifier")?;
    prepare_roots(project, &[name])?;
    Ok(path)
}

fn prepare_roots(project: &Project, packages: &[&str]) -> Result<()> {
    project.require_private_state()?;
    let cache = project.state.join("cache");
    project.check_cache_owner(&cache)?;
    private_directory(&cache)?;
    let parent = directory_identity(&cache)?;
    let mut absent = Vec::new();
    for &package in packages {
        let path = cache.join(package);
        if exists(&path)? {
            verify_package(project, package, &path)?;
        } else {
            let record = owner_record(project, package);
            if exists(&record)? {
                // Agreement is required even for a retired binding. Inconclusive
                // registry state is left untouched.
                read_binding(project, package)?;
            }
            absent.push((package, path, record));
        }
    }
    for (package, path, record) in absent {
        let stage = private_stage(&cache, ".package-")?.keep();
        let binding = PackageBinding {
            project: project.id().to_owned(),
            package: package.to_owned(),
            nonce: Uuid::new_v4().to_string(),
            directory: directory_identity(&stage)?,
        };
        registry::atomic(&stage.join(OWNER), binding.nonce.as_bytes())?;
        ensure!(
            directory_identity(&cache)? == parent && !exists(&path)?,
            "package cache path changed during preparation"
        );
        project.check_cache_owner(&cache)?;
        // Commit the binding first. An interrupted publication leaves a vacant
        // slot that a later invocation can provision, without adopting any tree.
        registry::atomic(&record, &serde_json::to_vec(&binding)?)?;
        rename_exclusive(&stage, &path)
            .with_context(|| format!("publish package cache {}", path.display()))?;
        verify_package(project, package, &path)?;
    }
    Ok(())
}

pub(crate) enum PurgeFailure {
    Unmoved(anyhow::Error),
    Retained { path: PathBuf, error: anyhow::Error },
}

/// Move the exact inspected directory into private quarantine before deletion.
/// Keep anything whose identity or contents cannot be re-established there.
pub(crate) fn purge_directory(
    path: &Path,
    identity: &DirectoryIdentity,
    check_container: impl Fn() -> Result<()>,
    verify: impl FnOnce(&Path) -> Result<()>,
) -> std::result::Result<(), PurgeFailure> {
    let stage = (|| {
        let parent = path.parent().context("cache has no parent")?;
        check_container()?;
        let stage = private_stage(parent, QUARANTINE_PREFIX)?.keep();
        let stage_id = directory_identity(&stage)?;
        let name = path.file_name().context("cache has no name")?;
        let staged_path = stage.join(name);
        let check_stage = || -> Result<()> {
            check_container()?;
            private_directory(&stage)?;
            ensure!(
                directory_identity(&stage)? == stage_id,
                "cache quarantine directory changed"
            );
            Ok(())
        };
        let move_result = (|| {
            check_stage()?;
            ensure!(
                directory_identity(path)? == *identity,
                "cache directory changed before quarantine"
            );
            rename_exclusive(path, &staged_path)?;
            Ok(())
        })();
        if let Err(error) = move_result {
            if check_stage().is_ok() {
                let _ = fs::remove_dir(&stage);
            }
            return Err(error);
        }
        Ok((stage, stage_id, staged_path))
    })()
    .map_err(PurgeFailure::Unmoved)?;
    let (directory, quarantine_id, moved) = stage;
    let check = || -> Result<()> {
        check_container()?;
        private_directory(&directory)?;
        ensure!(
            directory_identity(&directory)? == quarantine_id
                && directory_identity(&moved)? == *identity,
            "cache directory changed during quarantine"
        );
        ensure!(
            !exists(path)?,
            "cache path was repopulated during quarantine"
        );
        Ok(())
    };
    let removal = (|| {
        check()?;
        verify(&moved)?;
        check()?;
        fs::remove_dir_all(&moved).context("remove the verified quarantined cache")?;
        Ok(())
    })();
    match removal {
        Ok(()) => {
            // A leftover entry in the quarantine container is never removed here.
            if check_container().is_ok()
                && directory_identity(&directory).is_ok_and(|id| id == quarantine_id)
            {
                let _ = fs::remove_dir(directory);
            }
            Ok(())
        }
        Err(error) => Err(PurgeFailure::Retained { path: moved, error }),
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos", windows)))]
mod tests {
    use super::*;
    use crate::{engine::gc::purge_caches, git};

    #[test]
    fn persisted_identities_ignore_unix_device_numbers_only() {
        let volume = Some(VolumeId(Uuid::new_v4()));
        let stored = (16_777_231, 977_754_921, volume);
        // A reboot renumbered the device; nothing else changed.
        let renumbered = (16_777_232, 977_754_921, volume);
        assert_eq!(persisted_match(stored, renumbered), cfg!(unix));
        assert!(persisted_match(stored, stored));
        // Another directory has another inode or volume.
        assert!(!persisted_match(stored, (16_777_231, 977_754_922, volume)));
        let elsewhere = Some(VolumeId(Uuid::new_v4()));
        assert!(!persisted_match(
            stored,
            (16_777_231, 977_754_921, elsewhere)
        ));
        // Without a volume UUID on either side, as on Linux, in an identity
        // written before Graft recorded one, or on a filesystem that reports
        // none, the inode decides.
        for (stored, current) in [(None, volume), (volume, None), (None, None)] {
            assert_eq!(
                persisted_match((16_777_231, 977_754_921, stored), (1, 977_754_921, current)),
                cfg!(unix)
            );
            assert!(!persisted_match(
                (1, 977_754_921, stored),
                (1, 977_754_922, current)
            ));
        }
    }

    /// Identities an earlier build wrote, with a birth time or without a
    /// volume UUID, name the same file and are rewritten in the current form.
    #[cfg(unix)]
    #[test]
    fn file_identities_upgrade_only_for_the_same_file() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let current = FileIdentity::at(tmp.path(), &fs::metadata(tmp.path())?)?;
        assert!(current.is_current() && !current.upgrades_to(&current));
        assert_eq!(
            current.volume_uuid.is_some(),
            cfg!(target_os = "macos"),
            "a temporary directory's volume UUID"
        );
        let born = serde_json::json!({"secs_since_epoch": 1_795_209_831, "nanos_since_epoch": 7});
        let other_path = tempfile::tempdir()?;
        let other = FileIdentity::at(other_path.path(), &fs::metadata(other_path.path())?)?;
        for created in [None, Some(born)] {
            // Graft 0.1.0 wrote no birth time, and the build before this one
            // wrote one; neither wrote a volume UUID.
            let mut value = serde_json::json!({"volume": current.volume + 1, "file": current.file});
            if let Some(created) = created {
                value["created"] = created;
            }
            let legacy: FileIdentity = serde_json::from_value(value.clone())?;
            assert!(legacy.names(&current));
            assert_eq!(
                legacy.upgrades_to(&current),
                value.get("created").is_some() || cfg!(target_os = "macos"),
                "{value}"
            );
            assert_eq!(
                legacy.is_current(),
                !legacy.upgrades_to(&current),
                "{value}"
            );
            assert!(!legacy.names(&other) && !legacy.upgrades_to(&other));
        }
        // The current form records no birth time.
        assert!(serde_json::to_value(&current)?.get("created").is_none());
        Ok(())
    }

    #[test]
    fn package_bindings_survive_a_renumbered_device() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        git::bytes(tmp.path(), &["init", "-q", "--template="])?;
        let (project, _lock) = Project::open(tmp.path())?;
        prepare_package_roots(&project)?;
        let npm = project.state.join("cache/npm");
        let record = owner_record(&project, "npm");
        let original: serde_json::Value = serde_json::from_slice(&fs::read(&record)?)?;
        let rewrite = |change: &dyn Fn(&mut serde_json::Value)| -> Result<()> {
            let mut binding = original.clone();
            change(&mut binding["directory"]);
            registry::atomic(&record, &serde_json::to_vec(&binding)?)?;
            Ok(())
        };
        let renumber = |directory: &mut serde_json::Value| {
            directory["device"] = (directory["device"].as_u64().unwrap() + 1).into();
        };
        rewrite(&renumber)?;
        assert_eq!(verify_package(&project, "npm", &npm).is_ok(), cfg!(unix));
        #[cfg(unix)]
        prepare_package_roots(&project)?;
        rewrite(&|directory| {
            renumber(directory);
            directory["file"] = (directory["file"].as_u64().unwrap() + 1).into();
        })?;
        assert!(verify_package(&project, "npm", &npm).is_err());
        assert!(prepare_package_roots(&project).is_err());
        Ok(())
    }

    /// A package cache whose modification time was set into the past, which
    /// moves its birth time on macOS, is the same directory. One recorded on
    /// another volume is not.
    #[cfg(unix)]
    #[test]
    fn package_bindings_name_the_volume_not_the_birth_time() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        git::bytes(tmp.path(), &["init", "-q", "--template="])?;
        let (project, _lock) = Project::open(tmp.path())?;
        prepare_package_roots(&project)?;
        let npm = project.state.join("cache/npm");
        let past =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_577_836_800);
        fs::File::open(&npm)?.set_modified(past)?;
        verify_package(&project, "npm", &npm)?;
        prepare_package_roots(&project)?;
        if cfg!(target_os = "macos") {
            let record = owner_record(&project, "npm");
            let mut binding: serde_json::Value = serde_json::from_slice(&fs::read(&record)?)?;
            binding["directory"]["volume_uuid"] = Uuid::new_v4().to_string().into();
            registry::atomic(&record, &serde_json::to_vec(&binding)?)?;
            assert!(verify_package(&project, "npm", &npm).is_err());
        }
        Ok(())
    }

    #[test]
    fn existing_root_cannot_be_adopted_even_with_a_copied_marker() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        git::bytes(tmp.path(), &["init", "-q", "--template="])?;
        let (project, _lock) = Project::open(tmp.path())?;
        let cache = project.state.join("cache");
        let npm = cache.join("npm");
        prepare_package_roots(&project)?;
        let saved = cache.join("displaced-npm");
        fs::write(npm.join("old"), "old install")?;
        rename_exclusive(&npm, &saved)?;
        registry::dir(&npm)?;
        fs::copy(saved.join(OWNER), npm.join(OWNER))?;
        fs::write(npm.join("private"), "user data")?;
        assert!(prepare_package_roots(&project).is_err());

        let result = purge_caches(&project, true)?;
        assert!(result["blocked"].as_array().unwrap().iter().any(|entry| {
            entry["path"] == serde_json::json!(npm)
                && entry["reason"]["message"].as_str().is_some_and(|message| {
                    message.contains("package cache ownership is unverified")
                })
        }));
        assert!(
            !result["removed"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(npm))
        );
        assert_eq!(fs::read_to_string(npm.join("private"))?, "user data");
        assert_eq!(fs::read_to_string(saved.join("old"))?, "old install");
        verify_package(&project, "npm", &saved)?;
        Ok(())
    }

    #[test]
    fn purge_reports_a_previously_retained_quarantine_without_collecting_it() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        git::bytes(tmp.path(), &["init", "-q", "--template="])?;
        let (project, _lock) = Project::open(tmp.path())?;
        prepare_package_roots(&project)?;
        let path = project.state.join("cache/npm");
        fs::write(path.join("entry"), "captured contents")?;
        let identity = directory_identity(&path)?;
        let outcome = purge_directory(
            &path,
            &identity,
            || Ok(()),
            |moved| {
                fs::write(moved.join("entry"), "edited during purge")?;
                registry::dir(&path)?;
                fs::write(path.join("private"), "replacement")?;
                anyhow::bail!("payload changed")
            },
        );
        let Err(PurgeFailure::Retained { path: moved, .. }) = outcome else {
            panic!("changed cache must be retained after quarantine");
        };
        assert_eq!(
            fs::read_to_string(moved.join("entry"))?,
            "edited during purge"
        );
        assert_eq!(fs::read_to_string(path.join("private"))?, "replacement");
        let result = purge_caches(&project, true)?;
        let stage = moved.parent().unwrap();
        assert_eq!(result["retained"][0]["path"], serde_json::json!(stage));
        assert!(moved.exists());
        assert!(path.exists());
        assert!(
            !result["removed"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(path))
        );
        Ok(())
    }

    #[test]
    fn purge_preserves_replacements_before_and_after_quarantine() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("managed");
        let stored = tmp.path().join("stored");
        registry::dir(&path)?;
        fs::write(path.join("entry"), "verified")?;
        let identity = directory_identity(&path)?;
        rename_exclusive(&path, &stored)?;
        registry::dir(&path)?;
        fs::write(path.join("entry"), "different directory")?;
        assert!(matches!(
            purge_directory(&path, &identity, || Ok(()), |_| panic!("wrong identity")),
            Err(PurgeFailure::Unmoved(_))
        ));
        let result = purge_directory(
            &stored,
            &identity,
            || Ok(()),
            |moved| {
                ensure!(fs::read_to_string(moved.join("entry"))? == "verified");
                registry::dir(&stored)?;
                fs::write(stored.join("entry"), "new arrival")?;
                Ok(())
            },
        );
        let Err(PurgeFailure::Retained { path: moved, .. }) = result else {
            panic!("repopulated cache path must retain the quarantined directory");
        };
        assert_eq!(fs::read_to_string(moved.join("entry"))?, "verified");
        assert_eq!(fs::read_to_string(stored.join("entry"))?, "new arrival");
        assert_eq!(
            fs::read_to_string(path.join("entry"))?,
            "different directory"
        );
        Ok(())
    }

    #[test]
    fn quarantine_never_replaces_an_entry_that_appears_before_rename() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("owned");
        registry::dir(&path)?;
        fs::write(path.join("entry"), "original")?;
        let identity = directory_identity(&path)?;
        let check_container = || -> Result<()> {
            for entry in fs::read_dir(tmp.path())? {
                let entry = entry?;
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(QUARANTINE_PREFIX)
                {
                    let occupied = entry.path().join("owned");
                    if !exists(&occupied)? {
                        registry::dir(&occupied)?;
                    }
                }
            }
            Ok(())
        };
        assert!(matches!(
            purge_directory(&path, &identity, check_container, |_| panic!(
                "unexpected move"
            )),
            Err(PurgeFailure::Unmoved(_))
        ));
        assert_eq!(fs::read_to_string(path.join("entry"))?, "original");
        let quarantine = fs::read_dir(tmp.path())?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(QUARANTINE_PREFIX)
            })
            .context("collision was not retained")?;
        assert!(
            fs::read_dir(quarantine.path().join("owned"))?
                .next()
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn successful_purge_can_recreate_fresh_package_bindings() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        git::bytes(tmp.path(), &["init", "-q", "--template="])?;
        let (project, _lock) = Project::open(tmp.path())?;
        prepare_package_roots(&project)?;
        let npm = project.state.join("cache/npm");
        let original = verify_package(&project, "npm", &npm)?;
        assert_eq!(purge_caches(&project, true)?["purged"], true);
        assert!(!npm.exists());
        prepare_package_roots(&project)?;
        assert_ne!(verify_package(&project, "npm", &npm)?, original);
        Ok(())
    }
}
