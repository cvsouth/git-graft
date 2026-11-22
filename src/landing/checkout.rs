//! Native index ownership and the durable transition for a checked-out branch.
//!
//! As with `git merge`, an update goes ahead around the person's uncommitted
//! edits as long as none touches a path it changes, and leaves them exactly
//! as they were. Otherwise it refuses with `destination_has_local_changes`,
//! naming the overlapping paths, before any file is written.
//!
//! An update of the checked-out branch is journaled in three phases.
//! `Prepared` has changed nothing. `Updating` means native `read-tree` may
//! have rewritten some files. `Indexed` means the files and index hold the
//! merged commit and only the ref commit remains. A retry does not trust the
//! phase alone: it rebuilds a private index from the files on disk, then lets
//! Git finish the update or, for another merge's journal, return the files to
//! the previous commit. Each changed path must still hold one of the two
//! recorded versions, so neither direction replaces anyone's edits, and
//! Git's own checks keep untracked and ignored files in place: a file that
//! appears where the update writes, even during the write, is refused.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::{
    api::{Fault, fault},
    cache::{DirectoryIdentity, directory_identity},
    git, recovery, registry,
};

use super::{Plan, git_failure};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum Phase {
    Prepared,
    Updating,
    Indexed,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    plan: Plan,
    token: String,
    phase: Phase,
    directory: DirectoryIdentity,
    /// The last native Git command that locked the private index, such as
    /// the `read-tree` that writes files during `Updating`. A retry changes
    /// nothing while its lock remains and this process may still be running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worker: Option<u32>,
}

fn read_record(directory: &Path) -> Result<Record> {
    let identity = directory_identity(directory)?;
    let mut record: Record =
        serde_json::from_slice(&registry::regular(&directory.join("record.json"))?)?;
    ensure!(
        Uuid::parse_str(&record.token)?.to_string() == record.token
            && record.directory.names(&identity),
        "invalid checkout transition identity at {}",
        directory.display()
    );
    // A reboot can renumber the device. Carry the current identity, which the
    // next phase change records and later reads in this command compare.
    record.directory = identity;
    Ok(record)
}

/// An index entry in the form `update-index --index-info` reads. Mode
/// `000000` with the null object ID means the path is absent on that side.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    mode: String,
    oid: String,
}

impl Entry {
    fn present(&self) -> bool {
        self.mode != "000000"
    }
}

/// One path the update changes, with its entry before and after.
struct Change {
    path: Vec<u8>,
    old: Entry,
    new: Entry,
}

/// Name at most a screenful of paths in a message.
fn listed(paths: &[Vec<u8>]) -> String {
    let mut names: Vec<_> = paths
        .iter()
        .take(20)
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect();
    if paths.len() > names.len() {
        names.push(format!("and {} more", paths.len() - names.len()));
    }
    names.join(", ")
}

/// Stage-zero entries of the index Git reads for `command`, by path.
fn staged_entries(mut command: Command) -> Result<BTreeMap<Vec<u8>, Entry>> {
    let output = command.args(["ls-files", "--stage", "-z"]).output()?;
    if !output.status.success() {
        return Err(git_failure("read checkout index", &output));
    }
    let mut entries = BTreeMap::new();
    for row in output.stdout.split(|byte| *byte == 0) {
        if row.is_empty() {
            continue;
        }
        let tab = row
            .iter()
            .position(|byte| *byte == b'\t')
            .context("malformed Git index record")?;
        let header = std::str::from_utf8(&row[..tab])?;
        let mut fields = header.split(' ');
        let (Some(mode), Some(oid), Some(stage), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            bail!("malformed Git index record");
        };
        ensure!(
            stage == "0",
            "the destination index has unresolved conflicts"
        );
        entries.insert(
            row[tab + 1..].to_vec(),
            Entry {
                mode: mode.into(),
                oid: oid.into(),
            },
        );
    }
    Ok(entries)
}

/// Whether a process that wrote a private lock has certainly exited.
#[cfg(unix)]
fn worker_exited(worker: Option<u32>) -> bool {
    let Some(pid) = worker
        .and_then(|pid| libc::pid_t::try_from(pid).ok())
        .filter(|pid| *pid > 0)
    else {
        return false;
    };
    // Signal 0 only asks whether the process exists. ESRCH proves it does
    // not; a reused ID merely keeps the lock, which is the safe outcome.
    let result = unsafe { libc::kill(pid, 0) };
    result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

pub(super) struct Update {
    directory: PathBuf,
    record: Record,
}

impl Update {
    pub(super) fn load(admin: &Path) -> Result<Option<Self>> {
        let directory = admin.join("graft").join("checkout-update");
        if !registry::exists(&directory)? {
            return Ok(None);
        }
        let record = read_record(&directory)?;
        Ok(Some(Self { directory, record }))
    }

    pub(super) fn begin(admin: &Path, plan: &Plan) -> Result<Self> {
        let state = admin.join("graft");
        registry::owned_directory(&state)?;
        let mut builder = tempfile::Builder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        #[cfg(not(unix))]
        builder.prefix("checkout-");
        let staged = builder.tempdir_in(&state)?;
        let record = Record {
            plan: plan.clone(),
            token: Uuid::new_v4().to_string(),
            phase: Phase::Prepared,
            directory: directory_identity(staged.path())?,
            worker: None,
        };
        registry::atomic(
            &staged.path().join("record.json"),
            &serde_json::to_vec(&record)?,
        )?;
        let directory = state.join("checkout-update");
        recovery::rename_exclusive(&staged.keep(), &directory)?;
        #[cfg(unix)]
        fs::File::open(&state)?.sync_all()?;
        Ok(Self { directory, record })
    }

    pub(super) fn plan(&self) -> &Plan {
        &self.record.plan
    }

    pub(super) fn is_for(&self, plan: &Plan) -> bool {
        self.record.plan == *plan
    }

    fn lock_marker(&self) -> String {
        format!("git-graft checkout {}\n", self.record.token)
    }

    /// Hold the primary index. The lock of an interrupted attempt carries
    /// this journal's token; the project lock proves its process has exited.
    pub(super) fn acquire_index(&self, admin: &Path, retrying: bool) -> Result<IndexLock> {
        let path = admin.join("index.lock");
        let marker = self.lock_marker();
        if retrying && registry::exists(&path)? && registry::regular(&path)? == marker.as_bytes() {
            Ok(IndexLock { path, marker })
        } else {
            IndexLock::create(path, marker)
        }
    }

    pub(super) fn has_mutated_checkout(&self) -> bool {
        self.record.phase != Phase::Prepared
    }

    pub(super) fn finished_index(&self) -> bool {
        self.record.phase == Phase::Indexed
    }

    fn save(&self) -> Result<()> {
        registry::atomic(
            &self.directory.join("record.json"),
            &serde_json::to_vec(&self.record)?,
        )
    }

    /// Record a new phase. Every earlier private Git command has exited.
    fn set_phase(&mut self, phase: Phase) -> Result<()> {
        self.record.phase = phase;
        self.record.worker = None;
        self.save()
    }

    pub(super) fn clear(&self, admin: &Path) -> Result<()> {
        ensure!(
            read_record(&self.directory)? == self.record,
            "checkout transition changed before cleanup at {}",
            self.directory.display()
        );
        let parent = self
            .directory
            .parent()
            .context("checkout transition has no parent")?;
        let retained = parent.join(format!(".checkout-finished-{}", self.record.token));
        let path = admin.join("index.lock");
        if registry::exists(&path)? && registry::regular(&path)? == self.lock_marker().as_bytes() {
            fs::remove_file(path)?;
        }
        // Never recursively delete the live journal. An interruption may
        // leave a partial cleanup in this private path, away from retries.
        recovery::rename_exclusive(&self.directory, &retained)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        ensure!(
            read_record(&retained)? == self.record,
            "unverified checkout transition was retained at {}",
            retained.display()
        );
        for entry in fs::read_dir(&retained)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            ensure!(
                metadata.is_file()
                    && !registry::is_reparse_point(&metadata)
                    && matches!(
                        entry.file_name().to_str(),
                        Some("record.json" | "index.before" | "index.next")
                    ),
                "unrecognized data from the checkout transition was retained at {}",
                retained.display()
            );
        }
        fs::remove_dir_all(retained)?;
        Ok(())
    }

    /// Stop tracking an update that can no longer finish, because its branch
    /// moved or is no longer checked out. The checkout is not touched. The
    /// journal, including the saved original index, stays for inspection.
    pub(super) fn set_aside(self, admin: &Path) -> Result<PathBuf> {
        self.settle_worker()?;
        ensure!(
            read_record(&self.directory)? == self.record,
            "checkout transition changed before it was set aside at {}",
            self.directory.display()
        );
        let parent = self
            .directory
            .parent()
            .context("checkout transition has no parent")?;
        let retained = parent.join(format!(".checkout-set-aside-{}", self.record.token));
        let path = admin.join("index.lock");
        if registry::exists(&path)? && registry::regular(&path)? == self.lock_marker().as_bytes() {
            fs::remove_file(path)?;
        }
        recovery::rename_exclusive(&self.directory, &retained)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(retained)
    }

    pub(super) fn recovery_required(&self, cause: impl std::fmt::Display) -> anyhow::Error {
        self.stopped(cause, &[])
    }

    /// Report why recovery stopped, naming any paths that need attention.
    fn stopped(&self, cause: impl std::fmt::Display, paths: &[Vec<u8>]) -> anyhow::Error {
        let plan = &self.record.plan;
        let paths: Vec<_> = paths
            .iter()
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect();
        fault(
            "checkout_recovery_required",
            format!(
                "The checkout update of {} to {} was interrupted. Retry the merge or undo that started it, or any merge into this repository: Graft finishes or reverses the update from the files on disk and leaves every other change in place. Stopped because {cause}. The saved original index is kept in {}.",
                plan.target,
                plan.new,
                self.directory.display()
            ),
            json!({"target":plan.target,"previous":plan.old,"commit":plan.new,
                "graft_tip":plan.tip,"recovery_path":self.directory,"paths":paths}),
        )
    }

    /// Keep a recovery report as it is; wrap any other failure in one.
    pub(super) fn explain(&self, error: anyhow::Error) -> anyhow::Error {
        if error
            .downcast_ref::<Fault>()
            .is_some_and(|fault| fault.code == "checkout_recovery_required")
        {
            error
        } else {
            self.recovery_required(format!("{error:#}"))
        }
    }

    /// Wait out the native update of an interrupted attempt. Git keeps its
    /// private index lock from its first file write until it exits.
    fn settle_worker(&self) -> Result<()> {
        let lock = self.directory.join("index.next.lock");
        if !registry::exists(&lock)? {
            return Ok(());
        }
        #[cfg(unix)]
        let released = worker_exited(self.record.worker) && {
            fs::remove_file(&lock)?;
            true
        };
        // Windows refuses to delete a file that a running process holds
        // open. Every later step also verifies the files it relies on.
        #[cfg(not(unix))]
        let released = fs::remove_file(&lock).is_ok();
        if released {
            return Ok(());
        }
        Err(self.recovery_required(match self.record.worker {
            Some(pid) => format!(
                "Git process {pid} from the interrupted update may still be writing files. Wait for it to exit, then retry"
            ),
            None => format!(
                "the interrupted Git update still holds its private index lock. If no Git process is running in this repository, delete {} and retry; it only guards Graft's private index, which the retry rebuilds from the files on disk",
                lock.display()
            ),
        }))
    }

    fn index_command(&self, path: &Path, objects: Option<&Path>) -> Result<Command> {
        let mut command = git::command(path);
        command
            .env("GIT_INDEX_FILE", self.directory.join("index.next"))
            .args([
                "--no-replace-objects",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.splitIndex=false",
                "-c",
                "core.untrackedCache=false",
                "-c",
                "core.fsync=index",
                "-c",
                "core.fsyncMethod=fsync",
            ]);
        if let Some(objects) = objects {
            recovery::alternate_objects(&mut command, objects)?;
        }
        Ok(command)
    }

    /// Run a Git command that locks the private index, recording its
    /// process ID so a retry can prove it has exited before it reclaims the
    /// lock or touches the same files. `input` goes to standard input.
    fn run_private(&mut self, mut command: Command, input: Option<&[u8]>) -> Result<Output> {
        if self.record.worker.is_some() {
            // Never leave an exited process on record for a live lock.
            self.record.worker = None;
            self.save()?;
        }
        let child = command
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        self.record.worker = Some(child.id());
        let recorded = self.save();
        let (output, written) = feed_and_wait(child, input);
        recorded?;
        let output = output?;
        if output.status.success() {
            written?;
        }
        Ok(output)
    }

    /// The recorded commits an update moves from and to.
    fn sides(&self, forward: bool) -> (String, String) {
        let plan = &self.record.plan;
        if forward {
            (plan.old.clone(), plan.new.clone())
        } else {
            (plan.new.clone(), plan.old.clone())
        }
    }

    /// Run Git's two-tree merge on the private index, with `args`.
    fn read_tree(&mut self, path: &Path, objects: Option<&Path>, args: &[&str]) -> Result<()> {
        let mut command = self.index_command(path, objects)?;
        command
            .args(["read-tree", "--no-recurse-submodules"])
            .args(args);
        let output = self.run_private(command, None)?;
        if !output.status.success() {
            return Err(git_failure("update destination checkout", &output));
        }
        Ok(())
    }

    /// Check the move from one recorded commit to the other with Git's dry
    /// run, which refuses to replace an edited tracked file or an untracked
    /// one. Nothing is written.
    fn dry_run(&mut self, path: &Path, objects: Option<&Path>, forward: bool) -> Result<()> {
        let (from, to) = self.sides(forward);
        self.read_tree(path, objects, &["-m", "-u", "-n", &from, &to])
    }

    /// Move the files from `from` to `to`, keeping untracked and ignored
    /// files, and return the paths Git refused to write because a file
    /// appeared there after the checks.
    ///
    /// Git's two-tree `read-tree -u` always treats ignored files as
    /// expendable: it replaces any that lie where it writes, and has no
    /// option to keep them, unlike `git merge --no-overwrite-ignore`. So it
    /// only changes and removes paths `from` tracks. Each path `from` lacks is
    /// then written by `checkout-index`, which without `--force` refuses to
    /// replace anything there and creates each file exclusively, so a file
    /// that appears at any moment is never overwritten.
    fn write_files(
        &mut self,
        path: &Path,
        objects: Option<&Path>,
        from: &str,
        to: &str,
    ) -> Result<Vec<Vec<u8>>> {
        let created: Vec<Vec<u8>> = self
            .changes_between(path, objects, from, to)?
            .into_iter()
            .filter(|change| !change.old.present())
            .map(|change| change.path)
            .collect();
        if created.is_empty() {
            self.read_tree(path, objects, &["-m", "-u", from, to])?;
            return Ok(Vec::new());
        }
        let tracked = self.tree_without(path, objects, to, &created)?;
        self.read_tree(path, objects, &["-m", "-u", from, &tracked])?;
        // Stage the new entries without touching the files.
        self.read_tree(path, objects, &["-m", "-i", &tracked, to])?;
        let mut input = Vec::new();
        for name in &created {
            input.extend_from_slice(name);
            input.push(0);
        }
        // `read-tree -u` takes attributes from the index it builds, but
        // `checkout-index` from the files first, which may hold uncommitted
        // edits. Reading them from `to` writes what Git's own update would.
        let mut command = self.index_command(path, objects)?;
        command
            .arg(format!("--attr-source={to}"))
            .args(["checkout-index", "-u", "-z", "--stdin"]);
        let output = self.run_private(command, Some(&input))?;
        if output.status.success() {
            return Ok(Vec::new());
        }
        // Git left the index unwritten. Paths it wrote match it once
        // refreshed; the rest hold nothing yet, or what appeared there.
        self.refresh(path, objects)?;
        let differing = self.differing(path, objects)?;
        let mut refused = Vec::new();
        for name in created {
            if differing.contains_key(&name) && occupied(path, &name)? {
                refused.push(name);
            }
        }
        if refused.is_empty() {
            return Err(git_failure("write new checkout files", &output));
        }
        Ok(refused)
    }

    /// The tree `tree` without `paths`, built in a scratch index.
    fn tree_without(
        &self,
        path: &Path,
        objects: Option<&Path>,
        tree: &str,
        paths: &[Vec<u8>],
    ) -> Result<String> {
        let scratch = tempfile::tempdir()?;
        let index = scratch.path().join("index");
        let mut input = Vec::new();
        for name in paths {
            input.extend_from_slice(name);
            input.push(0);
        }
        let steps: [(&[&str], Option<&[u8]>); 3] = [
            (&["read-tree", tree], None),
            (
                &["update-index", "--force-remove", "-z", "--stdin"],
                Some(&input),
            ),
            (&["write-tree"], None),
        ];
        let mut written = Vec::new();
        for (args, stdin) in steps {
            let mut command = self.index_command(path, objects)?;
            command.env("GIT_INDEX_FILE", &index).args(args);
            let output = match stdin {
                None => command.output()?,
                Some(bytes) => {
                    let child = command
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()?;
                    let (output, written) = feed_and_wait(child, Some(bytes));
                    let output = output?;
                    written?;
                    output
                }
            };
            if !output.status.success() {
                return Err(git_failure("prepare checkout update", &output));
            }
            written = output.stdout;
        }
        Ok(String::from_utf8(written)?.trim_end().to_owned())
    }

    /// Refuse, before any file is written, when the person's uncommitted
    /// edits touch a path the update changes. Edits are staged and unstaged
    /// changes and untracked files; ignored files are `protect_ignored`'s.
    /// Edits to other paths stay exactly as they are: the two-tree
    /// `read-tree` keeps every index entry and file the update does not
    /// change, and the private index starts as a copy of the person's.
    ///
    /// Git's own two-tree check also refuses to replace a modified file or an
    /// untracked one, but only with an untyped message, and it accepts a
    /// staged version that happens to equal the merged one, a modified
    /// submodule and an edit its whole-second timestamps hide. Unresolved
    /// conflicts and hidden index flags were refused by `check_idle`.
    fn refuse_overlapping_edits(&self, path: &Path, objects: Option<&Path>) -> Result<()> {
        let edits = git::edits(path)?;
        if edits.is_empty() {
            return Ok(());
        }
        let edited: Vec<&[u8]> = edits.iter().map(|edit| &edit.name[..]).collect();
        let changes = self.changes(path, objects)?;
        let changed: Vec<&[u8]> = changes.iter().map(|change| &change.path[..]).collect();
        let refused: Vec<Vec<u8>> = crate::collision::overlapping(path, &edited, &changed)?
            .into_iter()
            .map(<[u8]>::to_vec)
            .collect();
        if refused.is_empty() {
            return Ok(());
        }
        Err(self.local_changes(
            path,
            "Uncommitted changes",
            "touch paths changed by updating",
            "Commit them or move them aside, then retry; changes to other paths can stay",
            &refused,
        ))
    }

    /// `destination_has_local_changes` for `paths`: `what` in the checkout
    /// at `path` that the update would change, such as uncommitted edits or
    /// ignored files. Nothing was written.
    fn local_changes(
        &self,
        path: &Path,
        what: &str,
        touch: &str,
        remedy: &str,
        paths: &[Vec<u8>],
    ) -> anyhow::Error {
        let plan = &self.record.plan;
        let names: Vec<_> = paths
            .iter()
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .collect();
        fault(
            "destination_has_local_changes",
            format!(
                "{what} in {} {touch} {} to {}: {}. Graft changed nothing. {remedy}",
                path.display(),
                plan.target,
                plan.new,
                listed(paths)
            ),
            json!({"target":plan.target,"previous":plan.old,"commit":plan.new,
                "checkout":path,"paths":names}),
        )
    }

    /// Refuse, before any file is written, when an ignored file lies at, in
    /// or above a path the update changes. One that appears later is kept by
    /// `write_files`.
    fn protect_ignored(&self, path: &Path, objects: Option<&Path>) -> Result<()> {
        // List ignored files against the private index, which already tracks
        // any path a resumed update wrote before its interruption.
        let ignored = self
            .index_command(path, objects)?
            .args([
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "-z",
            ])
            .output()?;
        if !ignored.status.success() {
            return Err(git_failure("list ignored checkout paths", &ignored));
        }
        let ignored = ignored.stdout;
        if ignored.is_empty() {
            return Ok(());
        }
        let entries: Vec<&[u8]> = ignored
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .collect();
        let ignored: BTreeSet<_> = entries
            .iter()
            .map(|path| path.strip_suffix(b"/").unwrap_or(path).to_ascii_lowercase())
            .collect();
        let output = self
            .index_command(path, objects)?
            .args([
                "diff-tree",
                "-r",
                "--no-renames",
                "--name-only",
                "-z",
                &self.record.plan.old,
                &self.record.plan.new,
            ])
            .output()?;
        if !output.status.success() {
            return Err(git_failure("check ignored checkout paths", &output));
        }
        let changed: Vec<&[u8]> = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .collect();
        // Byte comparison folds ASCII only. The filesystem may also fold other
        // case and Unicode normalization, so compare what each path resolves to.
        let mut overwritten: BTreeSet<Vec<u8>> =
            crate::collision::ignored_collisions(path, &entries, &changed)?
                .into_iter()
                .map(<[u8]>::to_vec)
                .collect();
        for changed in changed {
            let mut folded = changed.to_ascii_lowercase();
            let ancestor = folded
                .iter()
                .enumerate()
                .any(|(index, byte)| *byte == b'/' && ignored.contains(&folded[..index]));
            let exact = ignored.contains(&folded);
            folded.push(b'/');
            let descendant = ignored
                .range(folded.clone()..)
                .next()
                .is_some_and(|path| path.starts_with(&folded));
            if ancestor || exact || descendant {
                overwritten.insert(changed.to_vec());
            }
        }
        if overwritten.is_empty() {
            return Ok(());
        }
        let overwritten: Vec<_> = overwritten.into_iter().collect();
        Err(self.local_changes(
            path,
            "Ignored files",
            "would be overwritten by updating",
            "Move them aside, then retry",
            &overwritten,
        ))
    }

    fn matches_result(&self, path: &Path, objects: Option<&Path>) -> Result<bool> {
        if !registry::exists(&self.directory.join("index.next"))? {
            return Ok(false);
        }
        for args in [
            &[
                "diff-index",
                "--cached",
                "--quiet",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=none",
                &self.record.plan.new,
                "--",
            ][..],
            &[
                "diff-files",
                "--quiet",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=none",
                "--",
            ][..],
        ] {
            let result = self.index_command(path, objects)?.args(args).output()?;
            match result.status.code() {
                Some(0) => (),
                Some(1) => return Ok(false),
                _ => return Err(git_failure("verify recovered checkout", &result)),
            }
        }
        Ok(true)
    }

    /// Every path the update changes, with both sides' index entries.
    fn changes(&self, path: &Path, objects: Option<&Path>) -> Result<Vec<Change>> {
        let plan = &self.record.plan;
        self.changes_between(path, objects, &plan.old, &plan.new)
    }

    /// Every path that differs from `from` to `to`, with both entries.
    fn changes_between(
        &self,
        path: &Path,
        objects: Option<&Path>,
        from: &str,
        to: &str,
    ) -> Result<Vec<Change>> {
        let output = self
            .index_command(path, objects)?
            .args(["diff-tree", "-r", "--no-renames", "--raw", "-z", from, to])
            .output()?;
        if !output.status.success() {
            return Err(git_failure("list checkout changes", &output));
        }
        let mut fields = output.stdout.split(|byte| *byte == 0);
        let mut changes = Vec::new();
        while let Some(header) = fields.next().filter(|field| !field.is_empty()) {
            let path = fields
                .next()
                .filter(|field| !field.is_empty())
                .context("malformed Git tree difference")?;
            let header = std::str::from_utf8(header)?
                .strip_prefix(':')
                .context("malformed Git tree difference")?;
            let mut parts = header.split(' ');
            let (Some(old_mode), Some(new_mode), Some(old_oid), Some(new_oid)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                bail!("malformed Git tree difference");
            };
            changes.push(Change {
                path: path.to_vec(),
                old: Entry {
                    mode: old_mode.into(),
                    oid: old_oid.into(),
                },
                new: Entry {
                    mode: new_mode.into(),
                    oid: new_oid.into(),
                },
            });
        }
        Ok(changes)
    }

    /// Start the private index from `index`, or empty when there is none.
    fn seed(&self, index: Option<&[u8]>) -> Result<()> {
        let next = self.directory.join("index.next");
        match index {
            Some(bytes) => registry::atomic(&next, bytes),
            None if registry::exists(&next)? => {
                registry::regular(&next)?;
                Ok(fs::remove_file(&next)?)
            }
            None => Ok(()),
        }
    }

    /// Write a private index: `index` with the given entry for each path,
    /// refreshed against the files on disk.
    fn write_view<'a>(
        &mut self,
        path: &Path,
        objects: Option<&Path>,
        index: Option<&[u8]>,
        entries: impl IntoIterator<Item = (&'a [u8], &'a Entry)>,
    ) -> Result<()> {
        self.seed(index)?;
        // Removals go first so a file can give way to a directory.
        let mut removals = Vec::new();
        let mut additions = Vec::new();
        for (name, entry) in entries {
            let list = if entry.present() {
                &mut additions
            } else {
                &mut removals
            };
            list.extend_from_slice(format!("{} {}\t", entry.mode, entry.oid).as_bytes());
            list.extend_from_slice(name);
            list.push(0);
        }
        removals.append(&mut additions);
        if !removals.is_empty() {
            let mut command = self.index_command(path, objects)?;
            command.args(["update-index", "-z", "--index-info"]);
            let output = self.run_private(command, Some(&removals))?;
            if !output.status.success() {
                return Err(git_failure("build recovery index", &output));
            }
        }
        self.refresh(path, objects)
    }

    /// Refresh the private index's stat data from the files on disk. Git's
    /// two-tree update trusts stat data alone, so a file that was touched but
    /// still holds its staged content would otherwise count as modified. An
    /// entry whose file differs keeps its old stat data and stays modified.
    fn refresh(&mut self, path: &Path, objects: Option<&Path>) -> Result<()> {
        let mut command = self.index_command(path, objects)?;
        command.args(["update-index", "-q", "--refresh"]);
        let output = self.run_private(command, None)?;
        if !output.status.success() {
            return Err(git_failure("refresh checkout index", &output));
        }
        Ok(())
    }

    /// Paths whose files differ from the private index, with Git's status
    /// letter. `D` means Git finds no file for the entry.
    fn differing(&self, path: &Path, objects: Option<&Path>) -> Result<BTreeMap<Vec<u8>, u8>> {
        let output = self
            .index_command(path, objects)?
            .args([
                "diff-files",
                "--name-status",
                "-z",
                "--ignore-submodules=none",
                "--",
            ])
            .output()?;
        if !output.status.success() {
            return Err(git_failure("compare checkout files", &output));
        }
        let mut fields = output.stdout.split(|byte| *byte == 0);
        let mut differing = BTreeMap::new();
        while let Some(status) = fields.next().filter(|field| !field.is_empty()) {
            let name = fields.next().context("malformed Git file difference")?;
            differing.insert(name.to_vec(), status[0]);
        }
        Ok(differing)
    }

    /// Rebuild the private index from the files on disk after an interrupted
    /// update, so that `read-tree` can finish it (`forward`) or reverse it.
    /// Each changed path takes the entry whose version its file holds. Git
    /// removes a file before it writes the replacement, so a missing file
    /// takes the entry of the side without the path, if there is one, or else
    /// of the side the update moves from, and is written again. Staged entries for other paths are kept. A changed path with
    /// any other content or staged version stops recovery before a file is
    /// written. Paths in `left` are left as the primary's index has them.
    fn rebuild(
        &mut self,
        path: &Path,
        admin: &Path,
        objects: Option<&Path>,
        forward: bool,
        left: &[Vec<u8>],
    ) -> Result<()> {
        let mut changes = self.changes(path, objects)?;
        changes.retain(|change| !left.contains(&change.path));
        let staged = staged_entries(git::command(path))?;
        let mut edited = Vec::new();
        for change in &changes {
            let entry = staged.get(&change.path);
            let holds = |side: &Entry| {
                if side.present() {
                    entry == Some(side)
                } else {
                    entry.is_none()
                }
            };
            if !holds(&change.old) && !holds(&change.new) {
                edited.push(change.path.clone());
            }
        }
        if !edited.is_empty() {
            return Err(self.stopped(
                format!(
                    "these paths have staged changes made after the interruption: {}. Graft changed nothing. Record the staged versions somewhere safe, unstage them, then retry",
                    listed(&edited)
                ),
                &edited,
            ));
        }

        let index = index_bytes(&admin.join("index"))?;
        let index = index.as_deref();
        let side = |old: bool| {
            changes.iter().map(move |change| {
                (
                    &change.path[..],
                    if old { &change.old } else { &change.new },
                )
            })
        };
        self.write_view(path, objects, index, side(true))?;
        let from_old = self.differing(path, objects)?;
        self.write_view(path, objects, index, side(false))?;
        let from_new = self.differing(path, objects)?;
        // A side holds when its file is unchanged or missing. A side without
        // the path holds when Git finds no file there.
        let holds = |entry: &Entry,
                     own: &BTreeMap<Vec<u8>, u8>,
                     other: &BTreeMap<Vec<u8>, u8>,
                     name: &[u8]| {
            if entry.present() {
                own.get(name).is_none_or(|status| *status == b'D')
            } else {
                other.get(name) == Some(&b'D')
            }
        };
        let mut chosen = Vec::with_capacity(changes.len());
        for change in &changes {
            let name = &change.path;
            let old = (&change.old, &from_old, &from_new);
            let new = (&change.new, &from_new, &from_old);
            let (first, second) = if forward { (old, new) } else { (new, old) };
            let holding: Vec<_> = [first, second]
                .into_iter()
                .filter(|(entry, own, other)| holds(entry, own, other, name))
                .map(|(entry, _, _)| entry)
                .collect();
            // Both sides hold only a missing file. The side without the path
            // cannot clash with a file or directory written in its place.
            if let Some(entry) = holding
                .iter()
                .find(|entry| !entry.present())
                .or(holding.first())
            {
                chosen.push((&name[..], *entry));
            } else {
                edited.push(name.clone());
            }
        }
        if !edited.is_empty() {
            return Err(self.stopped(
                format!(
                    "these files changed after the interruption, so Graft cannot tell which version to keep: {}. Graft left them as they are. Keep a copy of your edits, return each file to its version in {} or in the merged commit, then retry",
                    listed(&edited),
                    self.record.plan.target
                ),
                &edited,
            ));
        }
        self.write_view(path, objects, index, chosen)
    }

    pub(super) fn update_index_and_files(
        &mut self,
        path: &Path,
        admin: &Path,
        objects: Option<&Path>,
        publish: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let index = admin.join("index");
        let before_path = self.directory.join("index.before");
        let next_path = self.directory.join("index.next");
        self.settle_worker()?;
        let current = index_bytes(&index)?;
        if self.has_mutated_checkout() {
            let next = index_bytes(&next_path)?;
            let unchanged = current == index_bytes(&before_path)? || current == next;
            if unchanged && self.matches_result(path, objects)? {
                let next = next.context("the finished checkout index is missing")?;
                // Native maintenance may have pruned the still-unreferenced
                // publication since the interruption. Retain the new import
                // guard through the surrounding ref commit on every attempt.
                publish()?;
                if current.as_deref() != Some(next.as_slice()) {
                    registry::atomic(&index, &next)?;
                }
                return self.set_phase(Phase::Indexed);
            }
            // The files can hold a mix of both versions, and someone may
            // have edited or staged other paths since. Git finishes the
            // update from a private index that matches the files.
            self.rebuild(path, admin, objects, true, &[])?;
        } else {
            self.refuse_overlapping_edits(path, objects)?;
            if let Some(current) = &current {
                registry::atomic(&before_path, current)?;
            }
            self.seed(current.as_deref())?;
            self.refresh(path, objects)?;
        }
        let fresh = !self.has_mutated_checkout();
        self.protect_ignored(path, objects)?;
        publish()?;
        // Git's dry run checks each file it would replace against the private
        // index, so an edit made since the first check, while publishing for
        // instance, stops the update here: just before the journal records
        // that files may change, not after. Only then is the status read
        // again, to name the edited paths.
        if let Err(error) = self.dry_run(path, objects, true) {
            if fresh {
                self.refuse_overlapping_edits(path, objects)?;
            }
            return Err(error);
        }
        self.set_phase(Phase::Updating)?;
        before_write();
        let (from, to) = self.sides(true);
        let refused = self.write_files(path, objects, &from, &to)?;
        if !refused.is_empty() {
            return Err(self.appeared(path, admin, objects, fresh, &refused));
        }
        let next = registry::regular(&next_path)?;
        registry::atomic(&index, &next)?;
        self.set_phase(Phase::Indexed)
    }

    /// Report files that appeared after the checks at paths the update
    /// creates, which Git refused to replace. A fresh update first returns
    /// every other path to the previous commit, so that it changed nothing.
    fn appeared(
        &mut self,
        path: &Path,
        admin: &Path,
        objects: Option<&Path>,
        fresh: bool,
        refused: &[Vec<u8>],
    ) -> anyhow::Error {
        if fresh {
            let backed_out = (|| -> Result<bool> {
                self.rebuild(path, admin, objects, false, refused)?;
                // The previous commit lacks the refused paths, and Git leaves
                // a path alone that neither side of the move tracks.
                let plan = self.record.plan.clone();
                let from = self.tree_without(path, objects, &plan.new, refused)?;
                if !self
                    .write_files(path, objects, &from, &plan.old)?
                    .is_empty()
                {
                    return Ok(false);
                }
                self.set_phase(Phase::Prepared)?;
                Ok(true)
            })();
            match backed_out {
                Ok(true) => {
                    return self.local_changes(
                        path,
                        "Files written during the update",
                        "would be overwritten by updating",
                        "Git kept them as they are. Move them aside, then retry",
                        refused,
                    );
                }
                Ok(false) => (),
                Err(error) => return self.explain(error),
            }
        }
        self.stopped(
            format!(
                "files appeared after Graft checked the checkout, where the update writes: {}. Git kept them as they are. Move them aside, then retry",
                listed(refused)
            ),
            refused,
        )
    }

    /// Return the files and index to the previous commit after another
    /// merge's update was interrupted. The caller holds the primary index
    /// and the destination ref locks, and has checked the branch is still at
    /// the previous commit. Afterwards the journal can be cleared.
    pub(super) fn restore_previous(&mut self, path: &Path, admin: &Path) -> Result<()> {
        self.settle_worker()?;
        if !self.has_mutated_checkout() {
            return Ok(());
        }
        self.rebuild(path, admin, None, false, &[])?;
        self.protect_ignored(path, None)?;
        self.dry_run(path, None, false)?;
        // Until the restore finishes, files can again hold either version,
        // and any later attempt resumes from them in either direction.
        self.set_phase(Phase::Updating)?;
        before_write();
        let (from, to) = self.sides(false);
        let refused = self.write_files(path, None, &from, &to)?;
        if !refused.is_empty() {
            return Err(self.appeared(path, admin, None, false, &refused));
        }
        let next = registry::regular(&self.directory.join("index.next"))?;
        registry::atomic(&admin.join("index"), &next)
    }
}

/// Wait for `child` while another thread writes `input` to it, so a child
/// that fills its output pipes before it reads all of its input, such as
/// `checkout-index` naming each path it refuses, cannot block both
/// processes. Returns the output and whether all of the input was written.
fn feed_and_wait(
    mut child: Child,
    input: Option<&[u8]>,
) -> (std::io::Result<Output>, std::io::Result<()>) {
    let stdin = child.stdin.take();
    std::thread::scope(|scope| {
        let writer = stdin
            .zip(input)
            .map(|(mut stdin, input)| scope.spawn(move || stdin.write_all(input)));
        let output = child.wait_with_output();
        let written = writer.map_or(Ok(()), |writer| {
            writer
                .join()
                .unwrap_or_else(|_| Err(std::io::Error::other("writing Git's input failed")))
        });
        (output, written)
    })
}

/// Whether anything is at `name` in the checkout at `root`, or in place of
/// one of its directories.
fn occupied(root: &Path, name: &[u8]) -> Result<bool> {
    let ends = name
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'/')
        .map(|(end, _)| (end, true))
        .chain([(name.len(), false)]);
    for (end, directory) in ends {
        match fs::symlink_metadata(crate::collision::join(root, &name[..end])?) {
            Ok(metadata) if directory && metadata.is_dir() => (),
            Ok(_) => return Ok(true),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

#[cfg(test)]
thread_local! {
    /// A test's action between Git's dry run and the first file written.
    static BEFORE_WRITE: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        std::cell::RefCell::new(None);
}

fn before_write() {
    #[cfg(test)]
    BEFORE_WRITE.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook();
        }
    });
}

pub(super) fn index_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    registry::exists(path)?
        .then(|| registry::regular(path))
        .transpose()
}

pub(super) struct IndexLock {
    path: PathBuf,
    marker: String,
}

impl IndexLock {
    pub(super) fn create(path: PathBuf, marker: String) -> Result<Self> {
        let parent = path.parent().context("Git index lock has no parent")?;
        let mut pending = tempfile::NamedTempFile::new_in(parent)?;
        pending.write_all(marker.as_bytes())?;
        pending.as_file().sync_all()?;
        pending
            .persist_noclobber(&path)
            .with_context(|| format!("cannot lock checkout index at {}", path.display()))?;
        Ok(Self { path, marker })
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if registry::regular(&self.path).is_ok_and(|bytes| bytes == self.marker.as_bytes()) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Project;

    fn fixture(format: &str) -> Result<(tempfile::TempDir, Project, fs::File, Plan)> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("repository");
        fs::create_dir(&repo)?;
        git::bytes(
            &repo,
            &[
                "init",
                "-q",
                "--template=",
                "-b",
                "main",
                &format!("--ref-format={format}"),
            ],
        )?;
        for (key, value) in [
            ("user.name", "Checkout Fixture"),
            ("user.email", "checkout@example.invalid"),
            ("core.autocrlf", "false"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "disabled-hooks"),
        ] {
            git::bytes(&repo, &["config", key, value])?;
        }
        git::bytes(&repo, &["commit", "-qm", "old", "--allow-empty"])?;
        let old = git::oid(&repo, "HEAD")?;
        git::bytes(&repo, &["branch", "other"])?;
        git::bytes(&repo, &["switch", "-qc", "incoming"])?;
        fs::write(repo.join("added"), b"landed\n")?;
        git::bytes(&repo, &["add", "added"])?;
        git::bytes(&repo, &["commit", "-qm", "new"])?;
        let new = git::oid(&repo, "HEAD")?;
        git::bytes(&repo, &["switch", "-q", "main"])?;
        let (p, lock) = Project::open(&repo)?;
        Ok((
            temp,
            p,
            lock,
            Plan {
                target: "main".into(),
                old,
                tip: new.clone(),
                new,
            },
        ))
    }

    fn apply(p: &Project, plan: &Plan, publish: impl FnOnce() -> Result<()>) -> Result<()> {
        super::super::apply_with_destination(
            &p.repo,
            plan,
            &p.repo,
            |reference, _| {
                Ok(
                    (git::symbolic_ref(&p.repo, "HEAD")?.as_deref() == Some(reference))
                        .then(|| p.repo.clone()),
                )
            },
            publish,
        )
    }

    /// A primary at `old` and a commit `new` that modifies, adds and
    /// deletes files and turns the file `swap` into a directory. It also
    /// deletes `gone.local` and adds `late.local`, which match the primary's
    /// ignore patterns.
    fn partial_fixture() -> Result<(tempfile::TempDir, Project, fs::File, Plan)> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("repository");
        fs::create_dir(&repo)?;
        git::bytes(&repo, &["init", "-q", "--template=", "-b", "main"])?;
        for (key, value) in [
            ("user.name", "Checkout Fixture"),
            ("user.email", "checkout@example.invalid"),
            ("core.autocrlf", "false"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "disabled-hooks"),
        ] {
            git::bytes(&repo, &["config", key, value])?;
        }
        for (name, content) in [
            (".gitignore", "ignored/\n*.local\n"),
            ("gone.local", "tracked though ignored\n"),
            ("a.txt", "a\n"),
            ("b.txt", "b\n"),
            ("gone.txt", "gone\n"),
            ("other.txt", "other\n"),
            ("swap", "file\n"),
            ("unrelated.txt", "unrelated\n"),
        ] {
            fs::write(repo.join(name), content)?;
        }
        git::bytes(&repo, &["add", "-A"])?;
        git::bytes(&repo, &["add", "-f", "gone.local"])?;
        git::bytes(&repo, &["commit", "-qm", "old"])?;
        let old = git::oid(&repo, "HEAD")?;
        git::bytes(&repo, &["switch", "-qc", "incoming"])?;
        fs::write(repo.join("a.txt"), "A\n")?;
        fs::write(repo.join("b.txt"), "B\n")?;
        fs::write(repo.join("added.txt"), "added\n")?;
        fs::remove_file(repo.join("gone.txt"))?;
        fs::remove_file(repo.join("swap"))?;
        fs::create_dir(repo.join("swap"))?;
        fs::write(repo.join("swap").join("inner"), "inner\n")?;
        fs::remove_file(repo.join("gone.local"))?;
        fs::write(repo.join("late.local"), "from the graft\n")?;
        git::bytes(&repo, &["add", "-A"])?;
        git::bytes(&repo, &["add", "-f", "late.local"])?;
        git::bytes(&repo, &["commit", "-qm", "new"])?;
        let new = git::oid(&repo, "HEAD")?;
        git::bytes(&repo, &["switch", "-q", "main"])?;
        fs::create_dir(repo.join("ignored"))?;
        fs::write(repo.join("ignored").join("keep"), "ignored data\n")?;
        let (p, lock) = Project::open(&repo)?;
        let plan = Plan {
            target: "main".into(),
            old,
            tip: new.clone(),
            new,
        };
        Ok((temp, p, lock, plan))
    }

    /// Leave the journal and files as a `read-tree` stopped partway leaves
    /// them: the deletions and the first update are on disk, the file being
    /// replaced has been unlinked, the rest is not written, and the primary
    /// index is untouched.
    fn interrupt(p: &Project, plan: &Plan) -> Result<Update> {
        let mut update = Update::begin(&p.common, plan)?;
        let index = fs::read(p.common.join("index"))?;
        registry::atomic(&update.directory.join("index.before"), &index)?;
        registry::atomic(&update.directory.join("index.next"), &index)?;
        update.set_phase(Phase::Updating)?;
        let repo = &p.repo;
        fs::remove_file(repo.join("gone.txt"))?;
        fs::remove_file(repo.join("gone.local"))?;
        fs::remove_file(repo.join("swap"))?;
        fs::write(repo.join("a.txt"), "A\n")?;
        fs::remove_file(repo.join("b.txt"))?;
        Ok(update)
    }

    fn read(p: &Project, name: &str) -> Result<String> {
        Ok(fs::read_to_string(p.repo.join(name))?)
    }

    fn code(error: &anyhow::Error) -> serde_json::Value {
        crate::api::error_json(error)["code"].clone()
    }

    /// Everything a person may have done in the primary after the crash.
    fn edit_primary(p: &Project) -> Result<()> {
        fs::write(p.repo.join("unrelated.txt"), "edited after the crash\n")?;
        fs::write(p.repo.join("staged.txt"), "staged after the crash\n")?;
        git::bytes(&p.repo, &["add", "staged.txt"])?;
        fs::write(p.repo.join("untracked.txt"), "untracked\n")?;
        fs::write(p.repo.join("ignored").join("later"), "later ignored\n")?;
        Ok(())
    }

    fn assert_primary_edits_kept(p: &Project) -> Result<()> {
        assert_eq!(read(p, "unrelated.txt")?, "edited after the crash\n");
        assert_eq!(read(p, "staged.txt")?, "staged after the crash\n");
        assert_eq!(read(p, "untracked.txt")?, "untracked\n");
        assert_eq!(read(p, "ignored/keep")?, "ignored data\n");
        assert_eq!(read(p, "ignored/later")?, "later ignored\n");
        let staged = git::text(&p.repo, &["diff", "--cached", "--name-only"])?;
        assert_eq!(staged, "staged.txt");
        Ok(())
    }

    /// The primary's index bytes and the content of every file by path.
    type Snapshot = (Vec<u8>, BTreeMap<PathBuf, Vec<u8>>);

    /// Every file and the index bytes of the primary, to show that a
    /// refused update changed nothing.
    fn snapshot(p: &Project) -> Result<Snapshot> {
        fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) -> Result<()> {
            for entry in fs::read_dir(dir)? {
                let path = entry?.path();
                let metadata = fs::symlink_metadata(&path)?;
                if path.file_name().is_some_and(|name| name == ".git") {
                    continue;
                }
                let relative = path.strip_prefix(root)?.to_path_buf();
                if metadata.is_dir() {
                    files.insert(relative, b"<dir>".to_vec());
                    walk(root, &path, files)?;
                } else if metadata.is_symlink() {
                    let target = fs::read_link(&path)?;
                    files.insert(relative, target.to_string_lossy().as_bytes().to_vec());
                } else {
                    files.insert(relative, fs::read(&path)?);
                }
            }
            Ok(())
        }
        let mut files = BTreeMap::new();
        walk(&p.repo, &p.repo, &mut files)?;
        Ok((fs::read(p.common.join("index"))?, files))
    }

    /// Apply `plan` expecting `destination_has_local_changes` naming
    /// `paths`, and check that nothing in the primary changed.
    fn assert_refused(p: &Project, plan: &Plan, paths: &[&str]) -> Result<()> {
        let before = snapshot(p)?;
        let error = apply(p, plan, || Ok(())).unwrap_err();
        let reported = crate::api::error_json(&error);
        assert_eq!(
            reported["code"], "destination_has_local_changes",
            "{reported}"
        );
        assert_eq!(reported["details"]["paths"], json!(paths), "{reported}");
        assert_eq!(reported["details"]["target"], json!(plan.target));
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.old);
        assert!(snapshot(p)? == before, "a refused update changed files");
        assert!(Update::load(&p.common)?.is_none());
        assert!(!p.common.join("index.lock").exists());
        Ok(())
    }

    /// Edits to paths the merge does not change, of every kind a person
    /// leaves: unstaged, staged, staged with more unstaged on top, a staged
    /// deletion, untracked files and Windows line endings.
    fn edit_unrelated(p: &Project) -> Result<()> {
        let repo = &p.repo;
        fs::write(repo.join("unrelated.txt"), "staged line\r\n")?;
        git::bytes(repo, &["add", "unrelated.txt"])?;
        fs::write(
            repo.join("unrelated.txt"),
            b"staged line\r\nunstaged \xff line\n",
        )?;
        fs::write(repo.join("staged.txt"), "staged new file\n")?;
        git::bytes(repo, &["add", "staged.txt"])?;
        git::bytes(repo, &["rm", "-q", "other.txt"])?;
        fs::create_dir(repo.join("notes"))?;
        fs::write(repo.join("notes").join("draft.md"), "untracked draft\n")?;
        fs::write(repo.join("untracked.txt"), "untracked\n")?;
        Ok(())
    }

    fn assert_unrelated_kept(p: &Project) -> Result<()> {
        let repo = &p.repo;
        assert_eq!(
            fs::read(repo.join("unrelated.txt"))?,
            b"staged line\r\nunstaged \xff line\n"
        );
        assert_eq!(
            git::bytes(repo, &["cat-file", "blob", ":unrelated.txt"])?,
            b"staged line\r\n"
        );
        assert_eq!(read(p, "staged.txt")?, "staged new file\n");
        assert!(!repo.join("other.txt").exists());
        assert_eq!(read(p, "notes/draft.md")?, "untracked draft\n");
        assert_eq!(read(p, "untracked.txt")?, "untracked\n");
        assert_eq!(read(p, "ignored/keep")?, "ignored data\n");
        let staged = git::text(repo, &["diff", "--cached", "--name-status", "--no-renames"])?;
        assert_eq!(
            staged, "D\tother.txt\nA\tstaged.txt\nM\tunrelated.txt",
            "staged changes must stay staged"
        );
        let unstaged = git::text(repo, &["diff", "--name-only"])?;
        assert_eq!(unstaged, "unrelated.txt");
        Ok(())
    }

    #[test]
    fn a_merge_keeps_unrelated_edits_byte_for_byte_and_staged_changes_staged() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        edit_unrelated(&p)?;
        apply(&p, &plan, || Ok(()))?;
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.new);
        assert_eq!(read(&p, "a.txt")?, "A\n");
        assert_eq!(read(&p, "b.txt")?, "B\n");
        assert_eq!(read(&p, "added.txt")?, "added\n");
        assert_eq!(read(&p, "swap/inner")?, "inner\n");
        assert!(!p.repo.join("gone.txt").exists());
        assert_unrelated_kept(&p)?;
        assert!(Update::load(&p.common)?.is_none());
        assert!(!p.common.join("index.lock").exists());
        Ok(())
    }

    /// The plan of an undo of the applied `plan`: a commit on top of it that
    /// returns to the previous tree.
    fn undo_plan(p: &Project, plan: &Plan) -> Result<Plan> {
        let previous = git::text(&p.repo, &["rev-parse", &format!("{}^{{tree}}", plan.old)])?;
        let reversal = git::text(
            &p.repo,
            &["commit-tree", &previous, "-p", &plan.new, "-m", "undo"],
        )?;
        Ok(Plan {
            target: plan.target.clone(),
            old: plan.new.clone(),
            new: reversal,
            tip: plan.new.clone(),
        })
    }

    #[test]
    fn an_undo_keeps_unrelated_edits() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        apply(&p, &plan, || Ok(()))?;
        let repo = &p.repo;
        let undo = undo_plan(&p, &plan)?;
        let reversal = undo.new.clone();
        fs::write(repo.join("b.txt"), "edited after the merge\n")?;
        assert_refused(&p, &undo, &["b.txt"])?;
        fs::write(repo.join("b.txt"), "B\n")?;

        edit_unrelated(&p)?;
        apply(&p, &undo, || Ok(()))?;
        assert_eq!(git::oid(repo, "HEAD")?, reversal);
        assert_eq!(read(&p, "a.txt")?, "a\n");
        assert_eq!(read(&p, "gone.txt")?, "gone\n");
        assert_eq!(read(&p, "swap")?, "file\n");
        assert!(!repo.join("added.txt").exists());
        assert_unrelated_kept(&p)?;
        Ok(())
    }

    #[test]
    fn a_retry_after_a_kill_before_any_write_keeps_unrelated_edits() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        // A kill in the Prepared phase leaves the journal and its index lock.
        let killed = || -> Result<()> {
            let update = Update::begin(&p.common, &plan)?;
            std::mem::forget(update.acquire_index(&p.common, false)?);
            Ok(())
        };
        edit_unrelated(&p)?;
        fs::write(p.repo.join("b.txt"), "my own edit\n")?;
        killed()?;
        assert_refused(&p, &plan, &["b.txt"])?;

        fs::write(p.repo.join("b.txt"), "b\n")?;
        killed()?;
        apply(&p, &plan, || Ok(()))?;
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.new);
        assert_eq!(read(&p, "b.txt")?, "B\n");
        assert_unrelated_kept(&p)?;
        assert!(Update::load(&p.common)?.is_none());
        assert!(!p.common.join("index.lock").exists());
        Ok(())
    }

    #[test]
    fn every_overlapping_edit_is_refused_with_its_paths() -> Result<()> {
        type Edit = fn(&Path) -> Result<()>;
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut cases: Vec<(&str, Edit, &[&str])> = vec![
            (
                "unstaged",
                |repo| Ok(fs::write(repo.join("b.txt"), "mine\n")?),
                &["b.txt"],
            ),
            (
                "staged only",
                |repo| {
                    fs::write(repo.join("a.txt"), "staged\n")?;
                    git::bytes(repo, &["add", "a.txt"])?;
                    Ok(())
                },
                &["a.txt"],
            ),
            (
                "staged as the merged version",
                |repo| {
                    fs::write(repo.join("a.txt"), "A\n")?;
                    git::bytes(repo, &["add", "a.txt"])?;
                    fs::write(repo.join("a.txt"), "A\nunstaged\n")?;
                    Ok(())
                },
                &["a.txt"],
            ),
            (
                "unstaged deletion",
                |repo| Ok(fs::remove_file(repo.join("b.txt"))?),
                &["b.txt"],
            ),
            (
                "untracked file where the merge adds one",
                |repo| Ok(fs::write(repo.join("added.txt"), "mine\n")?),
                &["added.txt"],
            ),
            (
                "rename of a changed file",
                |repo| {
                    git::bytes(repo, &["mv", "b.txt", "moved.txt"])?;
                    Ok(())
                },
                &["b.txt"],
            ),
            (
                "file replaced by a directory",
                |repo| {
                    fs::remove_file(repo.join("b.txt"))?;
                    fs::create_dir(repo.join("b.txt"))?;
                    fs::write(repo.join("b.txt").join("inside"), "mine\n")?;
                    Ok(())
                },
                &["b.txt", "b.txt/inside"],
            ),
            (
                "untracked file inside a path that becomes a directory",
                |repo| {
                    fs::remove_file(repo.join("swap"))?;
                    fs::create_dir(repo.join("swap"))?;
                    fs::write(repo.join("swap").join("mine"), "mine\n")?;
                    Ok(())
                },
                &["swap", "swap/mine"],
            ),
        ];
        #[cfg(unix)]
        cases.push((
            "file replaced by a symlink",
            |repo| {
                fs::remove_file(repo.join("b.txt"))?;
                std::os::unix::fs::symlink("unrelated.txt", repo.join("b.txt"))?;
                Ok(())
            },
            &["b.txt"],
        ));
        for (name, edit, paths) in cases {
            let (_temp, p, _lock, plan) = partial_fixture()?;
            edit(&p.repo).with_context(|| format!("set up {name}"))?;
            assert_refused(&p, &plan, paths).with_context(|| format!("case {name}"))?;
        }
        Ok(())
    }

    #[test]
    fn a_case_only_spelling_of_an_added_file_is_refused_where_names_fold() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        fs::write(p.repo.join("ADDED.txt"), "mine\n")?;
        if p.repo.join("added.txt").exists() {
            assert_refused(&p, &plan, &["ADDED.txt"])?;
        } else {
            apply(&p, &plan, || Ok(()))?;
            assert_eq!(read(&p, "ADDED.txt")?, "mine\n");
            assert_eq!(read(&p, "added.txt")?, "added\n");
        }
        Ok(())
    }

    #[test]
    fn an_edit_made_after_the_first_check_is_refused_before_the_journal_says_updating() -> Result<()>
    {
        type Edit = fn(&Path) -> Result<()>;
        let cases: [(Edit, &str, Edit); 2] = [
            (
                |repo| Ok(fs::write(repo.join("b.txt"), "edited during the merge\n")?),
                "b.txt",
                |repo| Ok(fs::write(repo.join("b.txt"), "b\n")?),
            ),
            (
                |repo| {
                    Ok(fs::write(
                        repo.join("added.txt"),
                        "created during the merge\n",
                    )?)
                },
                "added.txt",
                |repo| Ok(fs::remove_file(repo.join("added.txt"))?),
            ),
        ];
        for (edit, edited, revert) in cases {
            let (_temp, p, _lock, plan) = partial_fixture()?;
            let index = fs::read(p.common.join("index"))?;
            // Publishing runs after the first check and before Git's dry run.
            let error = apply(&p, &plan, || edit(&p.repo)).unwrap_err();
            let reported = crate::api::error_json(&error);
            assert_eq!(
                reported["code"], "destination_has_local_changes",
                "{reported}"
            );
            assert_eq!(reported["details"]["paths"], json!([edited]));
            assert!(read(&p, edited)?.ends_with("during the merge\n"));
            assert_eq!(read(&p, "a.txt")?, "a\n");
            assert_eq!(fs::read(p.common.join("index"))?, index);
            assert_eq!(git::oid(&p.repo, "HEAD")?, plan.old);
            // No journal is left behind, in Updating or any phase.
            assert!(Update::load(&p.common)?.is_none());
            assert!(!p.common.join("index.lock").exists());

            revert(&p.repo)?;
            apply(&p, &plan, || Ok(()))?;
            assert_eq!(git::oid(&p.repo, "HEAD")?, plan.new);
            assert_eq!(read(&p, "b.txt")?, "B\n");
            assert_eq!(read(&p, "added.txt")?, "added\n");
        }
        Ok(())
    }

    /// Run `action` with `hook` called between Git's dry run and the first
    /// file the update writes, as when someone writes in the primary then.
    fn with_late_write<T>(hook: impl FnMut() + 'static, action: impl FnOnce() -> T) -> T {
        BEFORE_WRITE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        let result = action();
        BEFORE_WRITE.with(|slot| *slot.borrow_mut() = None);
        result
    }

    /// Write `mine` at `name` in the primary after Git's dry run of `plan`,
    /// expect `destination_has_local_changes` naming `refused`, the path
    /// the update would write there, and check that the file is kept and
    /// nothing else in the primary changed.
    fn assert_late_ignored_write_refused(
        p: &Project,
        plan: &Plan,
        name: &str,
        refused: &str,
    ) -> Result<()> {
        let (index, mut files) = snapshot(p)?;
        let late = p.repo.join(name);
        let error = with_late_write(
            move || fs::write(&late, "mine\n").unwrap(),
            || apply(p, plan, || Ok(())),
        )
        .unwrap_err();
        let reported = crate::api::error_json(&error);
        assert_eq!(
            reported["code"], "destination_has_local_changes",
            "{reported}"
        );
        assert_eq!(reported["details"]["paths"], json!([refused]), "{reported}");
        files.insert(name.into(), b"mine\n".to_vec());
        assert!(
            snapshot(p)? == (index, files),
            "the refused update changed the primary"
        );
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.old);
        assert!(Update::load(&p.common)?.is_none());
        assert!(!p.common.join("index.lock").exists());
        Ok(())
    }

    #[test]
    fn a_merge_keeps_an_ignored_file_written_after_its_dry_run() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        assert_late_ignored_write_refused(&p, &plan, "late.local", "late.local")?;
        fs::remove_file(p.repo.join("late.local"))?;
        apply(&p, &plan, || Ok(()))?;
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.new);
        assert_eq!(read(&p, "late.local")?, "from the graft\n");
        Ok(())
    }

    #[test]
    fn an_undo_keeps_an_ignored_file_written_after_its_dry_run() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        apply(&p, &plan, || Ok(()))?;
        let undo = undo_plan(&p, &plan)?;
        assert_late_ignored_write_refused(&p, &undo, "gone.local", "gone.local")?;
        fs::remove_file(p.repo.join("gone.local"))?;
        // The undo turns the directory `swap` back into a file.
        assert_late_ignored_write_refused(&p, &undo, "swap/late.local", "swap")?;
        assert_eq!(read(&p, "swap/inner")?, "inner\n");
        fs::remove_file(p.repo.join("swap").join("late.local"))?;
        apply(&p, &undo, || Ok(()))?;
        assert_eq!(git::oid(&p.repo, "HEAD")?, undo.new);
        assert_eq!(read(&p, "gone.local")?, "tracked though ignored\n");
        assert_eq!(read(&p, "swap")?, "file\n");
        assert!(!p.repo.join("late.local").exists());
        Ok(())
    }

    #[test]
    fn a_retry_keeps_an_ignored_file_written_after_its_dry_run() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        drop(interrupt(&p, &plan)?);
        let late = p.repo.join("late.local");
        let mine = late.clone();
        let error = with_late_write(
            move || fs::write(&mine, "mine\n").unwrap(),
            || apply(&p, &plan, || Ok(())),
        )
        .unwrap_err();
        let reported = crate::api::error_json(&error);
        assert_eq!(reported["code"], "checkout_recovery_required", "{reported}");
        assert_eq!(reported["details"]["paths"], json!(["late.local"]));
        assert_eq!(fs::read_to_string(&late)?, "mine\n");
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.old);

        fs::remove_file(&late)?;
        apply(&p, &plan, || Ok(()))?;
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.new);
        assert_eq!(read(&p, "late.local")?, "from the graft\n");
        assert_eq!(read(&p, "b.txt")?, "B\n");
        Ok(())
    }

    #[test]
    fn reversing_a_stopped_update_keeps_an_ignored_file_written_after_its_dry_run() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        let other = second_plan(&p, &plan)?;
        drop(interrupt(&p, &plan)?);
        let gone = p.repo.join("gone.local");
        let mine = gone.clone();
        let error = with_late_write(
            move || fs::write(&mine, "mine\n").unwrap(),
            || apply(&p, &other, || Ok(())),
        )
        .unwrap_err();
        let reported = crate::api::error_json(&error);
        assert_eq!(reported["code"], "checkout_recovery_required", "{reported}");
        assert_eq!(reported["details"]["paths"], json!(["gone.local"]));
        assert_eq!(fs::read_to_string(&gone)?, "mine\n");
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.old);

        fs::remove_file(&gone)?;
        apply(&p, &other, || Ok(()))?;
        assert_eq!(git::oid(&p.repo, "HEAD")?, other.new);
        assert_eq!(read(&p, "gone.local")?, "tracked though ignored\n");
        assert!(!p.repo.join("late.local").exists());
        Ok(())
    }

    /// A primary at `old` holding `before` and a commit `new` that adds
    /// `added`, committed even where the ignore patterns match.
    fn adding_fixture(
        before: &[(String, String)],
        added: &[(String, String)],
    ) -> Result<(tempfile::TempDir, Project, fs::File, Plan)> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("repository");
        fs::create_dir(&repo)?;
        git::bytes(&repo, &["init", "-q", "--template=", "-b", "main"])?;
        for (key, value) in [
            ("user.name", "Checkout Fixture"),
            ("user.email", "checkout@example.invalid"),
            ("core.autocrlf", "false"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "disabled-hooks"),
        ] {
            git::bytes(&repo, &["config", key, value])?;
        }
        let write = |files: &[(String, String)]| -> Result<()> {
            for (name, content) in files {
                let file = repo.join(name);
                fs::create_dir_all(file.parent().context("no parent")?)?;
                fs::write(file, content)?;
            }
            git::bytes(&repo, &["add", "-A", "-f"])?;
            Ok(())
        };
        write(before)?;
        git::bytes(&repo, &["commit", "-qm", "old"])?;
        let old = git::oid(&repo, "HEAD")?;
        git::bytes(&repo, &["switch", "-qc", "incoming"])?;
        write(added)?;
        git::bytes(&repo, &["commit", "-qm", "new"])?;
        let new = git::oid(&repo, "HEAD")?;
        git::bytes(&repo, &["switch", "-q", "main"])?;
        let (p, lock) = Project::open(&repo)?;
        let plan = Plan {
            target: "main".into(),
            old,
            tip: new.clone(),
            new,
        };
        Ok((temp, p, lock, plan))
    }

    #[test]
    fn a_new_file_is_written_with_the_attributes_git_merge_uses() -> Result<()> {
        let attributes = |eol: &str| (".gitattributes".into(), format!("*.txt text eol={eol}\n"));
        let (_temp, p, _lock, plan) = adding_fixture(
            &[attributes("lf")],
            &[("new.txt".into(), "one\ntwo\n".into())],
        )?;
        // An uncommitted edit the merge does not touch. Git's own checkout
        // reads attributes from the index, never from this file.
        let (name, content) = attributes("crlf");
        fs::write(p.repo.join(name), content)?;
        apply(&p, &plan, || Ok(()))?;
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.new);
        assert_eq!(read(&p, "new.txt")?, "one\ntwo\n");
        Ok(())
    }

    #[test]
    fn many_files_written_after_the_dry_run_are_all_refused_without_a_hang() -> Result<()> {
        let names: Vec<String> = (0..1500)
            .map(|n| format!("many/{}-{n:04}.local", "x".repeat(80)))
            .collect();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let body = || -> Result<()> {
                let (_temp, p, _lock, plan) = adding_fixture(
                    &[(".gitignore".into(), "*.local\n".into())],
                    &names
                        .iter()
                        .map(|name| (name.clone(), "from the graft\n".into()))
                        .collect::<Vec<_>>(),
                )?;
                let (repo, late) = (p.repo.clone(), names.clone());
                let error = with_late_write(
                    move || {
                        fs::create_dir_all(repo.join("many")).unwrap();
                        for name in &late {
                            fs::write(repo.join(name), "mine\n").unwrap();
                        }
                    },
                    || apply(&p, &plan, || Ok(())),
                )
                .unwrap_err();
                let reported = crate::api::error_json(&error);
                assert_eq!(
                    reported["code"], "destination_has_local_changes",
                    "{reported}"
                );
                assert_eq!(reported["details"]["paths"], json!(names));
                for name in &names {
                    assert_eq!(read(&p, name)?, "mine\n");
                }
                assert_eq!(git::oid(&p.repo, "HEAD")?, plan.old);
                assert!(Update::load(&p.common)?.is_none());
                Ok(())
            };
            let _ = sender.send(body());
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(300))
            .context("the update hung")?
    }

    #[test]
    fn a_modified_submodule_blocks_only_a_merge_that_moves_it() -> Result<()> {
        let (_temp, p, _lock, _plan) = fixture("files")?;
        let repo = &p.repo;
        // A nested repository the primary tracks as a gitlink.
        let sub = repo.join("sub");
        fs::create_dir(&sub)?;
        git::bytes(&sub, &["init", "-q", "--template=", "-b", "main"])?;
        for (key, value) in [
            ("user.name", "Checkout Fixture"),
            ("user.email", "checkout@example.invalid"),
            ("commit.gpgsign", "false"),
        ] {
            git::bytes(&sub, &["config", key, value])?;
        }
        fs::write(sub.join("lib.txt"), "one\n")?;
        git::bytes(&sub, &["add", "lib.txt"])?;
        git::bytes(&sub, &["commit", "-qm", "one"])?;
        let first = git::oid(&sub, "HEAD")?;
        fs::write(sub.join("lib.txt"), "two\n")?;
        git::bytes(&sub, &["commit", "-qam", "two"])?;
        let second = git::oid(&sub, "HEAD")?;
        git::bytes(&sub, &["switch", "-q", "--detach", &first])?;
        git::bytes(
            repo,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{first},sub"),
            ],
        )?;
        git::bytes(repo, &["commit", "-qm", "track sub"])?;
        let tracked = git::oid(repo, "HEAD")?;
        git::bytes(
            repo,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{second},sub"),
            ],
        )?;
        git::bytes(repo, &["commit", "-qm", "move sub"])?;
        let moved = git::oid(repo, "HEAD")?;
        git::bytes(repo, &["reset", "-q", &tracked])?;
        fs::write(sub.join("lib.txt"), "local edit\n")?;

        let bump = Plan {
            target: "main".into(),
            old: tracked.clone(),
            tip: moved.clone(),
            new: moved.clone(),
        };
        assert_refused(&p, &bump, &["sub"])?;
        assert_eq!(fs::read_to_string(sub.join("lib.txt"))?, "local edit\n");

        // A merge that leaves the gitlink alone goes ahead around it.
        git::bytes(repo, &["switch", "-qc", "beside", &tracked])?;
        fs::write(repo.join("beside.txt"), "beside\n")?;
        git::bytes(repo, &["add", "beside.txt"])?;
        git::bytes(repo, &["commit", "-qm", "beside"])?;
        let beside = git::oid(repo, "HEAD")?;
        git::bytes(repo, &["switch", "-q", "main"])?;
        apply(
            &p,
            &Plan {
                target: "main".into(),
                old: tracked,
                tip: beside.clone(),
                new: beside.clone(),
            },
            || Ok(()),
        )?;
        assert_eq!(git::oid(repo, "HEAD")?, beside);
        assert_eq!(fs::read_to_string(sub.join("lib.txt"))?, "local edit\n");
        assert_eq!(git::oid(&sub, "HEAD")?, first);
        Ok(())
    }

    #[test]
    fn retry_finishes_a_partial_update_and_keeps_later_primary_edits() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        drop(interrupt(&p, &plan)?);
        edit_primary(&p)?;
        apply(&p, &plan, || Ok(()))?;
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.new);
        assert_eq!(read(&p, "a.txt")?, "A\n");
        assert_eq!(read(&p, "b.txt")?, "B\n");
        assert_eq!(read(&p, "added.txt")?, "added\n");
        assert_eq!(read(&p, "swap/inner")?, "inner\n");
        assert!(!p.repo.join("gone.txt").exists());
        assert_primary_edits_kept(&p)?;
        let changed = git::text(&p.repo, &["diff", "--name-only"])?;
        assert_eq!(changed, "unrelated.txt");
        assert!(Update::load(&p.common)?.is_none());
        assert!(!p.common.join("index.lock").exists());
        Ok(())
    }

    /// A reboot can renumber the device between a crash and the retry. The
    /// transition record still names its directory; one naming another
    /// directory does not.
    #[test]
    fn retry_finishes_an_update_interrupted_before_a_reboot() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        let update = interrupt(&p, &plan)?;
        let path = update.directory.join("record.json");
        drop(update);
        let original: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        let rewrite = |field: &str| -> Result<()> {
            let mut record = original.clone();
            let id = &mut record["directory"][field];
            *id = (id.as_u64().context("missing identity")? + 1).into();
            registry::atomic(&path, &serde_json::to_vec(&record)?)
        };
        rewrite("file")?;
        assert!(Update::load(&p.common).is_err());
        rewrite("device")?;
        if cfg!(windows) {
            assert!(Update::load(&p.common).is_err());
            return Ok(());
        }
        apply(&p, &plan, || Ok(()))?;
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.new);
        assert_eq!(read(&p, "a.txt")?, "A\n");
        assert_eq!(read(&p, "b.txt")?, "B\n");
        assert!(Update::load(&p.common)?.is_none());
        Ok(())
    }

    #[test]
    fn retry_finishes_an_update_stopped_after_a_directory_became_a_file() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        apply(&p, &plan, || Ok(()))?;
        let undo = undo_plan(&p, &plan)?;
        // The undo stopped after Git removed `swap/inner` and wrote `swap`.
        let mut update = Update::begin(&p.common, &undo)?;
        let index = fs::read(p.common.join("index"))?;
        registry::atomic(&update.directory.join("index.before"), &index)?;
        registry::atomic(&update.directory.join("index.next"), &index)?;
        update.set_phase(Phase::Updating)?;
        drop(update);
        fs::remove_dir_all(p.repo.join("swap"))?;
        fs::write(p.repo.join("swap"), "file\n")?;
        apply(&p, &undo, || Ok(()))?;
        assert_eq!(git::oid(&p.repo, "HEAD")?, undo.new);
        assert_eq!(read(&p, "swap")?, "file\n");
        assert_eq!(read(&p, "a.txt")?, "a\n");
        assert!(git::changes(&p.repo)?.iter().all(|c| c.status == "!!"));
        assert!(Update::load(&p.common)?.is_none());
        Ok(())
    }

    #[test]
    fn retry_stops_without_writing_when_a_changed_file_was_edited() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        drop(interrupt(&p, &plan)?);
        fs::write(p.repo.join("b.txt"), "my own edit\n")?;
        fs::write(p.repo.join("added.txt"), "my new file\n")?;
        let index = fs::read(p.common.join("index"))?;
        let error = apply(&p, &plan, || Ok(())).unwrap_err();
        let details = crate::api::error_json(&error);
        assert_eq!(details["code"], "checkout_recovery_required");
        assert_eq!(
            details["details"]["paths"],
            json!(["added.txt", "b.txt"]),
            "{details}"
        );
        assert_eq!(read(&p, "b.txt")?, "my own edit\n");
        assert_eq!(read(&p, "added.txt")?, "my new file\n");
        assert_eq!(read(&p, "a.txt")?, "A\n");
        assert_eq!(fs::read(p.common.join("index"))?, index);
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.old);

        // Returning each file to either version lets the retry finish.
        fs::write(p.repo.join("b.txt"), "b\n")?;
        fs::write(p.repo.join("added.txt"), "added\n")?;
        apply(&p, &plan, || Ok(()))?;
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.new);
        assert_eq!(read(&p, "b.txt")?, "B\n");
        assert!(git::changes(&p.repo)?.iter().all(|c| c.status == "!!"));
        Ok(())
    }

    #[test]
    fn retry_stops_when_a_changed_path_was_staged_after_the_crash() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        drop(interrupt(&p, &plan)?);
        fs::write(p.repo.join("b.txt"), "staged version\n")?;
        git::bytes(&p.repo, &["add", "b.txt"])?;
        fs::write(p.repo.join("b.txt"), "b\n")?;
        let error = apply(&p, &plan, || Ok(())).unwrap_err();
        assert_eq!(code(&error), "checkout_recovery_required");
        assert!(format!("{error:#}").contains("staged changes"), "{error:#}");
        let staged = git::text(&p.repo, &["show", ":b.txt"])?;
        assert_eq!(staged, "staged version");
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.old);
        Ok(())
    }

    /// Another merge into the primary's branch at `plan.old`, adding
    /// `second.txt`.
    fn second_plan(p: &Project, plan: &Plan) -> Result<Plan> {
        let repo = &p.repo;
        git::bytes(repo, &["switch", "-qc", "second", &plan.old])?;
        fs::write(repo.join("second.txt"), "second\n")?;
        git::bytes(repo, &["add", "second.txt"])?;
        git::bytes(repo, &["commit", "-qm", "second"])?;
        let second = git::oid(repo, "HEAD")?;
        git::bytes(repo, &["switch", "-q", "main"])?;
        Ok(Plan {
            target: "main".into(),
            old: plan.old.clone(),
            tip: second.clone(),
            new: second,
        })
    }

    #[test]
    fn another_merge_restores_the_previous_commit_before_it_applies() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        let repo = &p.repo;
        let other = second_plan(&p, &plan)?;
        let second = other.new.clone();
        drop(interrupt(&p, &plan)?);
        edit_primary(&p)?;
        // The interrupted update is reversed first. The new merge then
        // applies around the person's edits, none of which it changes.
        apply(&p, &other, || Ok(()))?;
        assert!(Update::load(&p.common)?.is_none());
        assert_eq!(git::oid(repo, "HEAD")?, second);
        assert_eq!(read(&p, "second.txt")?, "second\n");
        assert_eq!(read(&p, "a.txt")?, "a\n");
        assert_eq!(read(&p, "b.txt")?, "b\n");
        assert_eq!(read(&p, "gone.txt")?, "gone\n");
        assert_eq!(read(&p, "swap")?, "file\n");
        assert!(!repo.join("added.txt").exists());
        assert_primary_edits_kept(&p)?;
        let changed = git::text(repo, &["diff", "--name-only"])?;
        assert_eq!(changed, "unrelated.txt");
        Ok(())
    }

    #[test]
    fn a_moved_destination_sets_the_update_aside_without_touching_files() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        let update = interrupt(&p, &plan)?;
        let token = update.record.token.clone();
        drop(update);
        git::bytes(&p.repo, &["commit", "-qm", "moved", "--allow-empty"])?;
        let moved = git::oid(&p.repo, "HEAD")?;
        let error = apply(&p, &plan, || Ok(())).unwrap_err();
        assert_eq!(code(&error), "checkout_update_set_aside");
        let retained = p.state.join(format!(".checkout-set-aside-{token}"));
        assert!(retained.join("index.before").is_file());
        assert!(Update::load(&p.common)?.is_none());
        assert_eq!(git::oid(&p.repo, "HEAD")?, moved);
        assert_eq!(read(&p, "a.txt")?, "A\n");
        assert!(!p.repo.join("b.txt").exists());
        assert!(!p.repo.join("gone.txt").exists());
        Ok(())
    }

    #[test]
    fn every_command_that_locks_the_private_index_is_recorded() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        let mut update = Update::begin(&p.common, &plan)?;
        let index = fs::read(p.common.join("index"))?;
        update.seed(Some(&index))?;
        let recorded =
            |update: &Update| -> Result<Option<u32>> { Ok(read_record(&update.directory)?.worker) };
        assert_eq!(recorded(&update)?, None);
        update.dry_run(&p.repo, None, true)?;
        let dry_run = recorded(&update)?.context("the dry run was not recorded")?;
        let changes = update.changes(&p.repo, None)?;
        update.write_view(
            &p.repo,
            None,
            Some(&index),
            changes.iter().map(|c| (&c.path[..], &c.new)),
        )?;
        let view = recorded(&update)?.context("the recovery index was not recorded")?;
        assert_ne!(view, dry_run);
        update.set_phase(Phase::Updating)?;
        assert_eq!(recorded(&update)?, None);
        update.clear(&p.common)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn retry_waits_for_a_live_worker_and_reclaims_a_dead_one() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        let mut update = interrupt(&p, &plan)?;
        let lock = update.directory.join("index.next.lock");
        fs::write(&lock, b"partial")?;
        update.record.worker = Some(std::process::id());
        update.save()?;
        let error = apply(&p, &plan, || Ok(())).unwrap_err();
        assert_eq!(code(&error), "checkout_recovery_required");
        assert!(
            format!("{error:#}").contains("may still be writing"),
            "{error:#}"
        );
        assert!(lock.exists());
        assert!(!p.repo.join("b.txt").exists());

        update.record.worker = None;
        update.save()?;
        assert!(apply(&p, &plan, || Ok(())).is_err());
        assert!(lock.exists());

        let mut exited = std::process::Command::new("true").spawn()?;
        exited.wait()?;
        update.record.worker = Some(exited.id());
        update.save()?;
        apply(&p, &plan, || Ok(()))?;
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.new);
        assert_eq!(read(&p, "b.txt")?, "B\n");
        assert!(Update::load(&p.common)?.is_none());
        Ok(())
    }

    #[test]
    fn locks_exact_head_branch_and_index_on_both_ref_backends() -> Result<()> {
        for format in ["files", "reftable"] {
            let (_temp, p, _lock, plan) = fixture(format)?;
            let repo = &p.repo;
            apply(&p, &plan, || {
                for args in [
                    &["symbolic-ref", "HEAD", "refs/heads/other"][..],
                    &["update-ref", "refs/heads/main", &plan.new, &plan.old][..],
                    &["read-tree", "HEAD"][..],
                ] {
                    assert!(
                        !git::raw(repo, args)?.status.success(),
                        "{format} allowed {args:?}"
                    );
                }
                assert_eq!(git::oid(repo, "HEAD")?, plan.old);
                assert!(!repo.join("added").exists());
                Ok(())
            })?;
            assert_eq!(git::oid(repo, "refs/heads/main")?, plan.new);
            assert_eq!(git::oid(repo, "refs/heads/other")?, plan.old);
            assert_eq!(
                git::symbolic_ref(repo, "HEAD")?.as_deref(),
                Some("refs/heads/main")
            );
            assert_eq!(fs::read(repo.join("added"))?, b"landed\n");
            assert!(git::changes(repo)?.is_empty());
            assert!(Update::load(&p.common)?.is_none());
        }
        Ok(())
    }

    #[test]
    fn refuses_when_head_switches_before_git_prepares_the_update() -> Result<()> {
        let (_temp, p, _lock, plan) = fixture("files")?;
        let previous_index = fs::read(p.common.join("index"))?;
        let error = super::super::apply_with_destination(
            &p.repo,
            &plan,
            &p.repo,
            |_, owns_head_lock| {
                if owns_head_lock {
                    return Ok(None);
                }
                git::bytes(&p.repo, &["symbolic-ref", "HEAD", "refs/heads/other"])?;
                Ok(Some(p.repo.clone()))
            },
            || panic!("the checkout changed before publication"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("destination checkout changed"));
        assert_eq!(git::oid(&p.repo, "refs/heads/main")?, plan.old);
        assert_eq!(git::oid(&p.repo, "refs/heads/other")?, plan.old);
        assert_eq!(fs::read(p.common.join("index"))?, previous_index);
        assert!(!p.repo.join("added").exists());
        assert!(Update::load(&p.common)?.is_none());
        Ok(())
    }

    #[test]
    fn locks_primary_head_while_advancing_an_unchecked_branch() -> Result<()> {
        for format in ["files", "reftable"] {
            for detached in [false, true] {
                let (_temp, p, _lock, mut plan) = fixture(format)?;
                let repo = &p.repo;
                git::bytes(repo, &["branch", "unchecked"])?;
                if detached {
                    git::bytes(repo, &["switch", "-q", "--detach"])?;
                }
                plan.target = "unchecked".into();
                apply(&p, &plan, || {
                    assert!(
                        !git::raw(repo, &["symbolic-ref", "HEAD", "refs/heads/unchecked"])?
                            .status
                            .success()
                    );
                    Ok(())
                })?;
                assert_eq!(git::oid(repo, "HEAD")?, plan.old);
                assert_eq!(git::oid(repo, "refs/heads/unchecked")?, plan.new);
                assert!(!repo.join("added").exists());
            }
        }
        Ok(())
    }

    #[test]
    fn resumes_a_finalized_index_after_native_gc_prunes_the_landing() -> Result<()> {
        let (temp, p, _lock, plan) = fixture("files")?;
        let private = temp.path().join("private objects");
        git::bytes(
            temp.path(),
            &[
                "clone",
                "--bare",
                "--no-local",
                git::path_str(&p.repo)?,
                git::path_str(&private)?,
            ],
        )?;
        let mut transition = Update::begin(&p.common, &plan)?;
        {
            let _index = transition.acquire_index(&p.common, false)?;
            transition.update_index_and_files(&p.repo, &p.common, None, || Ok(()))?;
        }
        assert!(transition.finished_index());
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.old);
        assert_eq!(fs::read(p.repo.join("added"))?, b"landed\n");
        super::super::require_no_pending_checkout(&p.repo, &plan).unwrap_err();

        git::bytes(&p.repo, &["branch", "-D", "incoming"])?;
        git::bytes(
            &p.repo,
            &[
                "reflog",
                "expire",
                "--all",
                "--expire=now",
                "--expire-unreachable=now",
            ],
        )?;
        git::bytes(&p.repo, &["gc", "--prune=now"])?;
        assert!(git::oid(&p.repo, &plan.new).is_err());
        let mut published = false;
        super::super::apply_with_destination(
            &p.repo,
            &plan,
            &private,
            |reference, _| {
                Ok(
                    (git::symbolic_ref(&p.repo, "HEAD")?.as_deref() == Some(reference))
                        .then(|| p.repo.clone()),
                )
            },
            || {
                assert_eq!(git::oid(&p.repo, "HEAD")?, plan.old);
                git::bytes(
                    &p.repo,
                    &[
                        "-c",
                        "protocol.file.allow=always",
                        "fetch",
                        "--no-write-fetch-head",
                        "--no-auto-maintenance",
                        "--no-tags",
                        git::path_str(&private)?,
                        &plan.new,
                    ],
                )?;
                published = true;
                Ok(())
            },
        )?;
        assert!(published);
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.new);
        git::bytes(&p.repo, &["fsck", "--full", "--no-dangling"])?;
        assert!(git::changes(&p.repo)?.is_empty());
        assert!(Update::load(&p.common)?.is_none());
        Ok(())
    }

    #[test]
    fn undo_replay_clears_only_its_own_committed_checkout_update() -> Result<()> {
        let (_temp, p, _lock, plan) = fixture("files")?;
        apply(&p, &plan, || Ok(()))?;
        let repo = &p.repo;
        let previous_tree = git::text(repo, &["rev-parse", &format!("{}^{{tree}}", plan.old)])?;
        let candidate = git::text(
            repo,
            &["commit-tree", &previous_tree, "-p", &plan.new, "-m", "undo"],
        )?;
        let pin = "refs/graft/test/undo";
        git::create_private_ref(repo, pin, &candidate)?;
        let undo = Plan {
            target: plan.target.clone(),
            old: plan.new.clone(),
            new: candidate.clone(),
            tip: plan.new.clone(),
        };
        let mut update = Update::begin(&p.common, &undo)?;
        {
            let _index = update.acquire_index(&p.common, false)?;
            update.update_index_and_files(repo, &p.common, None, || Ok(()))?;
            git::bytes(repo, &["update-ref", "HEAD", &candidate, &plan.new])?;
        }
        assert!(Update::load(&p.common)?.is_some());

        let replay = || {
            crate::undo::reverse(repo, repo, &plan, pin, |_| {
                panic!("undo replay must not apply or republish")
            })
        };
        assert_eq!(replay()?["commit"], candidate);
        assert!(Update::load(&p.common)?.is_none());
        assert!(git::changes(repo)?.is_empty());

        let unrelated = Update::begin(&p.common, &plan)?;
        assert_eq!(replay()?["commit"], candidate);
        assert!(Update::load(&p.common)?.is_some_and(|update| update.is_for(&plan)));
        unrelated.clear(&p.common)?;
        Ok(())
    }

    #[test]
    fn clears_an_indexed_update_after_the_destination_advances() -> Result<()> {
        let (_temp, p, _lock, plan) = fixture("files")?;
        let mut update = Update::begin(&p.common, &plan)?;
        {
            let _index = update.acquire_index(&p.common, false)?;
            update.update_index_and_files(&p.repo, &p.common, None, || Ok(()))?;
            git::bytes(&p.repo, &["update-ref", "HEAD", &plan.new, &plan.old])?;
        }
        git::bytes(&p.repo, &["commit", "-qm", "later", "--allow-empty"])?;
        let current = git::oid(&p.repo, "HEAD")?;
        fs::write(p.repo.join("added"), b"local changes")?;
        let index = fs::read(p.common.join("index"))?;

        super::super::finish_applied_checkout(&p.repo, &plan)?;
        assert!(Update::load(&p.common)?.is_none());
        assert_eq!(git::oid(&p.repo, "HEAD")?, current);
        assert_eq!(fs::read(p.repo.join("added"))?, b"local changes");
        assert_eq!(fs::read(p.common.join("index"))?, index);
        Ok(())
    }

    #[test]
    fn detaches_before_final_cleanup_and_retains_unrecognized_contents() -> Result<()> {
        let (_temp, p, _lock, plan) = fixture("files")?;
        let update = Update::begin(&p.common, &plan)?;
        let retained = p
            .state
            .join(format!(".checkout-finished-{}", update.record.token));
        fs::write(update.directory.join("extra"), b"retain")?;
        assert!(update.clear(&p.common).is_err());
        assert!(Update::load(&p.common)?.is_none());
        assert_eq!(fs::read(retained.join("extra"))?, b"retain");

        // A crash during cleanup can leave a detached directory without its
        // record. The next operation can still use the live transition name.
        fs::remove_file(retained.join("record.json"))?;
        let fresh = Update::begin(&p.common, &plan)?;
        fresh.clear(&p.common)?;
        assert_eq!(fs::read(retained.join("extra"))?, b"retain");
        Ok(())
    }
}
