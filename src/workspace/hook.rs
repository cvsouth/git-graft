//! Commands the project's owner registers to run when a graft is added, is
//! about to be archived, or is restored. Only the command line registers
//! one, so an agent connected over MCP cannot grant itself execution.

use super::registry::{self as grafts, Record, Status};
use crate::{
    api, cache,
    registry::{self, Held, Project},
    runtime::Runtime,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs::File, path::Path, process::Command};

const REGISTRY_FILE: &str = "hooks-v1.json";
const MAX_BYTES: usize = 1024 * 1024;
const MAX_ARGUMENTS: usize = 64;
const MAX_ARGUMENT_BYTES: usize = 4096;

/// A point in a graft's life at which a registered command runs.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "kebab-case")]
pub enum Event {
    /// After `add` has created a graft.
    PostAdd,
    /// Before `remove`, `merge` or `prune --apply` archives a graft.
    PreArchive,
    /// After `restore` has reopened an archived graft.
    PostRestore,
}

impl Event {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::PostAdd => "post-add",
            Self::PreArchive => "pre-archive",
            Self::PostRestore => "post-restore",
        }
    }
}

/// Whether the caller may run hooks. The command line always may. An MCP
/// host may only when it was started with `--allow-exec`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Run,
    Skip,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hooks {
    version: u8,
    project: String,
    hooks: BTreeMap<Event, Vec<String>>,
}

/// A hook is a program and its arguments, started without a shell.
fn validate(argv: &[String]) -> Result<()> {
    ensure!(
        !argv.is_empty() && argv.len() <= MAX_ARGUMENTS,
        "a hook is a program and up to {} arguments",
        MAX_ARGUMENTS - 1
    );
    ensure!(!argv[0].is_empty(), "the hook program must not be empty");
    ensure!(
        argv.iter()
            .all(|arg| arg.len() <= MAX_ARGUMENT_BYTES && !arg.contains('\0')),
        "each hook argument must be at most {MAX_ARGUMENT_BYTES} bytes and contain no NUL character"
    );
    Ok(())
}

impl Hooks {
    fn read(project: &Project) -> Result<(Self, Option<Vec<u8>>)> {
        project.require_private_state()?;
        let path = project.state.join(REGISTRY_FILE);
        if !registry::exists(&path)? {
            return Ok((
                Self {
                    version: 1,
                    project: project.id().into(),
                    hooks: BTreeMap::new(),
                },
                None,
            ));
        }
        let bytes = cache::read_private_file(&path, MAX_BYTES as u64)?;
        let state: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("read hook registry {}", path.display()))?;
        ensure!(
            state.version == 1 && state.project == project.id(),
            "invalid project hook registry {}",
            path.display()
        );
        for argv in state.hooks.values() {
            validate(argv).with_context(|| format!("read hook registry {}", path.display()))?;
        }
        Ok((state, Some(bytes)))
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(self)?;
        ensure!(bytes.len() <= MAX_BYTES, "project hook registry is full");
        Ok(bytes)
    }

    /// The project lock serializes Graft writers. Recheck the source before
    /// replacing it, so a file changed by anything else is never overwritten.
    fn publish(project: &Project, previous: Option<Vec<u8>>, bytes: &[u8]) -> Result<()> {
        let path = project.state.join(REGISTRY_FILE);
        if let Some(previous) = previous {
            ensure!(
                cache::read_private_file(&path, MAX_BYTES as u64)? == previous,
                "hook registry changed"
            );
            if previous == bytes {
                return Ok(());
            }
        } else {
            ensure!(
                !registry::exists(&path)?,
                "hook registry appeared during registration"
            );
        }
        registry::atomic(&path, bytes)
    }
}

pub(crate) fn list(project: &Project) -> Result<Value> {
    let (state, _) = Hooks::read(project)?;
    let hooks: Vec<_> = state
        .hooks
        .iter()
        .map(|(event, argv)| json!({"event": event, "argv": argv}))
        .collect();
    Ok(json!({ "hooks": hooks }))
}

/// Register `args.argv` for its event, replacing the hook already there.
pub(crate) fn set(project: &Project, args: &api::SetHook) -> Result<Value> {
    validate(&args.argv)?;
    let (mut state, previous) = Hooks::read(project)?;
    let replaced = state.hooks.insert(args.event, args.argv.clone()).is_some();
    Hooks::publish(project, previous, &state.encode()?)?;
    Ok(json!({"event": args.event, "argv": args.argv, "registered": true, "replaced": replaced}))
}

/// Unregister the hook for `event`. Succeeds when none was registered.
pub(crate) fn remove(project: &Project, event: Event) -> Result<Value> {
    let (mut state, previous) = Hooks::read(project)?;
    let removed = state.hooks.remove(&event).is_some();
    if removed {
        Hooks::publish(project, previous, &state.encode()?)?;
    }
    Ok(unregistered(event, removed))
}

/// The result of `hook remove`: whether a hook was there to unregister.
pub(crate) fn unregistered(event: Event, removed: bool) -> Value {
    let message = if removed {
        format!("Unregistered the {} hook", event.name())
    } else {
        format!("No {} hook was registered", event.name())
    };
    json!({"event": event, "registered": false, "removed": removed, "message": message})
}

/// The program and arguments registered for `event`, if any. `add` and
/// `restore` read it before they change anything, so an unreadable hook
/// registry refuses them instead of leaving a graft whose hook never ran.
pub(crate) fn registered(project: &Project, event: Event) -> Result<Option<Vec<String>>> {
    Ok(Hooks::read(project)?.0.hooks.remove(&event))
}

/// The graft a hook runs in, as the hook's environment describes it.
struct Graft<'a> {
    name: &'a str,
    path: &'a Path,
    primary: &'a Path,
    branch: Option<&'a str>,
    base: Option<&'a str>,
    port: u16,
}

/// The process for a hook: the program started directly in the graft, with
/// the caller's environment and the graft's description. Like `exec`, it
/// drops the lease and the variables that point Git at another repository.
fn command(event: Event, argv: &[String], graft: &Graft<'_>) -> Result<Command> {
    let (program, args) = argv.split_first().context("the hook has no program")?;
    let mut command = Command::new(program);
    crate::git::clear_location(&mut command);
    command
        .args(args)
        .current_dir(graft.path)
        .env_remove("GIT_GRAFT_LEASE")
        .env("GIT_GRAFT_HOOK", event.name())
        .env("GIT_GRAFT_NAME", graft.name)
        .env("GIT_GRAFT_PATH", graft.path)
        .env("GIT_GRAFT_PRIMARY", graft.primary)
        .env("GIT_GRAFT_BRANCH", graft.branch.unwrap_or_default())
        .env("GIT_GRAFT_BASE", graft.base.unwrap_or_default())
        .env("GIT_GRAFT_PORT", graft.port.to_string());
    Ok(command)
}

/// The entry a command's `hooks` array carries for one hook.
fn entry(event: Event, status: &str, output: &Value) -> Value {
    json!({
        "event": event,
        "status": status,
        "exit_code": output["exit_code"],
        "stdout": output["stdout"].as_str().unwrap_or_default(),
        "stderr": output["stderr"].as_str().unwrap_or_default(),
        "stdout_truncated": output["stdout_truncated"] == true,
        "stderr_truncated": output["stderr_truncated"] == true,
    })
}

/// Run the hook `argv` for `event` in `graft` and wait for it, without a
/// time limit. A hook that cannot be started is a failed hook, with the
/// cause as its `reason`, so the caller decides what a failure means.
fn run(event: Event, argv: &[String], graft: &Graft<'_>) -> Value {
    let started =
        command(event, argv, graft).and_then(|command| Runtime::capture_command(command, &argv[0]));
    match started {
        Ok(output) => {
            let status = if output["success"] == true {
                "ok"
            } else {
                "failed"
            };
            entry(event, status, &output)
        }
        Err(error) => {
            let mut failed = entry(event, "failed", &Value::Null);
            failed["reason"] = json!(format!("The hook could not be started: {error:#}"));
            failed
        }
    }
}

/// The entry for a hook that an MCP host without `--allow-exec` did not run.
fn skipped(event: Event) -> Value {
    let mut skipped = entry(event, "skipped", &Value::Null);
    skipped["reason"] =
        json!("The MCP host was not started with --allow-exec, so the hook was not run");
    skipped
}

/// The project locks could not be taken again after a hook ran. The command
/// no longer holds what protects the project's records, so it must stop.
#[derive(Debug)]
pub(crate) struct Unlocked;

impl std::fmt::Display for Unlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("could not lock the project again after a hook")
    }
}

/// Runs a command's hooks and collects their reports. It holds the project
/// lock and the build lock the command works under, and releases both while
/// a hook runs, so a slow hook does not stall other grafts. The command
/// keeps the graft's own lock throughout, which keeps every other command
/// off the graft and its record meanwhile.
pub(crate) struct Runner<'a> {
    policy: Policy,
    project: &'a File,
    builds: Option<Held>,
    entries: Vec<Value>,
}

impl<'a> Runner<'a> {
    /// `project` is the held project lock and `builds` the held build lock,
    /// if the command has one.
    pub(crate) fn new(policy: Policy, project: &'a File, builds: Option<Held>) -> Self {
        Self {
            policy,
            project,
            builds,
            entries: Vec::new(),
        }
    }

    /// Add the collected reports to a command's result as `hooks`. A result
    /// is unchanged when no hook is registered for the command's event.
    pub(crate) fn report(self, mut value: Value) -> Value {
        if !self.entries.is_empty() {
            value["hooks"] = json!(self.entries);
        }
        value
    }

    /// The report of the hook `argv` for `event` in the graft `r`: skipped
    /// when the caller may not run hooks, otherwise run with the project
    /// locks released. They stay released; see [`Self::resume`].
    fn run(&mut self, p: &Project, event: Event, argv: &[String], r: &Record) -> Result<Value> {
        if self.policy == Policy::Skip {
            return Ok(skipped(event));
        }
        self.builds = None;
        self.project.unlock().context("release the project lock")?;
        let graft = Graft {
            name: &r.name,
            path: &r.path,
            primary: &p.repo,
            branch: r.branch.as_deref(),
            base: r.base.as_deref(),
            port: r.port,
        };
        Ok(run(event, argv, &graft))
    }

    /// Take the project locks again after the hook reported by `entry`, in
    /// the order [`Project::lock_builds`] gives. Returns whether the hook
    /// ran, in which case the caller reads its records again. A failure is
    /// an [`Unlocked`] error.
    fn resume(&mut self, p: &Project, entry: &Value) -> Result<bool> {
        let ran = entry["status"] != "skipped";
        if ran {
            self.project
                .lock()
                .map_err(anyhow::Error::from)
                .and_then(|()| p.lock_builds(false))
                .map(|builds| self.builds = Some(builds))
                .context(Unlocked)?;
        }
        Ok(ran)
    }

    /// Run the hook `argv` for `event` once `add` or `restore` has made the
    /// graft `r` usable. A failure is reported and nothing else: the graft
    /// stays. The project locks are not taken again, so the caller must
    /// have finished its work under them.
    pub(crate) fn after(
        &mut self,
        p: &Project,
        event: Event,
        argv: &[String],
        r: &Record,
    ) -> Result<()> {
        let entry = self.run(p, event, argv, r)?;
        self.entries.push(entry);
        Ok(())
    }

    /// Run the `pre-archive` hook, if one is registered, before `remove` or
    /// `merge` inspects the active graft `r` for work it must not archive.
    /// The caller has changed nothing yet. A hook that fails refuses the
    /// command with `hook_failed`. After a hook ran, `r` is read and checked
    /// again, since the project lock was released meanwhile.
    pub(crate) fn pre_archive(&mut self, p: &Project, r: &mut Record) -> Result<()> {
        let Some(argv) = registered(p, Event::PreArchive)? else {
            return Ok(());
        };
        let entry = self.run(p, Event::PreArchive, &argv, r)?;
        if entry["status"] == "failed" {
            return Err(hook_failed(r, entry));
        }
        if self.resume(p, &entry)? {
            *r = grafts::authenticate(p, &r.name, &r.lease)?;
            ensure!(
                r.status == Status::Active && r.retiring_to().is_none(),
                "graft changed while its pre-archive hook ran"
            );
            grafts::preflight_retire(p, r)?;
        }
        self.entries.push(entry);
        Ok(())
    }

    /// Run the `pre-archive` hook, if one is registered, for the graft `r`
    /// that `prune --apply` is about to archive, and return its report.
    /// Prune goes on to other grafts whatever the hook did, so the project
    /// locks are held again when this returns.
    pub(crate) fn before_prune(&mut self, p: &Project, r: &Record) -> Result<Option<Value>> {
        let Some(argv) = registered(p, Event::PreArchive)? else {
            return Ok(None);
        };
        let entry = self.run(p, Event::PreArchive, &argv, r)?;
        self.resume(p, &entry)?;
        Ok(Some(entry))
    }
}

/// The refusal of a command whose `pre-archive` hook failed: `entry` is the
/// hook's report, with its exit code and output.
fn hook_failed(r: &Record, entry: Value) -> anyhow::Error {
    let cause = match (entry["reason"].as_str(), entry["exit_code"].as_i64()) {
        (Some(reason), _) => reason.to_owned(),
        (None, Some(code)) => format!("The pre-archive hook exited with code {code}"),
        (None, None) => "The pre-archive hook was ended by a signal".to_owned(),
    };
    api::fault(
        "hook_failed",
        format!(
            "{cause}, so graft {} was not archived. Graft changed nothing itself. Fix what the hook reports and retry; the hook will run again",
            r.name
        ),
        json!({"name": r.name, "hooks": [entry]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    fn project(root: &Path) -> Result<(Project, std::fs::File)> {
        crate::git::bytes(root, &["init", "-q", "--template="])?;
        Project::open(root)
    }

    fn hook(event: Event, argv: &[&str]) -> api::SetHook {
        api::SetHook {
            event,
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    #[test]
    fn event_names_match_on_the_command_line_and_in_storage() {
        for event in Event::value_variants() {
            let name = event.name();
            assert_eq!(event.to_possible_value().unwrap().get_name(), name);
            assert_eq!(serde_json::to_value(event).unwrap(), json!(name));
            assert_eq!(Event::from_str(name, false).unwrap(), *event);
        }
        for unknown in ["", "post_add", "Post-Add", "pre-remove", "post-merge"] {
            assert!(Event::from_str(unknown, false).is_err(), "{unknown}");
            assert!(serde_json::from_value::<Event>(json!(unknown)).is_err());
        }
    }

    #[test]
    fn a_hook_is_a_bounded_program_and_arguments() {
        let argv = |parts: &[&str]| -> Vec<String> {
            parts.iter().map(|part| (*part).to_owned()).collect()
        };
        for accepted in [
            argv(&["/usr/local/bin/prepare"]),
            argv(&[
                "node",
                "script with spaces.js",
                "",
                "--flag=a b",
                "line\nbreak",
            ]),
            vec!["x".repeat(MAX_ARGUMENT_BYTES); MAX_ARGUMENTS],
        ] {
            assert!(validate(&accepted).is_ok(), "{accepted:?}");
        }
        for refused in [
            argv(&[]),
            argv(&[""]),
            argv(&["", "argument"]),
            argv(&["program", "nul\0byte"]),
            argv(&["program", &"x".repeat(MAX_ARGUMENT_BYTES + 1)]),
            vec!["x".to_owned(); MAX_ARGUMENTS + 1],
        ] {
            assert!(validate(&refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn registration_stores_one_hook_per_event() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (project, _lock) = project(tmp.path())?;
        assert_eq!(list(&project)?, json!({"hooks": []}));
        assert!(!project.state.join(REGISTRY_FILE).exists());
        assert_eq!(registered(&project, Event::PostAdd)?, None);

        let first = set(&project, &hook(Event::PostAdd, &["prepare", "--first"]))?;
        assert_eq!(first["registered"], true);
        assert_eq!(first["replaced"], false);
        set(&project, &hook(Event::PreArchive, &["clean up"]))?;
        let second = set(&project, &hook(Event::PostAdd, &["prepare", "", "second"]))?;
        assert_eq!(second["replaced"], true);
        assert_eq!(
            list(&project)?,
            json!({"hooks": [
                {"event": "post-add", "argv": ["prepare", "", "second"]},
                {"event": "pre-archive", "argv": ["clean up"]},
            ]})
        );
        assert_eq!(
            registered(&project, Event::PostAdd)?,
            Some(vec![
                "prepare".to_owned(),
                String::new(),
                "second".to_owned()
            ])
        );
        assert_eq!(registered(&project, Event::PostRestore)?, None);
        let stored: Value =
            serde_json::from_slice(&std::fs::read(project.state.join(REGISTRY_FILE))?)?;
        assert_eq!(stored["version"], 1);
        assert_eq!(stored["project"], project.id());

        assert!(set(&project, &hook(Event::PostRestore, &[])).is_err());
        assert!(set(&project, &hook(Event::PostRestore, &[""])).is_err());
        assert_eq!(registered(&project, Event::PostRestore)?, None);

        let removed = remove(&project, Event::PostAdd)?;
        assert_eq!(removed["removed"], true);
        assert_eq!(removed["registered"], false);
        let absent = remove(&project, Event::PostAdd)?;
        assert_eq!(absent["removed"], false);
        assert_eq!(absent["message"], "No post-add hook was registered");
        assert_eq!(
            list(&project)?,
            json!({"hooks": [{"event": "pre-archive", "argv": ["clean up"]}]})
        );
        Ok(())
    }

    #[test]
    fn foreign_or_invalid_registry_is_refused_and_kept() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (project, _lock) = project(tmp.path())?;
        let path = project.state.join(REGISTRY_FILE);
        let own = |hooks: Value| {
            serde_json::to_vec(&json!({"version": 1, "project": project.id(), "hooks": hooks}))
        };
        for stored in [
            br#"{"version":1,"project":"other-project","hooks":{}}"#.to_vec(),
            own(json!({"post-merge": ["program"]}))?,
            own(json!({"post-add": []}))?,
            own(json!({"post-add": [""]}))?,
            own(json!({"post-add": "program"}))?,
        ] {
            registry::atomic(&path, &stored)?;
            assert!(list(&project).is_err());
            assert!(registered(&project, Event::PostAdd).is_err());
            assert!(set(&project, &hook(Event::PostAdd, &["program"])).is_err());
            assert!(remove(&project, Event::PostAdd).is_err());
            assert_eq!(std::fs::read(&path)?, stored);
        }
        Ok(())
    }

    #[test]
    fn a_hook_starts_in_the_graft_without_the_lease_or_another_repository() -> Result<()> {
        let graft = Graft {
            name: "fix-parser",
            path: Path::new("grafts/fix-parser"),
            primary: Path::new("primary"),
            branch: None,
            base: Some("0123456789abcdef"),
            port: 30123,
        };
        let argv = ["program".to_owned(), "an argument".to_owned()];
        let command = command(Event::PreArchive, &argv, &graft)?;
        assert_eq!(command.get_program(), "program");
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["an argument"]);
        assert_eq!(command.get_current_dir(), Some(graft.path));
        let envs: std::collections::HashMap<_, _> = command.get_envs().collect();
        let env = |name: &str| envs.get(std::ffi::OsStr::new(name)).copied();
        for removed in [
            "GIT_GRAFT_LEASE",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
        ] {
            assert_eq!(env(removed), Some(None), "{removed}");
        }
        for (name, value) in [
            ("GIT_GRAFT_HOOK", "pre-archive"),
            ("GIT_GRAFT_NAME", "fix-parser"),
            ("GIT_GRAFT_PATH", "grafts/fix-parser"),
            ("GIT_GRAFT_PRIMARY", "primary"),
            ("GIT_GRAFT_BRANCH", ""),
            ("GIT_GRAFT_BASE", "0123456789abcdef"),
            ("GIT_GRAFT_PORT", "30123"),
        ] {
            assert_eq!(env(name), Some(Some(std::ffi::OsStr::new(value))), "{name}");
        }
        Ok(())
    }

    #[test]
    fn a_hook_that_cannot_start_is_a_failed_hook() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let graft = Graft {
            name: "missing",
            path: tmp.path(),
            primary: tmp.path(),
            branch: Some("graft/missing"),
            base: None,
            port: 30000,
        };
        let program = tmp.path().join("no-such-program");
        let argv = [program.to_string_lossy().into_owned()];
        let failed = run(Event::PostAdd, &argv, &graft);
        assert_eq!(failed["event"], "post-add");
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["exit_code"], Value::Null);
        assert_eq!(failed["stdout"], "");
        assert!(
            failed["reason"]
                .as_str()
                .is_some_and(|reason| reason.starts_with("The hook could not be started")),
            "{failed}"
        );
        let skipped = skipped(Event::PostRestore);
        assert_eq!(skipped["status"], "skipped");
        assert!(
            skipped["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("--allow-exec")),
            "{skipped}"
        );
        Ok(())
    }
}
