//! Consent for hooks that come from a file the repository carries. A hook in
//! `.graftconfig`, or in `.graftconfig.local` once Git tracks it, arrives with
//! a clone, a checked-out branch or a merged graft, so it runs only after the
//! person has run `git graft hook trust`. That command records a digest of
//! every hook those files define, by event, name and program with arguments,
//! in the project's Graft state. A command that would run one of them refuses
//! with `hooks_untrusted` until the digest matches, and again after any
//! change. What an untracked `.graftconfig.local` adds, replaces or turns off
//! is the person's own and needs no trust. No MCP tool records trust, so an
//! agent cannot approve its own hook.

use crate::{
    api,
    config::{Config, Hook, LOCAL, SHARED},
    registry::{self, Project},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, io, path::Path};

use super::{Event, Policy};

/// The file in the project's Graft state that records the trusted hooks.
const FILE: &str = "trusted-hooks.json";
/// The command that records trust, as the refusal names it.
pub(crate) const COMMAND: &str = "git graft hook trust";

/// What `git graft hook trust` recorded.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    sha256: String,
    hooks: Vec<Value>,
}

/// Which of the project's configuration files have hooks that run only once
/// trusted.
pub(crate) struct Gate {
    local: bool,
}

impl Gate {
    /// `.graftconfig` always does, and `.graftconfig.local` when Git tracks
    /// it in the primary. Git is asked only when the local file defines a hook.
    pub(crate) fn of(primary: &Path, config: &Config) -> Result<Self> {
        let local =
            config.defined.iter().any(|hook| hook.file == LOCAL) && tracked(primary, LOCAL)?;
        Ok(Self { local })
    }

    /// Whether a hook from `file` runs only once trusted.
    pub(crate) fn needs_trust(&self, file: &str) -> bool {
        file == SHARED || self.local && file == LOCAL
    }

    /// The hooks that run only once trusted, as the files define them.
    pub(crate) fn hooks<'a>(&self, config: &'a Config) -> Vec<&'a Hook> {
        config
            .defined
            .iter()
            .filter(|hook| self.needs_trust(hook.file))
            .collect()
    }
}

/// Whether the index of `primary` holds `name`, at the root of the checkout.
fn tracked(primary: &Path, name: &str) -> Result<bool> {
    let pathspec = format!(":(top,literal){name}");
    let listed = crate::git::bytes(primary, &["ls-files", "--cached", "-z", "--", &pathspec])
        .with_context(|| format!("check whether Git tracks {name}"))?;
    Ok(!listed.is_empty())
}

/// The digest of `hooks`: their events, names and arguments, in order.
fn digest(hooks: &[&Hook]) -> String {
    let parts: Vec<(&str, &str, &[String])> = hooks
        .iter()
        .map(|hook| (hook.event.name(), hook.name.as_str(), hook.argv.as_slice()))
        .collect();
    let bytes = serde_json::to_vec(&parts).expect("hook descriptions serialize");
    Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The record in the Graft state directory `state`, if trust was recorded.
fn recorded(state: &Path) -> Result<Option<Record>> {
    let path = state.join(FILE);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        Ok(_) => {}
    }
    let bytes = registry::regular(&path)?;
    let record = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is not a record of trusted hooks", path.display()))?;
    Ok(Some(record))
}

/// Whether the hooks that need trust, `hooks`, are the ones the record in
/// `state` trusts. With none, there is nothing to trust. `state` need not exist.
pub(crate) fn is_trusted(state: &Path, hooks: &[&Hook]) -> Result<bool> {
    if hooks.is_empty() {
        return Ok(true);
    }
    Ok(recorded(state)?.is_some_and(|record| record.sha256 == digest(hooks)))
}

/// Refuse with `hooks_untrusted` when a caller that runs hooks would run one
/// for `event` that needs trust, and the hooks that need trust are not the
/// ones last trusted. Nothing has run or changed when this refuses.
pub(crate) fn require(p: &Project, config: &Config, event: Event, policy: Policy) -> Result<()> {
    if policy == Policy::Skip {
        return Ok(());
    }
    let due = config.hooks_for(event);
    if due.is_empty() {
        return Ok(());
    }
    let gate = Gate::of(&p.repo, config)?;
    if !due.iter().any(|hook| gate.needs_trust(hook.file)) {
        return Ok(());
    }
    let gated = gate.hooks(config);
    let record = recorded(&p.state)?;
    if record
        .as_ref()
        .is_some_and(|record| record.sha256 == digest(&gated))
    {
        return Ok(());
    }
    let mut files: Vec<&str> = gated.iter().map(|hook| hook.file).collect();
    files.dedup();
    let state = if record.is_some() {
        "have changed since they were last trusted"
    } else {
        "have not been trusted"
    };
    Err(api::fault(
        "hooks_untrusted",
        format!(
            "The hooks in {} {state}, so Graft ran nothing and changed nothing. They come with the repository and run with your permissions. \
             Read them in details.hooks or with `git graft hook list`, then run `{COMMAND}` in a terminal to let them run. No MCP tool can trust hooks",
            files.join(" and ")
        ),
        json!({
            "event": event,
            "hooks": gated.iter().map(|hook| hook.describe()).collect::<Vec<_>>(),
            "trusted": record.map(|record| record.hooks),
            "command": COMMAND,
        }),
    ))
}

/// Record the hooks that need trust, as the files define them now, as
/// trusted. Returns them with their digest.
pub(crate) fn trust(p: &Project) -> Result<Value> {
    let config = Config::read(&p.repo)?;
    let gated = Gate::of(&p.repo, &config)?.hooks(&config);
    let hooks: Vec<Value> = gated.iter().map(|hook| hook.describe()).collect();
    let record = Record {
        sha256: digest(&gated),
        hooks: hooks.clone(),
    };
    registry::atomic(&p.state.join(FILE), &serde_json::to_vec_pretty(&record)?)?;
    Ok(json!({"trusted": hooks, "sha256": record.sha256}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(name: &str, event: Event, argv: &[&str], file: &'static str) -> Hook {
        Hook {
            name: name.into(),
            event,
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
            file,
        }
    }

    #[test]
    fn the_digest_covers_event_name_arguments_and_order_but_not_the_file() {
        let a = hook("db", Event::PostAdd, &["create-db"], SHARED);
        let b = hook("clean", Event::PreArchive, &["drop-db"], SHARED);
        let base = digest(&[&a, &b]);
        assert_eq!(base.len(), 64);
        assert_eq!(
            base,
            digest(&[&a, &hook("clean", Event::PreArchive, &["drop-db"], LOCAL)])
        );
        for changed in [
            digest(&[&b, &a]),
            digest(&[&a]),
            digest(&[&hook("db", Event::PostRestore, &["create-db"], SHARED), &b]),
            digest(&[&hook("db2", Event::PostAdd, &["create-db"], SHARED), &b]),
            digest(&[
                &hook("db", Event::PostAdd, &["create-db", "--x"], SHARED),
                &b,
            ]),
            // Arguments are not joined, so moving a boundary changes the digest.
            digest(&[&hook("db", Event::PostAdd, &["create", "-db"], SHARED), &b]),
        ] {
            assert_ne!(changed, base);
        }
    }

    #[test]
    fn shared_hooks_and_those_of_a_tracked_local_file_need_trust_as_written() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let primary = tmp.path();
        crate::git::bytes(primary, &["init", "-q", "--template="])?;
        fs::write(
            primary.join(SHARED),
            r#"{"hooks": {"db": {"event": "post-add", "argv": ["create-db"]},
                          "notify": {"event": "post-add", "argv": ["notify"]},
                          "drop": {"event": "pre-archive", "argv": ["drop-db"]}}}"#,
        )?;
        let shared_only = Config::read(primary)?;
        fs::write(
            primary.join(LOCAL),
            r#"{"hooks": {"notify": {"event": "post-add", "argv": ["notify", "--quiet"]},
                          "drop": null,
                          "mine": {"event": "pre-archive", "argv": ["mine"]}}}"#,
        )?;
        let config = Config::read(primary)?;
        let names = |hooks: Vec<&Hook>| {
            hooks
                .iter()
                .map(|hook| format!("{}:{}", hook.file, hook.name))
                .collect::<Vec<_>>()
        };
        // What an untracked local file replaces or turns off leaves the
        // shared definitions, and so their trust, as they were.
        let untracked = Gate::of(primary, &config)?;
        assert!(!untracked.needs_trust(LOCAL));
        assert_eq!(
            names(untracked.hooks(&config)),
            [
                ".graftconfig:db",
                ".graftconfig:notify",
                ".graftconfig:drop"
            ]
        );
        assert_eq!(
            digest(&untracked.hooks(&config)),
            digest(&Gate::of(primary, &shared_only)?.hooks(&shared_only))
        );
        crate::git::bytes(primary, &["add", "--", LOCAL])?;
        let tracked = Gate::of(primary, &config)?;
        assert!(tracked.needs_trust(LOCAL));
        assert_eq!(
            names(tracked.hooks(&config)),
            [
                ".graftconfig:db",
                ".graftconfig:notify",
                ".graftconfig:drop",
                ".graftconfig.local:notify",
                ".graftconfig.local:mine"
            ]
        );

        let state = primary.join("state");
        assert!(is_trusted(&state, &[])?);
        let gated = tracked.hooks(&config);
        assert!(!is_trusted(&state, &gated)?);
        fs::create_dir(&state)?;
        let record = Record {
            sha256: digest(&gated),
            hooks: Vec::new(),
        };
        fs::write(state.join(FILE), serde_json::to_vec(&record)?)?;
        assert!(is_trusted(&state, &gated)?);
        assert!(!is_trusted(&state, &gated[..1])?);
        fs::write(state.join(FILE), "{")?;
        assert!(is_trusted(&state, &gated).is_err());
        Ok(())
    }
}
