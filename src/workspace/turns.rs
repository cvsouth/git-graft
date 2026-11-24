//! Turns on a named shared resource: something a project has once and cannot
//! copy per graft, such as a development server on a fixed port. Each
//! resource has one holder and a first-come queue, kept in the project's
//! private state. Graft does nothing to the resource itself: like a lease, a
//! turn is a promise between cooperating clients. Callers hold the project
//! lock while reading or changing turn state.
use std::{collections::HashSet, fs, path::PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::registry::{self, Record};
use crate::{
    api, cache,
    registry::{Project, atomic, dir, exists, now, owned_directory},
};

const DIRECTORY: &str = "turns";
/// A resource may be named like a Windows device, such as `com3`, which
/// cannot be the stem of a file name there. The prefix keeps every stem legal.
const PREFIX: &str = "turn-";
const SUFFIX: &str = ".json";
const MAX_BYTES: usize = 1024 * 1024;
/// A queued graft keeps its place while it asks again within this long.
const QUEUE_SECS: u64 = 300;

pub(super) fn validate_resource(resource: &str) -> Result<()> {
    if resource.is_empty()
        || resource.len() > 64
        || resource.starts_with('-')
        || !resource
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    {
        return Err(api::fault(
            "invalid_resource",
            "Use a resource name of 1 to 64 lowercase letters, numbers or hyphens, starting with a letter or number",
            Value::Null,
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Holder {
    id: String,
    name: String,
    /// When the turn was granted.
    since: u64,
    /// When the holder last took the turn again.
    renewed: u64,
    ttl: u64,
}

impl Holder {
    fn expires_at(&self) -> u64 {
        self.renewed.saturating_add(self.ttl)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Waiter {
    id: String,
    name: String,
    joined: u64,
    /// When the graft last asked for the turn.
    asked: u64,
}

/// Who holds the turn on one resource and who waits for it, first come first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Turn {
    holder: Option<Holder>,
    queue: Vec<Waiter>,
}

/// The graft that asks.
#[derive(Clone, Copy)]
struct Graft<'a> {
    id: &'a str,
    name: &'a str,
}

// The rules. They read no clock and no file: `now` and the IDs of the grafts
// that are still active come from the caller.
impl Turn {
    /// Forget a holder whose graft is no longer active or whose turn ran out,
    /// and every queued graft that is no longer active or stopped asking.
    fn settle(&mut self, now: u64, active: &HashSet<String>) {
        if self
            .holder
            .as_ref()
            .is_some_and(|holder| !active.contains(&holder.id) || now >= holder.expires_at())
        {
            self.holder = None;
        }
        self.queue.retain(|waiter| {
            active.contains(&waiter.id) && now <= waiter.asked.saturating_add(QUEUE_SECS)
        });
    }

    /// Ask for the turn, and report whether `graft` now holds it. A holder
    /// renews its turn. Anyone else joins the queue, or keeps its place in
    /// it, and gets the turn once nobody holds it and nobody is ahead.
    fn take(
        &mut self,
        now: u64,
        graft: Graft<'_>,
        ttl: Option<u64>,
        active: &HashSet<String>,
    ) -> bool {
        self.settle(now, active);
        if let Some(holder) = self.holder.as_mut()
            && holder.id == graft.id
        {
            holder.renewed = now;
            holder.ttl = ttl.unwrap_or(holder.ttl);
            return true;
        }
        match self.queue.iter_mut().find(|waiter| waiter.id == graft.id) {
            Some(waiter) => waiter.asked = now,
            None => self.queue.push(Waiter {
                id: graft.id.into(),
                name: graft.name.into(),
                joined: now,
                asked: now,
            }),
        }
        if self.holder.is_some() || self.queue[0].id != graft.id {
            return false;
        }
        self.queue.remove(0);
        self.holder = Some(Holder {
            id: graft.id.into(),
            name: graft.name.into(),
            since: now,
            renewed: now,
            ttl: ttl.unwrap_or(api::TURN_TTL_DEFAULT),
        });
        true
    }

    /// Give up the turn or leave the queue, and report whether `graft` did
    /// either. The next graft in line gets the turn when it next asks.
    fn release(&mut self, now: u64, graft: &str, active: &HashSet<String>) -> bool {
        let held = self
            .holder
            .as_ref()
            .is_some_and(|holder| holder.id == graft);
        if held {
            self.holder = None;
        }
        let queued = self.queue.len();
        self.queue.retain(|waiter| waiter.id != graft);
        let released = held || self.queue.len() != queued;
        // Settled last: a holder whose turn has just run out still gave it up.
        self.settle(now, active);
        released
    }

    fn position(&self, graft: &str) -> Option<usize> {
        self.queue
            .iter()
            .position(|waiter| waiter.id == graft)
            .map(|index| index + 1)
    }

    fn idle(&self) -> bool {
        self.holder.is_none() && self.queue.is_empty()
    }

    /// What everyone may see: names and times, never a lease or a graft ID.
    fn describe(&self, resource: &str) -> Value {
        let holder = self.holder.as_ref();
        json!({
            "resource": resource,
            "holder": holder.map(|holder| &holder.name),
            "since": holder.map(|holder| holder.since),
            "expires_at": holder.map(Holder::expires_at),
            "queue": self.queue.iter().map(|waiter| &waiter.name).collect::<Vec<_>>(),
        })
    }
}

/// One resource's turn as stored, with the project and resource it is for.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    version: u8,
    project: String,
    resource: String,
    holder: Option<Holder>,
    queue: Vec<Waiter>,
}

fn file(p: &Project, resource: &str) -> PathBuf {
    p.state
        .join(DIRECTORY)
        .join(format!("{PREFIX}{resource}{SUFFIX}"))
}

/// Read the turn on `resource`: nobody holds or waits for one never taken.
/// A file that cannot be used is an error and stays as it is.
fn read(p: &Project, resource: &str) -> Result<Turn> {
    let directory = p.state.join(DIRECTORY);
    if !exists(&directory)? {
        return Ok(Turn::default());
    }
    owned_directory(&directory)?;
    let path = file(p, resource);
    if !exists(&path)? {
        return Ok(Turn::default());
    }
    (|| {
        let stored: Stored =
            serde_json::from_slice(&cache::read_private_file(&path, MAX_BYTES as u64)?)?;
        ensure!(
            stored.version == 1 && stored.project == p.id() && stored.resource == resource,
            "it belongs to another project, resource or version"
        );
        Ok(Turn {
            holder: stored.holder,
            queue: stored.queue,
        })
    })()
    .with_context(|| {
        format!(
            "The turn state of resource {resource} at {} cannot be used. Graft left it unchanged; repair it or move it aside to start the resource's queue again",
            path.display()
        )
    })
}

/// Replace the stored turn on `resource`. Callers read it first, under the
/// same project lock, so the path holds a regular file or nothing.
fn write(p: &Project, resource: &str, turn: &Turn) -> Result<()> {
    let directory = p.state.join(DIRECTORY);
    if !exists(&directory)? {
        dir(&directory)?;
        registry::sync_directory(&p.state)?;
    }
    let bytes = serde_json::to_vec(&Stored {
        version: 1,
        project: p.id().into(),
        resource: resource.into(),
        holder: turn.holder.clone(),
        queue: turn.queue.clone(),
    })?;
    ensure!(
        bytes.len() <= MAX_BYTES,
        "too many grafts wait for resource {resource}"
    );
    atomic(&file(p, resource), &bytes)
}

/// `turn take`, asked once. The caller authenticated the graft `r`.
pub(super) fn take(p: &Project, r: &Record, args: &api::TurnTake) -> Result<Value> {
    let active = registry::live_ids(p)?;
    let stored = read(p, &args.resource)?;
    let mut turn = stored.clone();
    let graft = Graft {
        id: &r.id,
        name: &r.name,
    };
    let granted = turn.take(now(), graft, args.ttl, &active);
    if turn != stored {
        write(p, &args.resource, &turn)?;
    }
    let mut reply = turn.describe(&args.resource);
    reply["granted"] = json!(granted);
    if !granted {
        reply["position"] = json!(turn.position(&r.id));
    }
    Ok(reply)
}

/// `turn release`. The caller authenticated the graft `r`.
pub(super) fn release(p: &Project, r: &Record, resource: &str) -> Result<Value> {
    let active = registry::live_ids(p)?;
    let stored = read(p, resource)?;
    let mut turn = stored.clone();
    let released = turn.release(now(), &r.id, &active);
    if turn != stored {
        write(p, resource, &turn)?;
    }
    Ok(json!({
        "resource": resource,
        "released": released,
        "next": turn.queue.first().map(|waiter| &waiter.name),
    }))
}

/// `turn list`: every resource someone holds or waits for, as the next
/// `take` would find it. Reads only; `take` and `release` store what they
/// forget.
pub(super) fn list(p: &Project) -> Result<Value> {
    let directory = p.state.join(DIRECTORY);
    let mut turns = Vec::new();
    if exists(&directory)? {
        owned_directory(&directory)?;
        let mut resources = Vec::new();
        for entry in fs::read_dir(&directory)? {
            // Anything else here, such as a temporary file an interrupted
            // write left behind, is not turn state.
            if let Some(resource) = entry?
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX))
                && validate_resource(resource).is_ok()
            {
                resources.push(resource.to_owned());
            }
        }
        resources.sort();
        let active = registry::live_ids(p)?;
        let now = now();
        for resource in resources {
            let mut turn = read(p, &resource)?;
            turn.settle(now, &active);
            if !turn.idle() {
                turns.push(turn.describe(&resource));
            }
        }
    }
    Ok(json!({ "turns": turns }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Graft<'static> = Graft {
        id: "id-a",
        name: "task-a",
    };
    const B: Graft<'static> = Graft {
        id: "id-b",
        name: "task-b",
    };
    const C: Graft<'static> = Graft {
        id: "id-c",
        name: "task-c",
    };

    fn active(grafts: &[Graft<'_>]) -> HashSet<String> {
        grafts.iter().map(|graft| graft.id.to_owned()).collect()
    }

    fn all() -> HashSet<String> {
        active(&[A, B, C])
    }

    fn queue(turn: &Turn) -> Vec<&str> {
        turn.queue
            .iter()
            .map(|waiter| waiter.name.as_str())
            .collect()
    }

    fn holder(turn: &Turn) -> Option<&str> {
        turn.holder.as_ref().map(|holder| holder.name.as_str())
    }

    #[test]
    fn the_turn_passes_in_the_order_grafts_first_asked() {
        let mut turn = Turn::default();
        assert!(turn.take(100, A, None, &all()));
        assert!(!turn.take(101, B, None, &all()));
        assert!(!turn.take(102, C, None, &all()));
        assert_eq!(
            (holder(&turn), queue(&turn)),
            (Some("task-a"), vec!["task-b", "task-c"])
        );
        assert_eq!(
            (turn.position(B.id), turn.position(C.id)),
            (Some(1), Some(2))
        );
        assert_eq!(turn.position(A.id), None);

        assert!(turn.release(103, A.id, &all()));
        // The turn is free, but task-b asked first.
        assert!(!turn.take(104, C, None, &all()));
        assert_eq!((holder(&turn), turn.position(C.id)), (None, Some(2)));
        assert!(turn.take(105, B, None, &all()));
        assert_eq!(
            (holder(&turn), queue(&turn)),
            (Some("task-b"), vec!["task-c"])
        );
        assert!(turn.release(106, B.id, &all()));
        assert!(turn.take(107, C, None, &all()));
        assert!(turn.release(108, C.id, &all()));
        assert!(turn.idle());
    }

    #[test]
    fn asking_again_keeps_a_place_in_the_queue() {
        let mut turn = Turn::default();
        assert!(turn.take(100, A, None, &all()));
        assert!(!turn.take(101, B, None, &all()));
        assert!(!turn.take(102, C, None, &all()));
        assert!(!turn.take(350, C, None, &all()));
        assert!(!turn.take(400, B, None, &all()));
        assert!(!turn.take(600, C, None, &all()));
        assert_eq!(queue(&turn), ["task-b", "task-c"]);
        let first = &turn.queue[0];
        assert_eq!((first.joined, first.asked), (101, 400));
    }

    #[test]
    fn a_turn_that_is_not_renewed_runs_out_after_its_ttl() {
        let mut turn = Turn::default();
        assert!(turn.take(100, A, Some(10), &all()));
        assert!(!turn.take(109, B, None, &all()));
        assert_eq!(holder(&turn), Some("task-a"));
        assert!(turn.take(110, B, None, &all()));
        let held = turn.holder.clone().unwrap();
        assert_eq!((held.since, held.ttl), (110, api::TURN_TTL_DEFAULT));
        // The earlier holder lost the turn and queues like anyone else.
        assert!(!turn.take(111, A, None, &all()));
        assert_eq!(turn.position(A.id), Some(1));
    }

    #[test]
    fn a_holder_whose_turn_ran_out_goes_behind_those_already_waiting() {
        let mut turn = Turn::default();
        assert!(turn.take(100, A, Some(10), &all()));
        assert!(!turn.take(105, B, None, &all()));
        assert!(!turn.take(120, A, None, &all()));
        assert_eq!(
            (holder(&turn), queue(&turn)),
            (None, vec!["task-b", "task-a"])
        );
        // Alone, it simply takes a new turn.
        let mut alone = Turn::default();
        assert!(alone.take(100, A, Some(10), &all()));
        assert!(alone.take(120, A, None, &all()));
        assert_eq!(alone.holder.unwrap().since, 120);
    }

    #[test]
    fn a_waiter_that_stopped_asking_loses_its_place() {
        let mut turn = Turn::default();
        assert!(turn.take(100, A, None, &all()));
        assert!(!turn.take(101, B, None, &all()));
        assert!(!turn.take(200, C, None, &all()));
        assert!(turn.release(300, A.id, &all()));
        // task-b is first and last asked at 101, so it has until 401.
        assert!(!turn.take(401, C, None, &all()));
        assert_eq!(queue(&turn), ["task-b", "task-c"]);
        assert!(turn.take(402, C, None, &all()));
        assert!(turn.queue.is_empty());
        // Asking again later joins at the end with a new time.
        assert!(!turn.take(403, B, None, &all()));
        assert_eq!(turn.queue[0].joined, 403);
    }

    #[test]
    fn grafts_that_are_no_longer_active_are_forgotten() {
        let mut turn = Turn::default();
        assert!(turn.take(100, A, None, &all()));
        assert!(!turn.take(101, B, None, &all()));
        assert!(!turn.take(102, C, None, &all()));
        // task-a and task-b were archived; task-c is next at once.
        assert!(turn.take(103, C, None, &active(&[C])));
        assert_eq!((holder(&turn), queue(&turn)), (Some("task-c"), vec![]));

        // A new graft that reuses an archived holder's name has another ID.
        let renamed = Graft {
            id: "id-d",
            name: C.name,
        };
        assert!(turn.take(104, renamed, None, &active(&[renamed])));
        assert_eq!(turn.holder.unwrap().id, "id-d");
    }

    #[test]
    fn taking_again_renews_the_turn_and_can_change_its_ttl() {
        let mut turn = Turn::default();
        assert!(turn.take(100, A, None, &all()));
        let expires = |turn: &Turn| turn.holder.as_ref().unwrap().expires_at();
        assert_eq!(expires(&turn), 100 + api::TURN_TTL_DEFAULT);
        assert!(turn.take(500, A, None, &all()));
        assert_eq!(expires(&turn), 500 + api::TURN_TTL_DEFAULT);
        assert!(turn.take(600, A, Some(60), &all()));
        assert_eq!(expires(&turn), 660);
        // A renewal without a ttl keeps the one the turn has.
        assert!(turn.take(650, A, None, &all()));
        assert_eq!(expires(&turn), 710);
        assert_eq!(turn.holder.unwrap().since, 100);
    }

    #[test]
    fn release_clears_a_holder_or_a_waiter_and_nothing_else() {
        let mut turn = Turn::default();
        assert!(!turn.release(99, A.id, &all()));
        assert!(turn.take(100, A, None, &all()));
        assert!(!turn.take(101, B, None, &all()));
        assert!(!turn.take(102, C, None, &all()));
        // A waiter leaves the queue and the holder keeps the turn.
        assert!(turn.release(103, B.id, &all()));
        assert_eq!(
            (holder(&turn), queue(&turn)),
            (Some("task-a"), vec!["task-c"])
        );
        assert!(!turn.release(104, B.id, &all()));
        assert!(turn.release(105, A.id, &all()));
        assert_eq!((holder(&turn), queue(&turn)), (None, vec!["task-c"]));
        assert!(!turn.release(106, A.id, &all()));

        // A turn that has already run out still counts as given up.
        let mut late = Turn::default();
        assert!(late.take(100, A, Some(1), &all()));
        assert!(late.release(200, A.id, &all()));
        assert!(late.idle());
    }

    #[test]
    fn describes_names_and_times_only() {
        let mut turn = Turn::default();
        assert_eq!(
            turn.describe("web"),
            json!({"resource":"web","holder":null,"since":null,"expires_at":null,"queue":[]})
        );
        assert!(turn.take(100, A, Some(30), &all()));
        assert!(!turn.take(101, B, None, &all()));
        assert_eq!(
            turn.describe("web"),
            json!({"resource":"web","holder":"task-a","since":100,"expires_at":130,"queue":["task-b"]})
        );
    }

    /// A project with real grafts, for the rules as stored.
    struct Fixture {
        _temp: tempfile::TempDir,
        project: Project,
        _lock: fs::File,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let temp = tempfile::tempdir()?;
            let primary = temp.path().join("primary");
            fs::create_dir(&primary)?;
            crate::git::bytes(&primary, &["init", "-q", "--template=", "-b", "main"])?;
            let (project, _lock) = Project::open(&primary)?;
            Ok(Self {
                _temp: temp,
                project,
                _lock,
            })
        }

        fn graft(&self, name: &str) -> Result<Record> {
            let p = &self.project;
            let mut record = registry::reserve(p, name, None, None, Some("agent"), Value::Null)?;
            // Like `add`, release the reservation's index lock once built.
            crate::storage::release_index(&record.path.join(".git"))?;
            crate::git::bytes(&record.path, &["init", "-q", "--template=", "-b", "agent"])?;
            registry::finish(p, &mut record)?;
            Ok(record)
        }
    }

    fn ask(p: &Project, r: &Record, resource: &str) -> Result<Value> {
        let args = api::TurnTake {
            name: r.name.clone(),
            resource: resource.into(),
            lease: r.lease.clone(),
            wait: 0,
            ttl: None,
        };
        take(p, r, &args)
    }

    fn listed(p: &Project) -> Result<Vec<(String, Value, Value)>> {
        let turns = list(p)?["turns"].take();
        Ok(turns
            .as_array()
            .unwrap()
            .iter()
            .map(|turn| {
                let resource = turn["resource"].as_str().unwrap().to_owned();
                (resource, turn["holder"].clone(), turn["queue"].clone())
            })
            .collect())
    }

    #[test]
    fn an_archived_holder_is_forgotten_and_a_restored_one_is_not() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        assert_eq!(list(p)?, json!({"turns": []}));
        assert!(!p.state.join(DIRECTORY).exists());
        let (a, b) = (f.graft("task-a")?, f.graft("task-b")?);
        assert_eq!(ask(p, &a, "web")?["granted"], true);
        assert_eq!(ask(p, &a, "database")?["granted"], true);
        assert_eq!(ask(p, &b, "web")?["position"], 1);

        // Archiving changes no turn state, yet nobody sees the holder again.
        let web = fs::read(file(p, "web"))?;
        registry::retire(p, &a.name, &a.lease, registry::Status::Dropped, None)?;
        assert_eq!(fs::read(file(p, "web"))?, web);
        assert_eq!(listed(p)?, [("web".into(), Value::Null, json!(["task-b"]))]);
        assert_eq!(ask(p, &b, "web")?["granted"], true);

        // Restored before anyone else took it and within its ttl, the graft
        // still holds the other turn, under the same graft ID.
        let restored = registry::restore(p, &a.name, &a.lease)?;
        assert_eq!(restored.id, a.id);
        assert_eq!(
            listed(p)?,
            [
                ("database".into(), json!("task-a"), json!([])),
                ("web".into(), json!("task-b"), json!([])),
            ]
        );
        assert_eq!(ask(p, &b, "database")?["position"], 1);
        assert_eq!(ask(p, &restored, "database")?["granted"], true);
        // The turn it lost while archived is the other graft's.
        assert_eq!(ask(p, &restored, "web")?["position"], 1);
        assert_eq!(
            release(p, &b, "web")?,
            json!({"resource":"web","released":true,"next":"task-a"})
        );
        Ok(())
    }

    #[test]
    fn a_graft_with_a_damaged_record_keeps_its_turn_until_it_runs_out() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        let (a, b) = (f.graft("task-a")?, f.graft("task-b")?);
        assert_eq!(ask(p, &a, "web")?["granted"], true);
        let record = p
            .state
            .join("workspaces")
            .join("records")
            .join(format!("{}.json", a.id));
        fs::write(&record, &fs::read(&record)?[..40])?;
        assert_eq!(registry::damaged(p)?.len(), 1);
        // Its graft may still be using the resource.
        let reply = ask(p, &b, "web")?;
        assert_eq!(
            (&reply["granted"], &reply["holder"]),
            (&json!(false), &json!("task-a"))
        );
        Ok(())
    }

    #[test]
    fn turn_state_that_cannot_be_used_is_reported_and_left_unchanged() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        let a = f.graft("task-a")?;
        assert_eq!(ask(p, &a, "web")?["granted"], true);
        let good = fs::read(file(p, "web"))?;

        let foreign =
            String::from_utf8(good.clone())?.replace(p.id(), &uuid::Uuid::new_v4().to_string());
        for unusable in [b"{\"version\":1".to_vec(), foreign.into_bytes()] {
            atomic(&file(p, "web"), &unusable)?;
            for error in [
                ask(p, &a, "web").unwrap_err(),
                release(p, &a, "web").unwrap_err(),
                list(p).unwrap_err(),
            ] {
                let message = format!("{error:#}");
                assert!(
                    message.contains("turn-web.json") && message.contains("left it unchanged"),
                    "{message}"
                );
            }
            assert_eq!(fs::read(file(p, "web"))?, unusable);
            // Other resources are unaffected.
            assert_eq!(ask(p, &a, "database")?["granted"], true);
        }

        // A stray file in the directory is not turn state.
        atomic(&file(p, "web"), &good)?;
        fs::write(p.state.join(DIRECTORY).join(".tmp-interrupted"), "partial")?;
        fs::write(
            p.state.join(DIRECTORY).join("turn-Not A Resource.json"),
            "{}",
        )?;
        assert_eq!(listed(p)?.len(), 2);
        Ok(())
    }

    #[test]
    fn resource_names_are_portable() {
        for valid in [
            "web",
            "database",
            "ios-simulator-2",
            "3d-printer",
            "com3",
            "a",
            &"a".repeat(64),
        ] {
            validate_resource(valid).unwrap();
        }
        for invalid in [
            "",
            "Web",
            "web server",
            "web_server",
            "-web",
            "web/api",
            "../web",
            "wéb",
            &"a".repeat(65),
        ] {
            let error = validate_resource(invalid).unwrap_err();
            assert_eq!(
                api::error_json(&error)["code"],
                "invalid_resource",
                "{invalid}"
            );
        }
    }

    #[test]
    fn wait_and_ttl_have_limits() {
        let take = |wait, ttl| api::TurnTake {
            name: "task-a".into(),
            resource: "web".into(),
            lease: String::new(),
            wait,
            ttl,
        };
        for (wait, ttl) in [
            (0, None),
            (api::TURN_WAIT_MAX, Some(1)),
            (0, Some(api::TURN_TTL_MAX)),
        ] {
            take(wait, ttl).check().unwrap();
        }
        for (wait, ttl) in [
            (api::TURN_WAIT_MAX + 1, None),
            (0, Some(0)),
            (0, Some(api::TURN_TTL_MAX + 1)),
        ] {
            take(wait, ttl).check().unwrap_err();
        }
    }
}
