//! Git's built-in end-of-line clean conversion, which decides whether a
//! primary file can be reused in a graft.
//!
//! A graft mirrors its primary. A tracked primary file F is reused for the
//! selected blob B exactly when Git's own clean conversion of F, which is
//! what `git status` compares with the index and what `git add` stores,
//! yields B byte for byte. The graft then keeps the primary's line endings,
//! and Git in the graft agrees that F holds B.
//!
//! This applies only to paths whose sole conversion is the built-in one set
//! by `core.autocrlf`, `core.eol` and the `text`, `crlf` and `eol`
//! attributes. A path with a `filter`, `ident` or `working-tree-encoding`
//! attribute has no [`Rule`] and is never reused: a clean filter can hide
//! real edits, so nothing is ever compared through one.
//!
//! The model follows convert.c (Git 2.46): `convert_attrs` chooses the
//! `crlf_action`, `gather_stats` and `convert_is_binary` classify the file,
//! `crlf_to_git` converts it, consulting `has_crlf_in_index` for automatic
//! detection, and `check_global_conv_flags_eol` makes `git add` refuse a
//! conversion that checkout would not reverse when `core.safecrlf=true`.
//! The tests compare every decision with Git itself.
use super::{Attribute, Conversion};
use sha1::Digest;
use std::io::{self, Read};

/// convert.c's `crlf_action`. [`Settings::action`] resolves `Text`, `Auto`
/// and `Undefined` from configuration, so a [`Rule`] holds only the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Undefined,
    Binary,
    Text,
    TextInput,
    TextCrlf,
    Auto,
    AutoInput,
    AutoCrlf,
}

impl Action {
    const RESOLVED: [Self; 5] = [
        Self::Binary,
        Self::TextInput,
        Self::TextCrlf,
        Self::AutoInput,
        Self::AutoCrlf,
    ];

    fn bit(self) -> u8 {
        1 << self as u8
    }

    /// Git guesses whether the file is text (`text=auto` or `core.autocrlf`).
    fn guesses(self) -> bool {
        matches!(self, Self::AutoInput | Self::AutoCrlf)
    }

    /// `git_path_check_crlf`: the actions an attribute value may mean.
    /// `git check-attr` prints `set` and `unset` both for the boolean states
    /// and for those literal strings, which Git treats as undefined.
    fn declared(attribute: Attribute) -> &'static [Self] {
        match attribute {
            Attribute::Set => &[Self::Text, Self::Undefined],
            Attribute::Unset => &[Self::Binary, Self::Undefined],
            Attribute::Input => &[Self::TextInput],
            Attribute::Auto => &[Self::Auto],
            _ => &[Self::Undefined],
        }
    }

    /// Whether `crlf_to_git` stores exactly the blob for a file with
    /// `stats` whose bytes hash to the blob unchanged (`identity`), or with
    /// every CR that precedes an LF removed (`!identity`).
    fn stores(self, stats: &Stats, identity: bool, safe_crlf: bool) -> bool {
        // An empty file or a binary action is never converted or checked.
        if self == Self::Binary || stats.size == 0 {
            return identity;
        }
        // Guessing returns early for binary content, before the round-trip check.
        if self.guesses() && stats.binary() {
            return identity;
        }
        // `has_crlf_in_index`: the index holds the blob. When the file is
        // the blob, that is the file's own (text) CRLF count. A blob that
        // matched the converted bytes has no CR at all, because text
        // content has no lone CR and conversion removed every other one.
        let index_crlf = self.guesses() && identity && stats.crlf > 0;
        let converts = stats.crlf > 0 && !index_crlf;
        if converts == identity {
            return false;
        }
        !safe_crlf || self.round_trips(stats, converts)
    }

    /// `check_global_conv_flags_eol`: whether checking out what `git add`
    /// stores restores the file's line endings, so `core.safecrlf=true`
    /// lets `git add` store it.
    fn round_trips(self, stats: &Stats, converts: bool) -> bool {
        let (mut crlf, mut lonelf) = (stats.crlf, stats.lonelf);
        // Simulate `git add`.
        if converts {
            lonelf += crlf;
            crlf = 0;
        }
        // Simulate `git checkout` (`will_convert_lf_to_crlf`).
        let expands = matches!(self, Self::TextCrlf | Self::AutoCrlf)
            && lonelf > 0
            && !(self.guesses() && (stats.lonecr > 0 || crlf > 0 || stats.binary()));
        if expands {
            crlf += lonelf;
            lonelf = 0;
        }
        !(stats.crlf > 0 && crlf == 0 || stats.lonelf > 0 && lonelf == 0)
    }
}

/// `core.autocrlf`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AutoCrlf {
    False,
    True,
    Input,
}

/// The configuration that shapes Git's line-ending clean in the graft.
#[derive(Clone, Copy, Debug)]
pub(super) struct Settings {
    pub(super) auto_crlf: AutoCrlf,
    /// `core.eol=crlf`.
    pub(super) eol_crlf: bool,
    /// `core.safecrlf=true`: `git add` refuses a conversion that checkout
    /// would not reverse (`warn`, the default, only warns).
    pub(super) safe_crlf: bool,
}

impl Settings {
    /// `text_eol_is_crlf` on a platform whose native line ending is LF.
    fn text_eol_is_crlf(self) -> bool {
        match self.auto_crlf {
            AutoCrlf::True => true,
            AutoCrlf::Input => false,
            AutoCrlf::False => self.eol_crlf,
        }
    }

    /// `convert_attrs` with the result of `git_path_check_crlf`.
    fn action(self, declared: Action, eol: Attribute) -> Action {
        let action = match (declared, eol) {
            (Action::Binary, _) => Action::Binary,
            (Action::Auto, Attribute::Lf) => Action::AutoInput,
            (Action::Auto, Attribute::Crlf) => Action::AutoCrlf,
            (_, Attribute::Lf) => Action::TextInput,
            (_, Attribute::Crlf) => Action::TextCrlf,
            (action, _) => action,
        };
        let crlf = self.text_eol_is_crlf();
        match action {
            Action::Text if crlf => Action::TextCrlf,
            Action::Text => Action::TextInput,
            Action::Auto if crlf => Action::AutoCrlf,
            Action::Auto => Action::AutoInput,
            Action::Undefined => match self.auto_crlf {
                AutoCrlf::False => Action::Binary,
                AutoCrlf::True => Action::AutoCrlf,
                AutoCrlf::Input => Action::AutoInput,
            },
            action => action,
        }
    }

    /// The reuse rule for a path with these attributes, or `None` when Git
    /// alone can convert it.
    pub(super) fn rule(self, conversion: &Conversion) -> Option<Rule> {
        if conversion.git {
            return None;
        }
        let mut actions = 0;
        for &text in Action::declared(conversion.text) {
            // `text` takes precedence; the legacy `crlf` applies without it.
            let declared = if text == Action::Undefined {
                Action::declared(conversion.crlf)
            } else {
                std::slice::from_ref(&text)
            };
            for &action in declared {
                actions |= self.action(action, conversion.eol).bit();
            }
        }
        Some(Rule {
            actions,
            safe_crlf: self.safe_crlf,
        })
    }
}

/// Every resolved `crlf_action` Git may apply to one path. A file is reused
/// only when every one of them stores exactly the blob.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Rule {
    actions: u8,
    safe_crlf: bool,
}

impl Rule {
    /// Matches no file; used for paths that are not reused at all.
    pub(super) const NEVER: Self = Self {
        actions: 0,
        safe_crlf: false,
    };

    /// Bytes Git never converts, such as a symbolic link's target.
    #[cfg(any(target_os = "macos", test))]
    pub(super) const BINARY: Self = Self {
        actions: 1 << Action::Binary as u8,
        safe_crlf: false,
    };

    fn actions(self) -> impl Iterator<Item = Action> {
        Action::RESOLVED
            .into_iter()
            .filter(move |action| self.actions & action.bit() != 0)
    }

    /// Whether Git's clean stores every file that already equals the blob
    /// unchanged, so Git's own stat test proving the primary file equals the
    /// blob proves it is reusable without reading it. Binary paths are never
    /// converted. A guessed path that equals its blob is either binary, has
    /// CRLF that `has_crlf_in_index` keeps, or has no CRLF to remove. Text
    /// paths and `core.safecrlf=true` need the file's contents.
    pub(super) fn identity_is_clean(self) -> bool {
        self.actions != 0
            && self
                .actions()
                .all(|action| action == Action::Binary || action.guesses() && !self.safe_crlf)
    }

    /// Whether Git's clean of a file with `stats` stores exactly the blob it
    /// hashed to, unchanged (`identity`) or converted.
    fn accepts(self, stats: &Stats, identity: bool) -> bool {
        self.actions != 0
            && self
                .actions()
                .all(|action| action.stores(stats, identity, self.safe_crlf))
    }

    fn may_convert(self) -> bool {
        self.actions().any(|action| action != Action::Binary)
    }
}

/// convert.c's `struct text_stat`, gathered while streaming.
#[derive(Clone, Copy, Debug, Default)]
struct Stats {
    nul: u64,
    lonecr: u64,
    lonelf: u64,
    crlf: u64,
    printable: u64,
    nonprintable: u64,
    size: u64,
    last: Option<u8>,
    /// The previous chunk ended with a CR whose successor is not yet known.
    cr: bool,
}

impl Stats {
    fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if std::mem::take(&mut self.cr) {
                if byte == b'\n' {
                    self.crlf += 1;
                    continue;
                }
                self.lonecr += 1;
            }
            match byte {
                b'\r' => self.cr = true,
                b'\n' => self.lonelf += 1,
                // BS, HT, ESC and FF are printable.
                b'\x08' | b'\t' | b'\x1b' | b'\x0c' => self.printable += 1,
                0 => {
                    self.nul += 1;
                    self.nonprintable += 1;
                }
                1..32 | 127 => self.nonprintable += 1,
                _ => self.printable += 1,
            }
        }
        self.size += bytes.len() as u64;
        if let Some(&byte) = bytes.last() {
            self.last = Some(byte);
        }
    }

    fn finish(&mut self) {
        if std::mem::take(&mut self.cr) {
            self.lonecr += 1;
        }
        // A trailing Ctrl-Z (DOS end of file) is not counted as non-printable.
        if self.last == Some(0x1a) {
            self.nonprintable -= 1;
        }
    }

    /// `convert_is_binary`.
    fn binary(&self) -> bool {
        self.lonecr > 0 || self.nul > 0 || (self.printable >> 7) < self.nonprintable
    }
}

enum Hasher {
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
}

impl Hasher {
    fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Sha1(hasher) => hasher.update(bytes),
            Self::Sha256(hasher) => hasher.update(bytes),
        }
    }

    fn matches(self, oid: &str) -> bool {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let digest = match self {
            Self::Sha1(hasher) => hasher.finalize().to_vec(),
            Self::Sha256(hasher) => hasher.finalize().to_vec(),
        };
        oid.len() == digest.len() * 2
            && digest
                .into_iter()
                .zip(oid.as_bytes().chunks(2))
                .all(|(byte, hex)| {
                    hex == [
                        DIGITS[usize::from(byte >> 4)],
                        DIGITS[usize::from(byte & 15)],
                    ]
                })
    }
}

/// Read the `size` bytes of `file` once and decide whether Git's clean under
/// `rule` stores exactly the blob `oid` of `blob_size` bytes.
///
/// Clean conversion only removes CRs, so a file as large as the blob must be
/// the blob itself and stay unchanged, and a larger file must lose exactly
/// `size - blob_size` CRs. The bytes hashed are the file's own in the first
/// case and the file with every CR before an LF removed in the second, which
/// is what every converting action stores for text content. The statistics
/// gathered on the way then decide whether Git really stores those bytes.
pub(super) fn cleans_to(
    mut file: impl Read,
    size: u64,
    blob_size: u64,
    oid: &str,
    rule: Rule,
    buffer: &mut [u8],
) -> io::Result<bool> {
    let identity = size == blob_size;
    // convert.c counts in 32 bits; leave larger converted files to Git.
    if size < blob_size || !identity && !rule.may_convert() || size > u64::from(u32::MAX) {
        return Ok(false);
    }
    let mut hasher = if oid.len() == 64 {
        Hasher::Sha256(sha2::Sha256::new())
    } else {
        Hasher::Sha1(sha1::Sha1::new())
    };
    hasher.update(format!("blob {blob_size}\0").as_bytes());
    let mut stats = Stats::default();
    let mut converted = Vec::new();
    let mut held = false;
    let mut emitted = 0;
    let mut remaining = size;
    while remaining > 0 {
        let length = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..length])?;
        let chunk = &buffer[..length];
        remaining -= length as u64;
        stats.update(chunk);
        if identity {
            hasher.update(chunk);
            continue;
        }
        converted.clear();
        for &byte in chunk {
            if std::mem::take(&mut held) && byte != b'\n' {
                converted.push(b'\r');
            }
            if byte == b'\r' {
                held = true;
            } else {
                converted.push(byte);
            }
        }
        if remaining == 0 && held {
            converted.push(b'\r');
        }
        emitted += converted.len() as u64;
        if emitted > blob_size {
            return Ok(false);
        }
        hasher.update(&converted);
    }
    stats.finish();
    Ok((identity || emitted == blob_size) && hasher.matches(oid) && rule.accepts(&stats, identity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path, process::Command};

    fn git(repo: &Path) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1");
        command
    }

    fn run(repo: &Path, args: &[&str], input: Option<&[u8]>) -> Vec<u8> {
        let mut command = git(repo);
        command.args(args);
        let output = if let Some(input) = input {
            use std::io::Write;
            let mut child = command
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(input).unwrap();
            child.wait_with_output().unwrap()
        } else {
            command.output().unwrap()
        };
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn attribute(value: &[u8]) -> Attribute {
        Attribute::parse(value)
    }

    /// Working-tree contents that exercise every branch of convert.c's
    /// statistics, binary detection and CRLF handling.
    fn samples() -> Vec<(&'static str, Vec<u8>)> {
        let text = |count: usize, suffix: &[u8]| [&vec![b'a'; count][..], suffix].concat();
        vec![
            ("empty", b"".to_vec()),
            ("no-eol", b"no line feed".to_vec()),
            ("lf", b"one\ntwo\n".to_vec()),
            ("crlf", b"one\r\ntwo\r\n".to_vec()),
            ("mixed", b"one\r\ntwo\n".to_vec()),
            ("lone-cr", b"one\rtwo\n".to_vec()),
            ("lone-cr-crlf", b"one\rtwo\r\n".to_vec()),
            ("double-cr", b"one\r\r\n".to_vec()),
            ("trailing-cr", b"one\n\r".to_vec()),
            ("only-cr", b"\r".to_vec()),
            ("nul-crlf", b"\0nul\r\n".to_vec()),
            ("nul-lf", b"\0nul\n".to_vec()),
            ("dos-eof", b"dos\r\n\x1a".to_vec()),
            ("controls", b"\x01ctl\r\n".to_vec()),
            ("printable-controls", b"\t\x08\x1b\x0c\r\n".to_vec()),
            ("threshold-text", text(128, b"\x01\r\n")),
            ("threshold-binary", text(127, b"\x01\r\n")),
            ("delete-binary", text(127, b"\x7f\r\n")),
            ("long-crlf", "line\r\n".repeat(30_000).into_bytes()),
        ]
    }

    /// Strip each CR before an LF: the blob a converting clean stores.
    fn lf(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for (at, &byte) in bytes.iter().enumerate() {
            if !(byte == b'\r' && bytes.get(at + 1) == Some(&b'\n')) {
                out.push(byte);
            }
        }
        out
    }

    /// Attribute sets by directory. `git check-attr` prints a boolean state
    /// and the literal string `set` or `unset` alike, so the model accepts a
    /// file under one of them only when Git stores the blob under both: the
    /// last field names the directory with the other spelling.
    const ATTRIBUTES: &[(&str, &str, Option<&str>)] = &[
        ("none", "", None),
        ("auto", "text=auto", None),
        ("input", "text=input", None),
        ("eol-lf", "eol=lf", None),
        ("eol-crlf", "eol=crlf", None),
        ("auto-crlf", "text=auto eol=crlf", None),
        ("auto-lf", "text=auto eol=lf", None),
        ("legacy-input", "crlf=input", None),
        ("text", "text", Some("text-string")),
        ("text-string", "text=set", Some("text")),
        ("text-crlf", "text eol=crlf", Some("text-string-crlf")),
        ("text-string-crlf", "text=set eol=crlf", Some("text-crlf")),
        ("no-text", "-text", Some("no-text-string")),
        ("no-text-string", "text=unset", Some("no-text")),
        ("binary", "binary", Some("no-text-string")),
        ("legacy-crlf", "crlf", Some("crlf-string")),
        ("crlf-string", "crlf=set", Some("legacy-crlf")),
        ("legacy-no-crlf", "-crlf", Some("no-crlf-string")),
        ("no-crlf-string", "crlf=unset", Some("legacy-no-crlf")),
    ];

    /// For each configuration, place every sample under every attribute set
    /// against two candidate blobs: the sample itself and the sample with its
    /// CRLFs converted. Git decides, from an index whose entries carry no
    /// stat data so it must read every file, which files `git status` calls
    /// clean and which ones `git add` stores unchanged; the model must agree
    /// on every one (see [`ATTRIBUTES`] for the two spellings of a state).
    #[test]
    fn clean_decisions_match_git() {
        let temp = tempfile::tempdir().unwrap();
        let configs: Vec<(&str, &str, &str)> = ["false", "true", "input"]
            .into_iter()
            .flat_map(|autocrlf| {
                ["lf", "crlf"].into_iter().flat_map(move |eol| {
                    ["false", "warn", "true"]
                        .into_iter()
                        .map(move |safe| (autocrlf, eol, safe))
                })
            })
            .collect();
        std::thread::scope(|scope| {
            for (number, &(autocrlf, eol, safe)) in configs.iter().enumerate() {
                let repo = temp.path().join(format!("repo-{number}"));
                scope.spawn(move || check_config(&repo, autocrlf, eol, safe));
            }
        });
    }

    fn check_config(repo: &Path, autocrlf: &str, eol: &str, safe: &str) {
        fs::create_dir(repo).unwrap();
        run(repo, &["init", "-q"], None);
        for (key, value) in [
            ("core.autocrlf", autocrlf),
            ("core.eol", eol),
            ("core.safecrlf", safe),
        ] {
            run(repo, &["config", key, value], None);
        }
        let settings = Settings {
            auto_crlf: match autocrlf {
                "true" => AutoCrlf::True,
                "input" => AutoCrlf::Input,
                _ => AutoCrlf::False,
            },
            eol_crlf: eol == "crlf",
            safe_crlf: safe == "true",
        };
        let mut gitattributes = String::new();
        for (name, attributes, _) in ATTRIBUTES {
            gitattributes.push_str(&format!("{name}/* {attributes}\n"));
        }
        fs::write(repo.join(".gitattributes"), &gitattributes).unwrap();
        // Every (directory, name, file bytes, blob bytes).
        let mut cases = Vec::new();
        for (directory, ..) in ATTRIBUTES {
            fs::create_dir(repo.join(directory)).unwrap();
            for (sample, bytes) in samples() {
                let converted = lf(&bytes);
                cases.push((*directory, sample.to_owned(), bytes.clone(), bytes.clone()));
                if converted != bytes {
                    cases.push((*directory, format!("{sample}.lf"), bytes, converted));
                }
            }
        }
        let cases: Vec<_> = cases
            .into_iter()
            .map(|(directory, name, file, blob)| (format!("{directory}/{name}"), file, blob))
            .collect();
        let mut index_info = Vec::new();
        let mut blobs = Vec::new();
        for (path, file, blob) in &cases {
            fs::write(repo.join(path), file).unwrap();
            let oid = String::from_utf8(run(
                repo,
                &["hash-object", "-w", "--no-filters", "--stdin"],
                Some(blob),
            ))
            .unwrap()
            .trim()
            .to_owned();
            index_info.extend_from_slice(format!("100644 {oid}\t{path}\0").as_bytes());
            blobs.push(oid);
        }
        run(
            repo,
            &["update-index", "-z", "--index-info"],
            Some(&index_info),
        );
        // The entries have no stat data, so status reads every file. It must
        // not record what it read, or `git add` would skip hashing below.
        let status = run(
            repo,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain",
                "-z",
                "--untracked-files=no",
            ],
            None,
        );
        let dirty: std::collections::HashSet<&[u8]> = status
            .split(|&b| b == 0)
            // Every entry is a staged addition; the second column is the
            // working tree's state.
            .filter(|record| record.len() > 3 && record[1] != b' ')
            .map(|record| &record[3..])
            .collect();
        let attributes = conversions(repo, cases.iter().map(|(path, ..)| path.as_str()));
        // A clean file is one Git stores as the blob; under
        // core.safecrlf=true, `git add` may still refuse it.
        let stored: std::collections::HashMap<&str, bool> = cases
            .iter()
            .map(|(path, ..)| {
                let clean = !dirty.contains(path.as_bytes());
                let added = || {
                    let output = git(repo).args(["add", "--", path]).output().unwrap();
                    output.status.success()
                };
                (path.as_str(), clean && (safe != "true" || added()))
            })
            .collect();
        for (((path, file, blob), oid), conversion) in cases.iter().zip(&blobs).zip(&attributes) {
            let (directory, name) = path.split_once('/').unwrap();
            let twin = ATTRIBUTES
                .iter()
                .find(|(candidate, ..)| *candidate == directory)
                .and_then(|(.., twin)| *twin);
            let expected = stored[path.as_str()]
                && twin.is_none_or(|twin| stored[format!("{twin}/{name}").as_str()]);
            let rule = settings.rule(conversion).unwrap();
            let mut buffer = [0; 7];
            let model = cleans_to(
                &file[..],
                file.len() as u64,
                blob.len() as u64,
                oid,
                rule,
                &mut buffer,
            )
            .unwrap();
            assert_eq!(
                model, expected,
                "autocrlf={autocrlf} eol={eol} safecrlf={safe} {path}"
            );
            if file == blob && rule.identity_is_clean() {
                assert!(
                    expected,
                    "identity must be clean: {autocrlf}/{eol}/{safe} {path}"
                );
            }
        }
    }

    fn conversions<'a>(repo: &Path, paths: impl Iterator<Item = &'a str>) -> Vec<Conversion> {
        let mut input = Vec::new();
        let paths: Vec<&str> = paths.collect();
        for path in &paths {
            input.extend_from_slice(path.as_bytes());
            input.push(0);
        }
        let output = run(
            repo,
            &["check-attr", "--all", "-z", "--stdin"],
            Some(&input),
        );
        let mut map: std::collections::HashMap<&[u8], Conversion> = Default::default();
        let mut fields = output.split(|&b| b == 0);
        while let (Some(path), Some(name), Some(value)) =
            (fields.next(), fields.next(), fields.next())
        {
            map.entry(path).or_default().set(name, value);
        }
        paths
            .iter()
            .map(|path| map.get(path.as_bytes()).copied().unwrap_or_default())
            .collect()
    }

    #[test]
    fn filters_have_no_rule() {
        let settings = Settings {
            auto_crlf: AutoCrlf::True,
            eol_crlf: false,
            safe_crlf: false,
        };
        let mut conversion = Conversion::default();
        conversion.set(b"filter", b"lfs");
        assert_eq!(settings.rule(&conversion), None);
        let mut conversion = Conversion::default();
        conversion.set(b"text", b"auto");
        assert_eq!(conversion.text, attribute(b"auto"));
        assert!(settings.rule(&conversion).unwrap().identity_is_clean());
        assert!(!Rule::NEVER.identity_is_clean());
        assert!(
            !cleans_to(
                &b""[..],
                0,
                0,
                "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
                Rule::NEVER,
                &mut [0; 8]
            )
            .unwrap()
        );
    }
}
