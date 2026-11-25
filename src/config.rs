//! The project's Graft configuration, read from two JSON files at the root
//! of the primary checkout: `.graftconfig`, which a project commits and
//! shares, and `.graftconfig.local`, the person's own. Each may set the
//! `include` patterns a new graft copies by default and the `hooks` that run
//! when a graft is added, about to be archived or restored.
//!
//! The local file overrides the shared one setting by setting. Include lists
//! are joined, shared patterns first, and a local `!pattern` removes the same
//! shared pattern. Hooks are keyed by name: a local hook replaces the shared
//! hook of that name whole, a local `null` turns it off, and a new name adds
//! a hook after the shared ones.
//!
//! Both files are read from the primary checkout, never from a graft, so
//! what a graft changes in its own copy does not change what Graft runs.
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;
use serde::de::{self, MapAccess, Visitor};
use serde_json::{Value, json};

use crate::workspace::hook::{self, Event};

/// The configuration a project commits and shares.
pub(crate) const SHARED: &str = ".graftconfig";
/// The person's own configuration, which overrides the shared one.
pub(crate) const LOCAL: &str = ".graftconfig.local";
const MAX_BYTES: u64 = 1024 * 1024;
const MAX_NAME_BYTES: usize = 64;

/// A hook as the configuration defines it: the program and arguments to run
/// for an event, and the file that defined it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hook {
    pub name: String,
    pub event: Event,
    pub argv: Vec<String>,
    pub file: &'static str,
}

impl Hook {
    pub(crate) fn describe(&self) -> Value {
        json!({"name": self.name, "event": self.event, "argv": self.argv, "file": self.file})
    }
}

/// The effective configuration of a project: both files merged.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Config {
    /// The include patterns a graft copies when a request names none.
    pub include: Vec<String>,
    /// The files whose `include` lists supplied a pattern.
    pub include_files: Vec<&'static str>,
    /// Every hook, in the order they run.
    pub hooks: Vec<Hook>,
}

impl Config {
    /// Read and merge the configuration files at the root of `primary`. A
    /// missing file sets nothing. A bare repository has neither.
    pub(crate) fn read(primary: &Path) -> Result<Self> {
        if primary.join(".git").symlink_metadata().is_err() {
            return Ok(Self::default());
        }
        let shared = load(primary, SHARED)?;
        let local = load(primary, LOCAL)?;
        merge(shared, local)
    }

    /// The hooks for `event`, in the order they run.
    pub(crate) fn hooks_for(&self, event: Event) -> Vec<Hook> {
        self.hooks
            .iter()
            .filter(|hook| hook.event == event)
            .cloned()
            .collect()
    }

    /// How `local-files` describes the include defaults.
    pub(crate) fn include_defaults(&self) -> Value {
        json!({"patterns": self.include, "files": self.include_files})
    }
}

/// One configuration file as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    include: Option<Vec<String>>,
    #[serde(default)]
    hooks: Option<Named>,
}

/// A hook entry as written. `null` in the local file turns off the shared
/// hook of the same name.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    event: Event,
    argv: Vec<String>,
}

/// The `hooks` object, in the order its names are written.
#[derive(Debug, Default)]
struct Named(Vec<(String, Option<Spec>)>);

impl<'de> Deserialize<'de> for Named {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Ordered;
        impl<'de> Visitor<'de> for Ordered {
            type Value = Named;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object of hooks keyed by name")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Named, A::Error> {
                let mut hooks: Vec<(String, Option<Spec>)> = Vec::new();
                while let Some(name) = map.next_key::<String>()? {
                    if hooks.iter().any(|(known, _)| *known == name) {
                        return Err(de::Error::custom(format!("hook {name:?} is defined twice")));
                    }
                    // Read each entry as JSON first: only an object or null is a
                    // hook entry, not the array a derived struct would accept.
                    let spec = match map.next_value::<Value>()? {
                        Value::Null => None,
                        entry @ Value::Object(_) => {
                            Some(Spec::deserialize(entry).map_err(de::Error::custom)?)
                        }
                        other => {
                            return Err(de::Error::custom(format!(
                                "hook {name:?} must be an object or null, not {other}"
                            )));
                        }
                    };
                    hooks.push((name, spec));
                }
                Ok(Named(hooks))
            }
        }
        deserializer.deserialize_map(Ordered)
    }
}

fn invalid(path: &Path, cause: impl fmt::Display) -> anyhow::Error {
    crate::api::fault(
        "invalid_config",
        format!(
            "{} is not a valid Graft configuration: {cause}",
            path.display()
        ),
        json!({"file": path}),
    )
}

/// Read and check the file `name` at the root of `primary`, if it exists.
fn load(primary: &Path, name: &'static str) -> Result<Option<File>> {
    let path = primary.join(name);
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(invalid(&path, error)),
    };
    let meta = file.metadata().map_err(|error| invalid(&path, error))?;
    if !meta.is_file() {
        return Err(invalid(&path, "it is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| invalid(&path, error))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid(
            &path,
            format!("it is larger than {MAX_BYTES} bytes"),
        ));
    }
    // A derived struct would also accept an array; the file holds an object.
    if bytes.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return Err(invalid(&path, "it must hold a JSON object"));
    }
    let parsed: File = serde_json::from_slice(&bytes).map_err(|error| invalid(&path, error))?;
    check(&parsed, name).map_err(|error| invalid(&path, format!("{error:#}")))?;
    Ok(Some(parsed))
}

/// Refuse what Graft could not use: a pattern on more than one line, a hook
/// name it could not report plainly, or a program `hook::validate` refuses.
fn check(file: &File, name: &'static str) -> Result<()> {
    if let Some(patterns) = &file.include {
        crate::include::validate(patterns)?;
    }
    for (hook, spec) in file.hooks.iter().flat_map(|named| &named.0) {
        anyhow::ensure!(
            !hook.is_empty()
                && hook.len() <= MAX_NAME_BYTES
                && hook
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
            "hook name {hook:?} must be 1 to {MAX_NAME_BYTES} letters, digits, '-', '_' or '.'"
        );
        match spec {
            Some(spec) => hook::validate(&spec.argv)
                .map_err(|error| anyhow::anyhow!("hook {hook:?}: {error:#}"))?,
            None => anyhow::ensure!(
                name == LOCAL,
                "hook {hook:?} is null; null turns off a shared hook and belongs in {LOCAL}"
            ),
        }
    }
    Ok(())
}

/// The effective configuration: the shared file, overridden by the local one.
fn merge(shared: Option<File>, local: Option<File>) -> Result<Config> {
    let mut config = Config::default();
    let shared = shared.unwrap_or_default();
    let local = local.unwrap_or_default();
    let blank = |pattern: &String| pattern.trim().is_empty();

    let shared_patterns: Vec<String> = shared.include.unwrap_or_default();
    config.include = shared_patterns
        .iter()
        .filter(|p| !blank(p))
        .cloned()
        .collect();
    if !config.include.is_empty() {
        config.include_files.push(SHARED);
    }
    let mut local_added = false;
    for pattern in local.include.unwrap_or_default() {
        if blank(&pattern) {
            continue;
        }
        // `!pattern` removes the same shared pattern. One that matches no
        // shared pattern stays, as a .gitignore negation of what precedes it.
        if let Some(removed) = pattern.strip_prefix('!')
            && config.include.iter().any(|known| known == removed)
        {
            config.include.retain(|known| known != removed);
        } else {
            config.include.push(pattern);
        }
        local_added = true;
    }
    if local_added {
        config.include_files.push(LOCAL);
    }
    if config.include.is_empty() {
        config.include_files.clear();
    }

    for (name, spec) in shared.hooks.unwrap_or_default().0 {
        if let Some(spec) = spec {
            config.hooks.push(Hook {
                name,
                event: spec.event,
                argv: spec.argv,
                file: SHARED,
            });
        }
    }
    for (name, spec) in local.hooks.unwrap_or_default().0 {
        let known = config.hooks.iter().position(|hook| hook.name == name);
        match (known, spec) {
            // Replacing a shared hook keeps its place in the order.
            (Some(at), Some(spec)) => {
                config.hooks[at] = Hook {
                    name,
                    event: spec.event,
                    argv: spec.argv,
                    file: LOCAL,
                };
            }
            (Some(at), None) => {
                config.hooks.remove(at);
            }
            (None, Some(spec)) => config.hooks.push(Hook {
                name,
                event: spec.event,
                argv: spec.argv,
                file: LOCAL,
            }),
            // Turning off a hook the shared file does not define changes nothing.
            (None, None) => {}
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn primary() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        crate::git::bytes(tmp.path(), &["init", "-q", "--template="]).unwrap();
        tmp
    }

    fn write(root: &Path, name: &str, body: &str) {
        fs::write(root.join(name), body).unwrap();
    }

    fn absolute(name: &str) -> String {
        if cfg!(windows) {
            format!(r"C:\tools\{name}.exe")
        } else {
            format!("/usr/local/bin/{name}")
        }
    }

    fn names(config: &Config) -> Vec<(&str, Event, &str)> {
        config
            .hooks
            .iter()
            .map(|hook| (hook.name.as_str(), hook.event, hook.file))
            .collect()
    }

    #[test]
    fn missing_files_and_bare_repositories_set_nothing() {
        let tmp = primary();
        assert_eq!(Config::read(tmp.path()).unwrap(), Config::default());
        let bare = tempfile::tempdir().unwrap();
        crate::git::bytes(bare.path(), &["init", "-q", "--bare", "--template="]).unwrap();
        write(bare.path(), SHARED, "not json");
        assert_eq!(Config::read(bare.path()).unwrap(), Config::default());
        write(tmp.path(), SHARED, "{}");
        write(tmp.path(), LOCAL, r#"{"include": [], "hooks": {}}"#);
        assert_eq!(Config::read(tmp.path()).unwrap(), Config::default());
    }

    #[test]
    fn local_include_patterns_add_to_the_shared_ones_and_remove_them_with_a_bang() {
        let tmp = primary();
        write(
            tmp.path(),
            SHARED,
            r#"{"include": [".env", "config/*.json", " ", "build/"]}"#,
        );
        let config = Config::read(tmp.path()).unwrap();
        assert_eq!(config.include, [".env", "config/*.json", "build/"]);
        assert_eq!(config.include_files, [SHARED]);

        write(
            tmp.path(),
            LOCAL,
            r#"{"include": ["!build/", ".env.local", "!config/secret.json", ""]}"#,
        );
        let config = Config::read(tmp.path()).unwrap();
        assert_eq!(
            config.include,
            [".env", "config/*.json", ".env.local", "!config/secret.json"]
        );
        assert_eq!(config.include_files, [SHARED, LOCAL]);
        assert_eq!(
            config.include_defaults(),
            json!({"patterns": config.include, "files": [SHARED, LOCAL]})
        );

        // Removing every shared pattern leaves no default at all.
        write(
            tmp.path(),
            LOCAL,
            r#"{"include": ["!.env", "!config/*.json", "!build/"]}"#,
        );
        let config = Config::read(tmp.path()).unwrap();
        assert!(config.include.is_empty());
        assert!(config.include_files.is_empty());

        // The local file alone can set the default.
        fs::remove_file(tmp.path().join(SHARED)).unwrap();
        write(tmp.path(), LOCAL, r#"{"include": [".env"]}"#);
        let config = Config::read(tmp.path()).unwrap();
        assert_eq!(config.include, [".env"]);
        assert_eq!(config.include_files, [LOCAL]);
    }

    #[test]
    fn local_hooks_replace_turn_off_or_add_to_shared_hooks_by_name() {
        let tmp = primary();
        // Written as text: the order of the names is the order hooks run.
        let create = json!(absolute("create-db"));
        let drop = json!(absolute("drop-db"));
        let shared = format!(
            r#"{{"hooks": {{
                "database": {{"event": "post-add", "argv": [{create}]}},
                "drop-database": {{"event": "pre-archive", "argv": [{drop}]}},
                "assets": {{"event": "post-add", "argv": ["npm", "run", "assets"]}},
                "reopen": {{"event": "post-restore", "argv": [{create}]}}
            }}}}"#
        );
        write(tmp.path(), SHARED, &shared);
        let config = Config::read(tmp.path()).unwrap();
        assert_eq!(
            names(&config),
            [
                ("database", Event::PostAdd, SHARED),
                ("drop-database", Event::PreArchive, SHARED),
                ("assets", Event::PostAdd, SHARED),
                ("reopen", Event::PostRestore, SHARED),
            ]
        );

        let local = format!(
            r#"{{"hooks": {{
                "notify": {{"event": "post-add", "argv": ["notify-send", "graft added"]}},
                "assets": null,
                "database": {{"event": "post-add", "argv": [{create}, "--small"]}},
                "not-shared": null
            }}}}"#
        );
        write(tmp.path(), LOCAL, &local);
        let config = Config::read(tmp.path()).unwrap();
        // A replaced hook keeps its place; a new one runs after the shared ones.
        assert_eq!(
            names(&config),
            [
                ("database", Event::PostAdd, LOCAL),
                ("drop-database", Event::PreArchive, SHARED),
                ("reopen", Event::PostRestore, SHARED),
                ("notify", Event::PostAdd, LOCAL),
            ]
        );
        assert_eq!(
            config.hooks[0].argv,
            [absolute("create-db"), "--small".into()]
        );
        let post_add: Vec<_> = config
            .hooks_for(Event::PostAdd)
            .into_iter()
            .map(|hook| hook.name)
            .collect();
        assert_eq!(post_add, ["database", "notify"]);
        assert!(config.hooks_for(Event::PreArchive).len() == 1);
        assert_eq!(
            config.hooks[1].describe(),
            json!({"name": "drop-database", "event": "pre-archive", "argv": [absolute("drop-db")], "file": SHARED})
        );
    }

    #[test]
    fn a_local_hook_may_move_a_shared_one_to_another_event() {
        let tmp = primary();
        write(
            tmp.path(),
            SHARED,
            r#"{"hooks": {"setup": {"event": "post-add", "argv": ["setup"]}}}"#,
        );
        write(
            tmp.path(),
            LOCAL,
            r#"{"hooks": {"setup": {"event": "post-restore", "argv": ["setup"]}}}"#,
        );
        let config = Config::read(tmp.path()).unwrap();
        assert!(config.hooks_for(Event::PostAdd).is_empty());
        assert_eq!(config.hooks_for(Event::PostRestore)[0].file, LOCAL);
    }

    #[test]
    fn malformed_files_are_refused_with_the_file_named() {
        let tmp = primary();
        for (file, body, cause) in [
            (SHARED, "{", "EOF"),
            (SHARED, "[]", "JSON object"),
            (SHARED, "", "JSON object"),
            (
                LOCAL,
                r#"{"hooks": {"a": ["post-add", ["x"]]}}"#,
                "object or null",
            ),
            (SHARED, r#"{"includes": [".env"]}"#, "unknown field"),
            (SHARED, r#"{"include": ".env"}"#, "invalid type"),
            (SHARED, r#"{"include": ["a\nb"]}"#, "single line"),
            (
                LOCAL,
                r#"{"hooks": {"a": {"event": "post-merge", "argv": ["x"]}}}"#,
                "unknown variant",
            ),
            (
                LOCAL,
                r#"{"hooks": {"a": {"event": "post-add", "argv": ["x"], "shell": true}}}"#,
                "unknown field",
            ),
            (
                LOCAL,
                r#"{"hooks": {"a": {"event": "post-add"}}}"#,
                "missing field",
            ),
            (
                LOCAL,
                r#"{"hooks": {"a": {"event": "post-add", "argv": []}}}"#,
                "program",
            ),
            (
                LOCAL,
                r#"{"hooks": {"a": {"event": "post-add", "argv": ["./setup"]}}}"#,
                "relative path",
            ),
            (
                LOCAL,
                r#"{"hooks": {"a": {"event": "post-add", "argv": "setup"}}}"#,
                "invalid type",
            ),
            (
                LOCAL,
                r#"{"hooks": {"a b": {"event": "post-add", "argv": ["x"]}}}"#,
                "hook name",
            ),
            (
                LOCAL,
                r#"{"hooks": {"": {"event": "post-add", "argv": ["x"]}}}"#,
                "hook name",
            ),
            (
                LOCAL,
                r#"{"hooks": {"a": {"event": "post-add", "argv": ["x"]}, "a": null}}"#,
                "defined twice",
            ),
            (
                SHARED,
                r#"{"hooks": {"a": null}}"#,
                "belongs in .graftconfig.local",
            ),
            (SHARED, r#"{"hooks": []}"#, "object of hooks"),
        ] {
            let _ = fs::remove_file(tmp.path().join(SHARED));
            let _ = fs::remove_file(tmp.path().join(LOCAL));
            write(tmp.path(), file, body);
            let error = Config::read(tmp.path()).expect_err(body);
            let json = crate::api::error_json(&error);
            assert_eq!(json["code"], "invalid_config", "{body}");
            let message = json["message"].as_str().unwrap();
            assert!(message.contains(file), "{message}");
            assert!(message.contains(cause), "{body}: {message}");
            assert_eq!(json["details"]["file"], json!(tmp.path().join(file)));
        }
        let _ = fs::remove_file(tmp.path().join(LOCAL));
        fs::remove_file(tmp.path().join(SHARED)).unwrap();
        fs::create_dir(tmp.path().join(SHARED)).unwrap();
        let error = Config::read(tmp.path()).unwrap_err();
        assert_eq!(crate::api::error_json(&error)["code"], "invalid_config");
    }
}
