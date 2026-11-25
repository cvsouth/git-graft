//! Remotes of the primary that the project's configuration gives every graft
//! (`remotes` in `.graftconfig`), so ordinary `git fetch` and `git push` work
//! in it. A graft has no remotes otherwise. Only a remote's URLs and fetch
//! refspecs are copied, never credentials: a remote whose URL carries a user
//! name or password is left out.
use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use crate::git;

/// The values of `key` in the configuration of the repository at `dir`.
fn values(dir: &Path, key: &str) -> Result<Vec<String>> {
    let out = git::raw(dir, &["config", "--get-all", key])?;
    // Exit status 1 means the key is not set.
    if out.status.code() == Some(1) {
        return Ok(Vec::new());
    }
    anyhow::ensure!(out.status.success(), "git config --get-all {key} failed");
    Ok(String::from_utf8(out.stdout)?
        .lines()
        .map(str::to_owned)
        .collect())
}

/// Whether `url` names a user or password before its host, as in
/// `https://user:token@host/repo`. An scp-like `git@host:repo` names only the
/// SSH user, which is no credential.
fn carries_credentials(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let Some((userinfo, _)) = authority.rsplit_once('@') else {
        return false;
    };
    // An SSH user is how SSH logs in, not a secret; a password is.
    let ssh = scheme.eq_ignore_ascii_case("ssh") || scheme.eq_ignore_ascii_case("git+ssh");
    !ssh || userinfo.contains(':')
}

/// Give the graft at `graft` each remote `names` lists, as the primary at
/// `primary` configures it, and, when `copy_refs`, the primary's
/// remote-tracking refs of it. A remote the graft already has is left as it
/// is. Returns one entry per name: `configured`, the `refs` copied, or why it
/// was `skipped`.
pub(crate) fn configure(
    primary: &Path,
    graft: &Path,
    names: &[String],
    copy_refs: bool,
) -> Result<Vec<Value>> {
    let mut entries = Vec::new();
    for name in names {
        let urls = values(primary, &format!("remote.{name}.url"))?;
        let entry = |status: &str, extra: Value| {
            let mut entry = json!({"name": name, "status": status});
            if let (Value::Object(entry), Value::Object(extra)) = (&mut entry, extra) {
                entry.extend(extra);
            }
            entry
        };
        if urls.is_empty() {
            entries.push(entry("skipped", json!({"reason": "not_configured"})));
            continue;
        }
        let pushurls = values(primary, &format!("remote.{name}.pushurl"))?;
        if urls
            .iter()
            .chain(&pushurls)
            .any(|url| carries_credentials(url))
        {
            entries.push(entry("skipped", json!({"reason": "url_has_credentials"})));
            continue;
        }
        if !values(graft, &format!("remote.{name}.url"))?.is_empty() {
            entries.push(entry("kept", json!({})));
            continue;
        }
        let mut fetch = values(primary, &format!("remote.{name}.fetch"))?;
        if fetch.is_empty() {
            fetch.push(format!("+refs/heads/*:refs/remotes/{name}/*"));
        }
        let set = |key: String, values: &[String]| -> Result<()> {
            for value in values {
                git::bytes(graft, &["config", "--add", &key, value])?;
            }
            Ok(())
        };
        set(format!("remote.{name}.url"), &urls)?;
        set(format!("remote.{name}.pushurl"), &pushurls)?;
        set(format!("remote.{name}.fetch"), &fetch)?;
        let mut refs = 0;
        if copy_refs {
            // With local history the graft already holds their objects.
            let refspec = format!("+refs/remotes/{name}/*:refs/remotes/{name}/*");
            let primary_str = git::path_str(primary)?;
            git::bytes(
                graft,
                &[
                    "fetch",
                    "-q",
                    "--no-tags",
                    "--no-write-fetch-head",
                    primary_str,
                    &refspec,
                ],
            )?;
            refs = git::bytes(
                graft,
                &[
                    "for-each-ref",
                    "--format=x",
                    &format!("refs/remotes/{name}/"),
                ],
            )?
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .count();
        }
        entries.push(entry("configured", json!({"refs": refs})));
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_in_a_url_are_recognised() {
        for plain in [
            "git@github.com:owner/repo.git",
            "ssh://git@github.com/owner/repo.git",
            "https://github.com/owner/repo.git",
            "/srv/git/repo.git",
            "file:///srv/git/repo.git",
        ] {
            assert!(!carries_credentials(plain), "{plain}");
        }
        for secret in [
            "https://token@github.com/owner/repo.git",
            "https://user:pass@example.com/repo.git",
        ] {
            assert!(carries_credentials(secret), "{secret}");
        }
    }

    fn repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git::bytes(dir, &["init", "-q", "--template=", "-b", "main"]).unwrap();
        git::bytes(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "one",
            ],
        )
        .unwrap();
    }

    #[test]
    fn named_remotes_are_configured_with_their_refs_and_others_are_reported() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (primary, graft) = (tmp.path().join("primary"), tmp.path().join("graft"));
        repo(&primary);
        repo(&graft);
        let head = git::text(&primary, &["rev-parse", "HEAD"])?;
        git::bytes(
            &primary,
            &["remote", "add", "origin", "git@example.com:owner/repo.git"],
        )?;
        git::bytes(
            &primary,
            &[
                "config",
                "remote.origin.pushurl",
                "git@example.com:owner/push.git",
            ],
        )?;
        git::bytes(&primary, &["update-ref", "refs/remotes/origin/main", &head])?;
        git::bytes(
            &primary,
            &[
                "remote",
                "add",
                "secret",
                "https://token@example.com/repo.git",
            ],
        )?;
        let names: Vec<String> = ["origin", "secret", "missing"].map(str::to_owned).into();
        let entries = configure(&primary, &graft, &names, false)?;
        assert_eq!(
            entries,
            [
                json!({"name": "origin", "status": "configured", "refs": 0}),
                json!({"name": "secret", "status": "skipped", "reason": "url_has_credentials"}),
                json!({"name": "missing", "status": "skipped", "reason": "not_configured"}),
            ]
        );
        assert_eq!(
            git::text(&graft, &["config", "remote.origin.url"])?,
            "git@example.com:owner/repo.git"
        );
        assert_eq!(
            git::text(&graft, &["config", "remote.origin.pushurl"])?,
            "git@example.com:owner/push.git"
        );
        assert_eq!(
            git::text(&graft, &["config", "remote.origin.fetch"])?,
            "+refs/heads/*:refs/remotes/origin/*"
        );
        assert!(values(&graft, "remote.secret.url")?.is_empty());
        // A remote the graft has is kept as it is.
        let again = configure(&primary, &graft, &names[..1], true)?;
        assert_eq!(again, [json!({"name": "origin", "status": "kept"})]);

        // With refs copied, the graft has the primary's remote-tracking refs.
        let fresh = tmp.path().join("fresh");
        repo(&fresh);
        git::bytes(&fresh, &["fetch", "-q", git::path_str(&primary)?, "HEAD"])?;
        let copied = configure(&primary, &fresh, &names[..1], true)?;
        assert_eq!(
            copied,
            [json!({"name": "origin", "status": "configured", "refs": 1})]
        );
        assert_eq!(git::text(&fresh, &["rev-parse", "origin/main"])?, head);
        Ok(())
    }
}
