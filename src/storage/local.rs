//! Direct CoW reuse from a trusted local checkout. Git still owns the tree and
//! index, and supplies every file the donor cannot be proven to provide.
use super::*;
use std::path::PathBuf;

#[derive(Default)]
struct CheckoutMetadata<'a> {
    objects: Option<&'a Path>,
    index: Option<&'a TipIndex>,
}

pub(crate) fn materialize_local(
    workspace: &Path,
    state: &Path,
    oid: &str,
    sparse: &[String],
    primary: &Path,
) -> Result<Value> {
    let workspace = dunce::canonicalize(workspace)?;
    materialize(
        &workspace,
        state,
        oid,
        sparse,
        primary,
        CheckoutMetadata::default(),
        || Ok(()),
    )
}

/// Prepare files from a verified manifest and join history checks before index publication.
pub(crate) fn materialize_planned<F: FnOnce() -> Result<()>>(
    workspace: &Path,
    state: &Path,
    oid: &str,
    primary: &Path,
    proof_objects: Option<&Path>,
    manifest: Option<&TipIndex>,
    ready: F,
) -> Result<Value> {
    let mut ready = Some(ready);
    let result = (|| {
        let workspace = dunce::canonicalize(workspace)?;
        materialize(
            &workspace,
            state,
            oid,
            &[],
            primary,
            CheckoutMetadata {
                objects: proof_objects,
                index: manifest,
            },
            || {
                ready.take().expect("checkout readiness called once")()?;
                checkout_identity(&workspace, oid)?;
                Ok(())
            },
        )
    })();
    // Always join the history checks, including when planning fails early.
    if let Some(ready) = ready {
        let joined = ready();
        if result.is_ok() {
            joined?;
        }
    }
    result
}

fn materialize<F: FnOnce() -> Result<()>>(
    workspace: &Path,
    state: &Path,
    oid: &str,
    sparse: &[String],
    primary: &Path,
    metadata: CheckoutMetadata<'_>,
    ready: F,
) -> Result<Value> {
    // Blob sizes only depend on object IDs, and the primary's index listing
    // only selects candidates that are proven later, so the primary reports
    // both while the workspace index is prepared.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return std::thread::scope(|scope| {
        let early = Early {
            sizes: metadata.index.map(|manifest| {
                scope.spawn(move || direct::blob_sizes(|| git_command(primary), &manifest.entries))
            }),
            tracked: sparse
                .is_empty()
                .then(|| scope.spawn(|| direct::donor_index(primary))),
        };
        materialize_with(
            workspace, state, oid, sparse, primary, metadata, ready, early,
        )
    });
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    materialize_with(workspace, state, oid, sparse, primary, metadata, ready, ())
}

/// Primary queries started before the checkout plan needs them.
#[cfg(any(target_os = "macos", target_os = "linux"))]
struct Early<'s> {
    /// Blob sizes of a planned manifest.
    sizes: Option<std::thread::ScopedJoinHandle<'s, Option<std::collections::HashMap<String, u64>>>>,
    /// The primary's index listing, for a full checkout.
    tracked: Option<std::thread::ScopedJoinHandle<'s, Result<Option<Vec<u8>>>>>,
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
type Early<'s> = ();

#[allow(clippy::too_many_arguments)]
fn materialize_with<F: FnOnce() -> Result<()>>(
    workspace: &Path,
    state: &Path,
    oid: &str,
    sparse: &[String],
    primary: &Path,
    metadata: CheckoutMetadata<'_>,
    ready: F,
    early: Early<'_>,
) -> Result<Value> {
    let proof_objects = metadata.objects;
    let oid = checkout_identity_with_objects(workspace, oid, proof_objects)?;
    validate_sparse(workspace, &oid, sparse)?;
    let mut checkout = LocalCheckout::reserve(workspace, proof_objects)?;
    crate::trace::mark("checkout: identity and reserve");
    let listed;
    let files = if let Some(manifest) = metadata.index {
        if manifest.prepare(&checkout).is_err() {
            ready()?;
            drop(checkout);
            return materialize_local(workspace, state, &oid, sparse, primary);
        }
        checkout.planned = true;
        crate::trace::mark("checkout: prepare index");
        &manifest.entries
    } else {
        listed = entries_with_objects(workspace, &oid, proof_objects)?;
        &listed
    };

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let receipt = {
        let planned = direct::plan(&checkout, &oid, sparse, files, state, primary, early)?;
        crate::trace::mark("checkout: plan donors");
        match planned {
            Ok(plan) => direct::install(&checkout, &oid, files, plan, ready)?,
            Err(reason) => {
                ready()?;
                checkout.native(&oid, sparse, files, reason)?
            }
        }
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let receipt = {
        let _ = (state, primary, early);
        ready()?;
        // Report the same reason as the cloning platforms for a sparse request.
        let reason = if sparse.is_empty() {
            "native clone unavailable"
        } else {
            "sparse checkout"
        };
        checkout.native(&oid, sparse, files, reason)?
    };
    checkout.publish()?;
    Ok(receipt)
}

pub(super) struct LocalCheckout<'a> {
    planned: bool,
    worktree: &'a Path,
    scratch: tempfile::TempDir,
    index: PathBuf,
    reservation: IndexReservation,
    proof_objects: Option<&'a Path>,
}

impl<'a> LocalCheckout<'a> {
    fn reserve(worktree: &'a Path, proof_objects: Option<&'a Path>) -> Result<Self> {
        ensure_empty(worktree)?;
        let admin = worktree.join(".git");
        let reservation = IndexReservation::hold(&admin)?;
        let scratch = tempfile::Builder::new()
            .prefix(".graft-local-")
            .tempdir_in(&admin)?;
        let index = scratch.path().join("index");
        Ok(Self {
            planned: false,
            worktree,
            scratch,
            index,
            reservation,
            proof_objects,
        })
    }

    pub(super) fn proof_command(&self) -> Command {
        let mut command = self.command();
        if let Some(objects) = self.proof_objects {
            command.env("GIT_OBJECT_DIRECTORY", objects);
        }
        command
    }

    fn command(&self) -> Command {
        let mut command = git_command(self.worktree);
        // Let Git compress common path prefixes in the new private index.
        command.env("GIT_INDEX_FILE", &self.index).args([
            "-c",
            "index.version=4",
            "-c",
            "core.splitIndex=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "core.ignorestat=false",
        ]);
        command
    }

    fn publish(self) -> Result<()> {
        self.reservation.publish(&self.index)
    }

    fn native(
        &mut self,
        oid: &str,
        sparse: &[String],
        files: &[Entry],
        reason: &str,
    ) -> Result<Value> {
        if self.planned {
            self.index = self.scratch.path().join("fallback-index");
        }
        // read-tree verifies untracked paths before using Git's forced
        // checkout. Run it off the live tree so a path created in that gap
        // cannot be overwritten.
        let staged = self.scratch.path().join("tree");
        fs::create_dir(&staged)?;
        // Git resolves relative core.attributesFile paths from GIT_WORK_TREE.
        // Keep the workspace's effective attributes when using the scratch tree.
        let attributes = crate::git::raw(
            self.worktree,
            &["config", "--null", "--path", "--get", "core.attributesFile"],
        )?;
        let relative_attributes = if attributes.status.success() {
            let path = attributes
                .stdout
                .strip_suffix(b"\0")
                .context("invalid Git attributes file path")?;
            let path = Path::new(std::str::from_utf8(path)?);
            (path.is_relative() && !path.as_os_str().is_empty()).then(|| self.worktree.join(path))
        } else {
            ensure!(
                attributes.status.code() == Some(1),
                "read checkout attributes: {}",
                String::from_utf8_lossy(&attributes.stderr).trim()
            );
            None
        };
        let staged_command = || {
            let mut command = self.command();
            command.env("GIT_WORK_TREE", &staged);
            if let Some(path) = &relative_attributes {
                command
                    .env("GRAFT_CHECKOUT_ATTRIBUTES", path)
                    .arg("--config-env=core.attributesFile=GRAFT_CHECKOUT_ATTRIBUTES");
            }
            command
        };
        if !sparse.is_empty() {
            let mut command = staged_command();
            command
                .args(["sparse-checkout", "set", "--cone", "--"])
                .args(sparse);
            run(command)?;
        }
        let mut command = parallel_checkout(staged_command());
        command.args(["read-tree", "-m", "-u", oid]);
        run(command)?;
        let mut receipt = Receipt::default();
        count_native(&staged, files.iter(), &mut receipt.private);
        for entry in fs::read_dir(&staged)? {
            let entry = entry?;
            let destination = self.worktree.join(entry.file_name());
            crate::recovery::rename_exclusive(&entry.path(), &destination)
                .with_context(|| format!("publish checkout path {}", destination.display()))?;
        }
        Ok(receipt.json("git", Some(reason)))
    }
}

pub(super) fn run(mut command: Command) -> Result<()> {
    let output = command.output().context("run Git checkout")?;
    ensure!(
        output.status.success(),
        "Git checkout: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn count_native<'a>(worktree: &Path, files: impl Iterator<Item = &'a Entry>, counts: &mut Counts) {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if let Ok(root) = native::Root::open(worktree) {
        for entry in files {
            if let Ok(size) = entry.path().and_then(|path| Ok(root.regular_size(path)?)) {
                counts.add(size);
            }
        }
        return;
    }
    for entry in files {
        count_private(worktree, entry, counts);
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) mod direct {
    use super::*;
    use std::{collections::HashMap, fs::File, os::unix::fs::MetadataExt};

    #[cfg(target_os = "macos")]
    mod tree;

    pub(super) struct Plan {
        donor: native::Root,
        destination: native::Root,
        candidates: Vec<bool>,
        /// Per entry, when Git's line-ending clean of a donor file stores
        /// exactly the entry's blob (see [`eol`]).
        rules: Vec<eol::Rule>,
        /// Blob sizes by object ID.
        sizes: Option<HashMap<String, u64>>,
        mode: u32,
        /// Symbolic links may be installed as links.
        #[cfg(target_os = "macos")]
        links: bool,
        /// The primary's index stat data, trusted as `git status` trusts it.
        trusted: Option<index_stat::Trusted>,
    }

    pub(super) fn plan(
        checkout: &LocalCheckout<'_>,
        oid: &str,
        sparse: &[String],
        files: &[Entry],
        state: &Path,
        primary: &Path,
        early: super::Early<'_>,
    ) -> Result<std::result::Result<Plan, &'static str>> {
        if !sparse.is_empty() {
            return Ok(Err("sparse checkout"));
        }
        if files.is_empty() {
            return Ok(Err("empty tree"));
        }
        let gitlinks = files.iter().any(|entry| entry.mode == 0o160000);
        if !unambiguous_paths(files) {
            return Ok(Err("filesystem-dependent Git paths"));
        }
        if state.parent() == Some(primary) {
            return Ok(Err("primary checkout unavailable"));
        }
        let Ok(primary) = dunce::canonicalize(primary) else {
            return Ok(Err("primary checkout unavailable"));
        };
        let (Ok(donor), Ok(destination)) = (
            native::Root::open(&primary),
            native::Root::open(checkout.worktree),
        ) else {
            return Ok(Err("native clone unavailable"));
        };
        if !donor.same_device(&destination)
            || !probe(checkout.scratch.path(), checkout.scratch.path())?
        {
            return Ok(Err("native clone unavailable"));
        }

        // These read-only queries use independent Git processes. Join all
        // workers before using their results to select donor files.
        let (tracked, policy, conversions, sizes) = std::thread::scope(|scope| -> Result<_> {
            let config = scope.spawn(|| raw_file_policy(checkout.worktree, gitlinks));
            let sizes = match early.sizes {
                Some(early) => early,
                None => scope.spawn(|| blob_sizes(|| checkout.proof_command(), files)),
            };
            let tracked = match early.tracked {
                Some(early) => early,
                None => scope.spawn(|| donor_index(&primary)),
            };
            // Git also applies info/attributes and user and system rules when
            // the selected tree has no .gitattributes of its own.
            let attributes = scope.spawn(|| {
                let source = if checkout.planned {
                    AttributeSource::Index
                } else {
                    AttributeSource::Commit(oid)
                };
                checkout_conversions(checkout.proof_command(), source, files.iter())
            });
            let tracked = crate::workspace::joined(tracked)?;
            let policy = config
                .join()
                .map_err(|_| anyhow::anyhow!("checkout configuration worker failed"))??;
            let conversions = attributes
                .join()
                .map_err(|_| anyhow::anyhow!("checkout attribute worker failed"))?;
            Ok((
                tracked,
                policy,
                conversions,
                crate::workspace::joined(sizes),
            ))
        })?;
        let config = match policy {
            Ok(value) => value,
            Err(reason) => return Ok(Err(reason)),
        };
        let Some(tracked) = tracked else {
            return Ok(Err("primary checkout unavailable"));
        };
        let tracked = donor_entries(&tracked);
        let mut candidates: Vec<_> = files
            .iter()
            .map(|entry| {
                let mode: &[u8] = if entry.mode == 0o100755 {
                    b"100755"
                } else {
                    b"100644"
                };
                entry.regular()
                    && tracked.get(entry.name.as_slice()) == Some(&(mode, entry.blob.as_bytes()))
            })
            .collect();
        if !candidates.contains(&true) {
            return Ok(Err("no eligible local donor files"));
        }

        let Ok(conversions) = conversions else {
            return Ok(Err("cannot inspect checkout attributes"));
        };
        // Paths that only Git can convert keep Git's own checkout.
        let mut rules = Vec::with_capacity(files.len());
        for (eligible, conversion) in candidates.iter_mut().zip(&conversions) {
            let rule = config.line_endings.rule(conversion);
            *eligible &= rule.is_some();
            rules.push(rule.unwrap_or(eol::Rule::NEVER));
        }
        if !candidates.contains(&true) {
            return Ok(Err("path-dependent checkout conversion"));
        }
        let trusted = sizes.as_ref().and_then(|_| {
            index_stat::Trusted::read(&primary.join(".git/index"), files[0].blob.len() / 2)
        });
        let mode = native::creation_mode(checkout.scratch.path())?;
        Ok(Ok(Plan {
            donor,
            destination,
            candidates,
            rules,
            sizes,
            mode,
            #[cfg(target_os = "macos")]
            links: config.links,
            trusted,
        }))
    }

    /// Each regular file blob's size, or `None` if Git cannot report them all.
    /// Reading a loose object's size inflates its header, so large sets are
    /// split across several Git processes made by `command`.
    pub(super) fn blob_sizes(
        command: impl Fn() -> Command + Sync,
        files: &[Entry],
    ) -> Option<HashMap<String, u64>> {
        let mut seen = std::collections::HashSet::new();
        let blobs: Vec<&str> = files
            .iter()
            .filter(|entry| entry.regular())
            .map(|entry| entry.blob.as_str())
            .filter(|blob| seen.insert(*blob))
            .collect();
        let workers = if blobs.len() >= 1024 { 4 } else { 1 };
        let shards = std::thread::scope(|scope| {
            let handles: Vec<_> = blobs
                .chunks(blobs.len().div_ceil(workers).max(1))
                .map(|shard| scope.spawn(|| shard_sizes(command(), shard)))
                .collect();
            handles
                .into_iter()
                .map(crate::workspace::joined)
                .collect::<Option<Vec<_>>>()
        })?;
        let sizes: HashMap<String, u64> = shards.into_iter().flatten().collect();
        (sizes.len() == blobs.len()).then_some(sizes)
    }

    fn shard_sizes(mut command: Command, blobs: &[&str]) -> Option<Vec<(String, u64)>> {
        let mut input = Vec::with_capacity(blobs.len() * 65);
        for blob in blobs {
            input.extend_from_slice(blob.as_bytes());
            input.push(b'\n');
        }
        let mut stdin = tempfile::tempfile().ok()?;
        std::io::Write::write_all(&mut stdin, &input).ok()?;
        std::io::Seek::rewind(&mut stdin).ok()?;
        command
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .args([
                "cat-file",
                "--batch-check=%(objectname) %(objecttype) %(objectsize)",
                "--buffer",
            ])
            .stdin(Stdio::from(stdin));
        let output = crate::git::bounded_stdout(&mut command, 64 << 20).ok()?;
        let mut sizes = Vec::with_capacity(blobs.len());
        for line in output
            .split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
        {
            let line = std::str::from_utf8(line).ok()?;
            let mut fields = line.split(' ');
            let (Some(oid), Some("blob"), Some(size), None) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return None;
            };
            sizes.push((oid.to_owned(), size.parse().ok()?));
        }
        Some(sizes)
    }

    // Decline aliases before either Git or clonefile creates their names.
    fn unambiguous_paths(files: &[Entry]) -> bool {
        let mut spellings = HashMap::new();
        for entry in files {
            if !entry.name.is_ascii() {
                return false;
            }
            let lowercase = entry.name.to_ascii_lowercase();
            for end in entry
                .name
                .iter()
                .enumerate()
                .filter_map(|(i, &byte)| (byte == b'/').then_some(i))
                .chain([entry.name.len()])
            {
                let original = &entry.name[..end];
                let directory = end < entry.name.len();
                let key = &lowercase[..end];
                match spellings.get(key) {
                    Some(&(previous, true)) if directory && previous == original => (),
                    Some(_) => return false,
                    None => {
                        spellings.insert(key.to_vec(), (original, directory));
                    }
                }
            }
        }
        true
    }

    fn donor_entries(output: &[u8]) -> HashMap<&[u8], (&[u8], &[u8])> {
        output
            .split(|&byte| byte == 0)
            .filter_map(|record| {
                let tab = record.iter().position(|&byte| byte == b'\t')?;
                let header = record[..tab].strip_suffix(b" 0")?;
                let mode = header.get(..6)?;
                let oid = header.get(7..)?;
                Some((&record[tab + 1..], (mode, oid)))
            })
            .collect()
    }

    pub(super) fn donor_index(primary: &Path) -> Result<Option<Vec<u8>>> {
        // The listing is only used once the prefix proves it belongs to this
        // checkout root, so both read-only queries can run at once.
        let (location, output) = std::thread::scope(|scope| {
            let output = scope.spawn(|| donor_listing(primary).output());
            (
                donor_prefix(primary).output(),
                crate::workspace::joined(output),
            )
        });
        let location = location?;
        if !location.status.success() || location.stdout != b"\n" {
            return Ok(None);
        }
        let output = output?;
        Ok(output.status.success().then_some(output.stdout))
    }

    fn donor_prefix(primary: &Path) -> Command {
        // Git can discover a surrounding repository even if the original one
        // moved. Only use the index belonging to the selected checkout root.
        let mut location = git_command(primary);
        location.args(["rev-parse", "--show-prefix"]);
        location
    }

    fn donor_listing(primary: &Path) -> Command {
        let mut listing = git_command(primary);
        listing.args([
            "-c",
            "core.fsmonitor=false",
            "ls-files",
            "--stage",
            "--full-name",
            "-z",
        ]);
        listing
    }

    struct Config {
        /// What Git's line-ending clean depends on in the workspace.
        line_endings: eol::Settings,
        /// Whether Git checks symbolic links out as links (`core.symlinks`).
        /// Only macOS installs links by directory clone.
        #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
        links: bool,
    }

    fn raw_file_policy(worktree: &Path, gitlinks: bool) -> Result<Result<Config, &'static str>> {
        let settings = if gitlinks {
            r"^(core\.(autocrlf|eol|safecrlf|sparsecheckout|symlinks)|submodule\.recurse)$"
        } else {
            r"^core\.(autocrlf|eol|safecrlf|sparsecheckout|symlinks)$"
        };
        // Both reads are independent, so their Git processes run at once.
        let (config, replacements) = std::thread::scope(|scope| {
            let replacements = scope.spawn(|| {
                git(
                    worktree,
                    &[
                        "for-each-ref",
                        "--count=1",
                        "--format=%(refname)",
                        "refs/replace",
                    ],
                )
            });
            let config = git_command(worktree)
                .args([
                    "config",
                    "--null",
                    "--type=bool-or-str",
                    "--get-regexp",
                    settings,
                ])
                .output();
            (config, crate::workspace::joined(replacements))
        });
        let config = config?;
        if !matches!(config.status.code(), Some(0 | 1)) {
            return Ok(Err("Git checkout configuration"));
        }
        let mut auto_crlf = eol::AutoCrlf::False;
        let mut eol_crlf = false;
        let mut safe_crlf = false;
        let mut sparse = false;
        let mut recurse = false;
        // Git creates symbolic links unless core.symlinks is false.
        let mut links = true;
        for field in config.stdout.split(|&b| b == 0).filter(|f| !f.is_empty()) {
            let mut parts = field.splitn(2, |&b| b == b'\n');
            let key = parts.next().unwrap_or_default();
            let value = parts.next().unwrap_or_default();
            let boolean = matches!(value, b"true" | b"false");
            match key {
                b"core.autocrlf" if value.eq_ignore_ascii_case(b"input") => {
                    auto_crlf = eol::AutoCrlf::Input
                }
                b"core.autocrlf" if boolean => {
                    auto_crlf = if value == b"true" {
                        eol::AutoCrlf::True
                    } else {
                        eol::AutoCrlf::False
                    }
                }
                b"core.eol" => eol_crlf = value.eq_ignore_ascii_case(b"crlf"),
                b"core.safecrlf" if value.eq_ignore_ascii_case(b"warn") => safe_crlf = false,
                b"core.safecrlf" if boolean => safe_crlf = value == b"true",
                b"core.sparsecheckout" if boolean => sparse = value == b"true",
                b"submodule.recurse" if boolean => recurse = value == b"true",
                b"core.symlinks" if boolean => links = value == b"true",
                _ => return Ok(Err("Git checkout configuration")),
            }
        }
        if sparse {
            return Ok(Err("Git checkout configuration"));
        }
        if recurse {
            return Ok(Err("submodules"));
        }
        if !replacements?.stdout.is_empty() {
            return Ok(Err("Git replacement objects"));
        }
        Ok(Ok(Config {
            line_endings: eol::Settings {
                auto_crlf,
                eol_crlf,
                safe_crlf,
            },
            links,
        }))
    }

    pub(super) fn install<F: FnOnce() -> Result<()>>(
        checkout: &LocalCheckout<'_>,
        oid: &str,
        files: &[Entry],
        plan: Plan,
        ready: F,
    ) -> Result<Value> {
        let mut command = checkout.proof_command();
        command.env("GIT_NO_REPLACE_OBJECTS", "1").args([
            "-c",
            "core.protectHFS=true",
            "-c",
            "core.protectNTFS=true",
            "read-tree",
            oid,
        ]);
        if !checkout.planned {
            run(command)?;
            // read-tree can create an index even when a referenced blob is missing.
            // Check the object store before a donor working file can mask that loss.
            crate::git::require_blobs(
                checkout.proof_command(),
                files
                    .iter()
                    .filter(|entry| entry.mode != 0o160000)
                    .map(|entry| entry.blob.as_str()),
            )?;
        }

        let mut receipt = Receipt::default();
        let proof = Proof {
            trusted: plan.trusted.as_ref(),
            sizes: plan.sizes.as_ref(),
        };
        #[cfg(target_os = "macos")]
        let reuse_ancestors = native::parents::available();
        #[cfg(not(target_os = "macos"))]
        let reuse_ancestors = false;
        // Leave conversions and submodule directory creation in Git's original order.
        let overlap_links = checkout.planned
            && files.len() >= PARALLEL_THRESHOLD
            && files.iter().any(|entry| entry.mode == 0o120000)
            && files
                .iter()
                .zip(&plan.candidates)
                .all(|(entry, eligible)| *eligible || entry.mode == 0o120000);
        let checkout_paths = |input: File| -> Result<()> {
            let mut command = parallel_checkout(checkout.command());
            command
                .env("GIT_ATTR_SOURCE", oid)
                .env("GIT_NO_REPLACE_OBJECTS", "1")
                .args(["checkout-index", "-u", "--stdin", "-z"])
                .stdin(Stdio::from(input));
            run(command)?;
            Ok(())
        };
        let mut ready = Some(ready);
        // Whole clean directories first; the per-file workers skip their entries.
        #[cfg(target_os = "macos")]
        let directories = tree::clone_directories(
            files,
            &plan.candidates,
            &plan.donor,
            &plan.destination,
            tree::Policy {
                proof,
                rules: &plan.rules,
                links: plan.links,
                mode: plan.mode,
                reuse_ancestors,
            },
        )?;
        #[cfg(target_os = "macos")]
        let covered = directories.covered.as_slice();
        #[cfg(target_os = "linux")]
        let covered = vec![false; files.len()];
        #[cfg(target_os = "linux")]
        let covered = covered.as_slice();
        crate::trace::mark(&format!(
            "checkout: clone directories ({} of {} entries)",
            covered.iter().filter(|covered| **covered).count(),
            files.len()
        ));
        let batches = std::thread::scope(|scope| -> Result<Vec<_>> {
            let mut tasks = Vec::new();
            let mut flags = plan.candidates.as_slice();
            let mut rules = plan.rules.as_slice();
            let mut skipped = covered;
            for chunk in source_chunks(files) {
                let (eligible, remaining) = flags.split_at(chunk.len());
                flags = remaining;
                let (rule, remaining) = rules.split_at(chunk.len());
                rules = remaining;
                let (skip, remaining) = skipped.split_at(chunk.len());
                skipped = remaining;
                let donor = plan.donor.checkout_worker(reuse_ancestors)?;
                let destination = plan.destination.checkout_worker(reuse_ancestors)?;
                let mode = plan.mode;
                #[cfg(target_os = "linux")]
                let scratch = tempfile::tempdir_in(checkout.scratch.path())?;
                tasks.push(scope.spawn(move || {
                    #[cfg(target_os = "linux")]
                    let temporary = native::Root::open(scratch.path())?;
                    clone_batch(
                        chunk,
                        eligible,
                        rule,
                        skip,
                        &donor,
                        &destination,
                        mode,
                        proof,
                        #[cfg(target_os = "linux")]
                        &temporary,
                    )
                }));
            }
            if overlap_links {
                ready.take().expect("history readiness is checked once")()?;
                let links = files
                    .iter()
                    .zip(covered)
                    .filter(|(entry, covered)| entry.mode == 0o120000 && !**covered)
                    .map(|(entry, _)| entry);
                // Git treats EEXIST as a checkout failure if a clone worker
                // creates a missing directory between Git's lookup and mkdir.
                for link in links.clone() {
                    plan.destination.create_parent(link.path()?)?;
                }
                checkout_paths(path_input(links)?)?;
            }
            tasks
                .into_iter()
                .map(|task| {
                    task.join()
                        .map_err(|_| anyhow::anyhow!("local clone worker failed"))?
                })
                .collect()
        })?;
        let mut missing = Vec::new();
        let mut installed = HashMap::new();
        #[cfg(target_os = "macos")]
        let batches = batches.into_iter().chain(std::iter::once((
            directories.counts,
            directories.missing,
            directories.installed,
        )));
        for (cloned, paths, stats) in batches {
            receipt.cloned.merge(cloned);
            missing.extend(paths);
            installed.extend(stats.into_iter().map(|(entry, stat)| {
                let record = index_stat::Installed {
                    blob: entry.blob.clone(),
                    mode: entry.mode,
                    stat,
                };
                (entry.name.clone(), record)
            }));
        }
        if let Some(ready) = ready {
            ready()?;
        } else {
            // HEAD can change after native link checkout while regular files are still cloning.
            checkout_identity(checkout.worktree, oid)?;
        }
        if !missing.is_empty() {
            // checkout-index accepts an existing gitlink directory, including
            // a populated repository. Reserve each one exclusively first.
            for entry in missing.iter().filter(|entry| entry.mode == 0o160000) {
                let path = entry.path()?;
                plan.destination
                    .create_empty_directory(path)
                    .with_context(|| format!("reserve submodule directory {}", path.display()))?;
            }
            let mut remaining = missing
                .iter()
                .copied()
                .filter(|e| !overlap_links || e.mode != 0o120000)
                .peekable();
            if remaining.peek().is_some() {
                checkout_paths(path_input(remaining)?)?;
            }
            count_native(checkout.worktree, missing.into_iter(), &mut receipt.private);
        }
        // Like Git's own checkout, record each cloned file's stat as captured
        // immediately after installation; no later refresh blesses an edit.
        crate::trace::mark("checkout: clone files");
        index_stat::record(&checkout.index, &installed)?;
        receipt.primary.files = receipt.cloned.files;
        receipt.primary.bytes = receipt.cloned.bytes;
        let backend = match (receipt.cloned.files, cfg!(target_os = "macos")) {
            (0, _) => "git",
            (_, true) => "clonefile",
            (_, false) => "reflink",
        };
        Ok(receipt.json(backend, None))
    }

    /// Cloned bytes, entries Git must check out, and cloned entries' stat data.
    type Batch<'a> = (Counts, Vec<&'a Entry>, Vec<(&'a Entry, index_stat::Stat)>);

    /// Clone `files` one by one. Entries flagged in `skip` were already
    /// installed; `rules` say when each donor file holds its blob.
    #[allow(clippy::too_many_arguments)]
    fn clone_batch<'a>(
        files: &'a [Entry],
        candidates: &[bool],
        rules: &[eol::Rule],
        skip: &[bool],
        donor: &native::Root,
        destination: &native::Root,
        mode: u32,
        proof: Proof<'_>,
        #[cfg(target_os = "linux")] scratch: &native::Root,
    ) -> Result<Batch<'a>> {
        let mut counts = Counts::default();
        let mut missing = Vec::new();
        let mut installed = Vec::new();
        let mut buffer = [0; 64 * 1024];
        for (((entry, &eligible), &rule), &done) in
            files.iter().zip(candidates).zip(rules).zip(skip)
        {
            if done {
                continue;
            }
            if !eligible {
                missing.push(entry);
                continue;
            }
            let path = entry.path()?;
            let Ok((mut file, metadata)) = donor.regular_metadata(path) else {
                missing.push(entry);
                continue;
            };
            let mode = entry.creation_mode(mode);
            let same_mode = if cfg!(target_os = "macos") {
                metadata.mode() & 0o7777 == mode
            } else {
                metadata.mode() & 0o100 == mode & 0o100
            };
            // A tracked index entry can still name a staged change, an
            // assume-unchanged file or a checkout produced with old filters.
            // Git's own stat test, unchanged across the clone below, proves
            // a file equal to its blob where Git's clean leaves it unchanged;
            // otherwise hash what Git's clean of it stores, catching
            // same-size edits.
            let proven = native::plain(&file, &metadata).unwrap_or(false)
                && proof.stat_proven(
                    entry,
                    rule,
                    &index_stat::Stat::from_metadata(&metadata),
                    metadata.len(),
                );
            if !same_mode
                || (!proven && !proof.hashed(&mut file, &metadata, entry, rule, &mut buffer))
            {
                missing.push(entry);
                continue;
            }
            destination.create_parent(path)?;
            let stamp = cache::Stamp::from(&metadata);
            if cache::Stamp::from(&file.metadata()?) != stamp {
                missing.push(entry);
                continue;
            }
            #[cfg(target_os = "macos")]
            let cloned = destination.clone_regular(path, &file, &metadata, mode, true);
            #[cfg(target_os = "linux")]
            let cloned = scratch.clone_regular(Path::new("clone"), &file, &metadata, mode, true);
            if let Err(error) = cloned {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    return Err(error).with_context(|| {
                        format!("checkout destination exists: {}", path.display())
                    });
                }
                missing.push(entry);
                continue;
            }
            let stable = cache::Stamp::from(&file.metadata()?) == stamp;
            #[cfg(target_os = "macos")]
            ensure!(
                stable,
                "local donor changed during clone: {}",
                path.display()
            );
            #[cfg(target_os = "linux")]
            {
                if !stable {
                    scratch.discard(Path::new("clone"))?;
                    missing.push(entry);
                    continue;
                }
                destination.install_local_clone(path, scratch)?;
            }
            // Capture the installed inode right away, as Git's checkout does.
            // A file Graft cannot stat keeps empty stat data and is rehashed.
            if let Ok(info) = destination.regular_stat(path) {
                installed.push((entry, index_stat::Stat::from_libc(&info)));
            }
            counts.add(metadata.len());
        }
        Ok((counts, missing, installed))
    }

    /// How a donor file is proven to hold an entry's blob.
    #[derive(Clone, Copy)]
    pub(super) struct Proof<'p> {
        /// The primary's index stat data, trusted as `git status` trusts it.
        pub(super) trusted: Option<&'p index_stat::Trusted>,
        /// Blob sizes by object ID.
        pub(super) sizes: Option<&'p HashMap<String, u64>>,
    }

    impl Proof<'_> {
        fn blob_size(&self, entry: &Entry) -> Option<u64> {
            self.sizes.and_then(|sizes| sizes.get(&entry.blob)).copied()
        }

        /// Whether Git's stat test in the primary vouches that the donor,
        /// with stat data `stat` and `size` bytes, equals the entry's blob,
        /// and Git's clean under `rule` leaves such a file unchanged.
        pub(super) fn stat_proven(
            &self,
            entry: &Entry,
            rule: eol::Rule,
            stat: &index_stat::Stat,
            size: u64,
        ) -> bool {
            rule.identity_is_clean()
                && self.trusted.is_some_and(|trusted| {
                    trusted.holds(
                        &entry.name,
                        &entry.blob,
                        entry.mode,
                        stat,
                        size,
                        self.blob_size(entry),
                    )
                })
        }

        /// Hash `file`: whether it carries no flags or extended attributes
        /// and Git's line-ending clean under `rule` stores exactly the
        /// entry's blob.
        pub(super) fn hashed(
            &self,
            file: &mut File,
            metadata: &fs::Metadata,
            entry: &Entry,
            rule: eol::Rule,
            buffer: &mut [u8],
        ) -> bool {
            let size = metadata.len();
            native::plain(file, metadata).unwrap_or(false)
                && eol::cleans_to(
                    file,
                    size,
                    self.blob_size(entry).unwrap_or(size),
                    &entry.blob,
                    rule,
                    buffer,
                )
                .unwrap_or(false)
        }
    }

    /// Whether a symbolic link's target bytes are exactly the Git blob `oid`.
    #[cfg(target_os = "macos")]
    fn link_matches(target: &[u8], oid: &str) -> bool {
        let mut buffer = [0; 4096];
        let size = target.len() as u64;
        eol::cleans_to(target, size, size, oid, eol::Rule::BINARY, &mut buffer).unwrap_or(false)
    }
}
