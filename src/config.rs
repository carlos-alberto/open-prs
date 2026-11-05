//! Where to look for PRs: the `--github` and `--forgejo` flags, else the config file, else
//! the PRs on github.com that involve you.

use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table};

/// The GitHub search when nothing else is set.
pub const DEFAULT_QUERY: &str = "involves:@me";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// The name the user gave, if any; see `name()`
    pub name: Option<String>,
    pub kind: Kind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// PRs that match a GitHub search, through gh
    GitHub {
        /// Search qualifiers, such as `org:rust-lang` or `repo:owner/name`
        query: String,
        /// For GitHub Enterprise; gh uses github.com without it
        hostname: Option<String>,
    },
    /// The open PRs a tea login can see, on Forgejo or Gitea
    Forgejo {
        /// A name from `tea logins list`
        login: String,
        /// Only the repos of this user or org
        owner: Option<String>,
    },
}

impl Source {
    pub fn github(query: &str, hostname: Option<String>, name: Option<String>) -> Self {
        Source {
            name,
            kind: Kind::GitHub {
                query: query.into(),
                hostname,
            },
        }
    }

    pub fn forgejo(login: &str, owner: Option<String>, name: Option<String>) -> Self {
        Source {
            name,
            kind: Kind::Forgejo {
                login: login.into(),
                owner,
            },
        }
    }

    /// Shown in the Host column: the name the user gave, else the GitHub hostname or the tea
    /// login. tea names a login after its host by default, so the login makes a good name.
    pub fn name(&self) -> &str {
        match (&self.name, &self.kind) {
            (Some(name), _) => name,
            (
                None,
                Kind::GitHub {
                    hostname: Some(hostname),
                    ..
                },
            ) => hostname,
            (None, Kind::GitHub { .. }) => "GitHub",
            (None, Kind::Forgejo { login, .. }) => login,
        }
    }

    /// What it searches, in a few words.
    pub fn describe(&self) -> String {
        match &self.kind {
            Kind::GitHub { query, hostname } => {
                format!("{}: {query}", hostname.as_deref().unwrap_or("github.com"))
            }
            Kind::Forgejo { login, owner: None } => format!("tea login {login}"),
            Kind::Forgejo {
                login,
                owner: Some(owner),
            } => format!("tea login {login}, {owner}'s repos"),
        }
    }
}

/// Where the sources came from, which says where changes to them go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The flags, so changes last until open_prs quits
    Flags,
    /// The config file, or the default when it doesn't exist yet; changes save to it
    File(PathBuf),
    /// The default, with no home folder to save changes in
    Nowhere,
}

#[derive(Clone, Debug)]
pub enum Change {
    Add(Source),
    Update(usize, Source),
    Remove(usize),
}

/// What the flags or the config file set.
#[derive(Debug)]
pub struct Config {
    pub sources: Vec<Source>,
    pub origin: Origin,
    /// Send desktop notifications; `notify = true` in the file
    pub notify: bool,
    /// Allow changes on the hosts, such as review requests; `writes = true` in the file
    pub writes: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    notify: bool,
    #[serde(default)]
    writes: bool,
    #[serde(default)]
    source: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum Entry {
    Github {
        name: Option<String>,
        query: Option<String>,
        hostname: Option<String>,
    },
    #[serde(alias = "gitea")]
    Forgejo {
        name: Option<String>,
        login: String,
        owner: Option<String>,
    },
}

/// `$XDG_CONFIG_HOME/open_prs/config.toml`, else `~/.config/open_prs/config.toml`.
pub fn path() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::home_dir().map(|home| home.join(".config")))?;
    Some(base.join("open_prs").join("config.toml"))
}

/// The sources to load, and the settings. The flags replace the config file, and with
/// neither, it searches github.com for `involves:@me`.
pub fn load(github: &[String], forgejo: &[String]) -> Result<Config> {
    let config = |sources, origin, (notify, writes)| Config {
        sources,
        origin,
        notify,
        writes,
    };
    if !github.is_empty() || !forgejo.is_empty() {
        let github = github.iter().map(|q| Source::github(q, None, None));
        let forgejo = forgejo.iter().map(|l| Source::forgejo(l, None, None));
        return Ok(config(
            github.chain(forgejo).collect(),
            Origin::Flags,
            (false, false),
        ));
    }
    let default = || vec![Source::github(DEFAULT_QUERY, None, None)];
    let Some(path) = path() else {
        return Ok(config(default(), Origin::Nowhere, (false, false)));
    };
    match fs::read_to_string(&path) {
        Ok(text) => {
            let file = read(&text).with_context(|| format!("{}", path.display()))?;
            let settings = (file.notify, file.writes);
            let sources = sources(file).with_context(|| format!("{}", path.display()))?;
            Ok(config(sources, Origin::File(path), settings))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            Ok(config(default(), Origin::File(path), (false, false)))
        }
        Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
    }
}

fn read(text: &str) -> Result<File> {
    Ok(toml::from_str(text)?)
}

fn parse(text: &str) -> Result<Vec<Source>> {
    sources(read(text)?)
}

fn sources(file: File) -> Result<Vec<Source>> {
    if file.source.is_empty() {
        bail!("there is no [[source]] in the file");
    }
    Ok(file
        .source
        .into_iter()
        .map(|entry| match entry {
            Entry::Github {
                name,
                query,
                hostname,
            } => Source::github(query.as_deref().unwrap_or(DEFAULT_QUERY), hostname, name),
            Entry::Forgejo { name, login, owner } => Source::forgejo(&login, owner, name),
        })
        .collect())
}

/// Makes `change` to the `[[source]]` tables in the file at `path`, which must still hold
/// `current`. It edits the tables in place, so the comments in the file stay.
pub fn save(path: &Path, current: &[Source], change: &Change) -> Result<()> {
    let (text, mut doc) = open(path)?;
    let new_file = text.trim().is_empty();
    // Don't write over changes made in an editor since open_prs read the file
    if !new_file && parse(&text).ok().as_deref() != Some(current) {
        bail!(
            "{} changed since open_prs read it; restart open_prs to use it",
            path.display()
        );
    }

    let tables = doc
        .as_table_mut()
        .entry("source")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .context("source in the config file isn't a list of [[source]] tables")?;
    if new_file {
        // The default source isn't in a file yet, so it goes in first
        for source in current {
            tables.push(table(source));
        }
    }
    match change {
        Change::Add(source) => tables.push(table(source)),
        Change::Update(i, source) => write(tables.get_mut(*i).context("no such source")?, source),
        Change::Remove(i) if *i < tables.len() => {
            tables.remove(*i);
        }
        Change::Remove(_) => bail!("no such source"),
    }
    write_file(path, &doc)
}

/// Sets `notify` in the file at `path`, and keeps the rest. A new file gets the sources in
/// `current` too, as a file needs one.
pub fn save_notify(path: &Path, current: &[Source], on: bool) -> Result<()> {
    let (text, mut doc) = open(path)?;
    if text.trim().is_empty() {
        let mut tables = ArrayOfTables::new();
        for source in current {
            tables.push(table(source));
        }
        doc.insert("source", Item::ArrayOfTables(tables));
    }
    match doc.get_mut("notify").and_then(Item::as_value_mut) {
        // A changed value keeps the comment after it
        Some(value) => {
            let decor = value.decor().clone();
            *value = on.into();
            *value.decor_mut() = decor;
        }
        None => {
            doc.insert("notify", toml_edit::value(on));
        }
    }
    write_file(path, &doc)
}

/// The file's text and document; empty when there is no file.
fn open(path: &Path) -> Result<(String, DocumentMut)> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
    };
    let doc = text
        .parse()
        .with_context(|| format!("{}", path.display()))?;
    Ok((text, doc))
}

fn write_file(path: &Path, doc: &DocumentMut) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("could not make {}", dir.display()))?;
    }
    // Write a new file and move it over the old one, so a crash can't leave half a file
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, doc.to_string())
        .with_context(|| format!("could not write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("could not replace {}", path.display()))
}

fn table(source: &Source) -> Table {
    let mut table = Table::new();
    write(&mut table, source);
    table
}

/// Puts `source` in `table`, and changes only the keys that differ.
fn write(table: &mut Table, source: &Source) {
    let (kind, keys) = match &source.kind {
        Kind::GitHub { query, hostname } => (
            "github",
            [
                ("query", Some(query.as_str())),
                ("hostname", hostname.as_deref()),
                ("login", None),
                ("owner", None),
            ],
        ),
        Kind::Forgejo { login, owner } => (
            "forgejo",
            [
                ("login", Some(login.as_str())),
                ("owner", owner.as_deref()),
                ("query", None),
                ("hostname", None),
            ],
        ),
    };
    // gitea means the same as forgejo, so a file that says gitea keeps it
    let same_kind = table
        .get("kind")
        .and_then(Item::as_str)
        .is_some_and(|k| k == kind || (kind == "forgejo" && k == "gitea"));
    if !same_kind {
        set(table, "kind", Some(kind));
    }
    set(table, "name", source.name.as_deref());
    for (key, value) in keys {
        set(table, key, value);
    }
}

/// Sets or removes a key. A changed value keeps the comment after it.
fn set(table: &mut Table, key: &str, new: Option<&str>) {
    let Some(new) = new else {
        table.remove(key);
        return;
    };
    if let Some(value) = table.get_mut(key).and_then(Item::as_value_mut) {
        if value.as_str() != Some(new) {
            let decor = value.decor().clone();
            *value = new.into();
            *value.decor_mut() = decor;
        }
        return;
    }
    table.insert(key, toml_edit::value(new));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_each_kind_with_its_defaults() {
        let sources = parse(
            r#"
            [[source]]
            kind = "github"
            query = "org:rust-lang"

            [[source]]
            kind = "github"
            hostname = "github.example.com"

            [[source]]
            kind = "forgejo"
            name = "Forgejo"
            login = "work"
            owner = "team"

            [[source]]
            kind = "gitea"
            login = "gitea.com"
            "#,
        )
        .unwrap();
        assert_eq!(
            sources,
            [
                Source::github("org:rust-lang", None, None),
                Source::github("involves:@me", Some("github.example.com".into()), None),
                Source::forgejo("work", Some("team".into()), Some("Forgejo".into())),
                Source::forgejo("gitea.com", None, None),
            ]
        );
        let names: Vec<_> = sources.iter().map(Source::name).collect();
        assert_eq!(
            names,
            ["GitHub", "github.example.com", "Forgejo", "gitea.com"]
        );
    }

    #[test]
    fn parse_rejects_mistakes() {
        let error = |text| format!("{:#}", parse(text).unwrap_err());
        assert!(error("").contains("no [[source]]"));
        assert!(error("[[source]]\nkind = \"gitlab\"").contains("unknown variant `gitlab`"));
        assert!(error("[[source]]\nkind = \"forgejo\"").contains("missing field `login`"));
        assert!(
            error("[[source]]\nkind = \"github\"\nqeury = \"x\"").contains("unknown field `qeury`")
        );
        assert!(error("[[sources]]\nkind = \"github\"").contains("unknown field `sources`"));
    }

    #[test]
    fn flags_replace_the_file() {
        let config = load(&["org:a".into()], &["work".into()]).unwrap();
        assert_eq!(
            config.sources,
            [
                Source::github("org:a", None, None),
                Source::forgejo("work", None, None)
            ]
        );
        assert_eq!(config.origin, Origin::Flags);
        assert!(!config.notify);
        assert!(!config.writes);
    }

    #[test]
    fn writes_are_off_unless_the_file_turns_them_on() {
        assert!(!read(FILE).unwrap().writes);
        let text = format!("writes = true\n{FILE}");
        assert!(read(&text).unwrap().writes);
        assert_eq!(parse(&text).unwrap(), parse(FILE).unwrap());
        let wrong = format!("writes = \"yes\"\n{FILE}");
        assert!(read(&wrong).is_err());
    }

    #[test]
    fn save_notify_sets_the_key_and_keeps_the_rest() {
        let file = TempFile::new("notify", Some(FILE));
        let current = parse(FILE).unwrap();
        assert!(
            !read(FILE).unwrap().notify,
            "notifications are off by default"
        );

        save_notify(&file.0, &current, true).unwrap();
        let text = file.read();
        assert!(text.starts_with("notify = true\n"), "{text}");
        assert!(text.contains("# My sources") && text.contains("query = \"org:acme\"  # work"));
        assert!(read(&text).unwrap().notify);
        assert_eq!(parse(&text).unwrap(), current);

        // Off again, with the comment after it
        fs::write(
            &file.0,
            text.replace("notify = true", "notify = true  # on"),
        )
        .unwrap();
        save_notify(&file.0, &current, false).unwrap();
        assert!(
            file.read().starts_with("notify = false  # on\n"),
            "{}",
            file.read()
        );
    }

    #[test]
    fn save_notify_writes_the_sources_into_a_new_file() {
        let file = TempFile::new("notify-new", None);
        let current = vec![Source::github(DEFAULT_QUERY, None, None)];
        save_notify(&file.0, &current, true).unwrap();
        let text = file.read();
        assert!(read(&text).unwrap().notify);
        assert_eq!(parse(&text).unwrap(), current);
    }

    /// A config file in its own temporary folder, removed when the test ends.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str, text: Option<&str>) -> Self {
            let dir = env::temp_dir().join(format!("open_prs-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("config.toml");
            if let Some(text) = text {
                fs::write(&path, text).unwrap();
            }
            TempFile(path)
        }

        fn read(&self) -> String {
            fs::read_to_string(&self.0).unwrap()
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    const FILE: &str = r#"# My sources
[[source]]
kind = "github"
query = "org:acme"  # work

# The Forgejo at home
[[source]]
kind = "gitea"
login = "home"
owner = "me"
"#;

    #[test]
    fn save_edits_in_place_and_keeps_comments() {
        let file = TempFile::new("edit", Some(FILE));
        let mut current = parse(FILE).unwrap();

        let edited = Source::github("org:acme2", None, Some("Work".into()));
        save(&file.0, &current, &Change::Update(0, edited.clone())).unwrap();
        current[0] = edited;
        let gitea = Source::forgejo("home", None, None);
        save(&file.0, &current, &Change::Update(1, gitea.clone())).unwrap();
        current[1] = gitea;
        assert_eq!(
            file.read(),
            r#"# My sources
[[source]]
kind = "github"
query = "org:acme2"  # work
name = "Work"

# The Forgejo at home
[[source]]
kind = "gitea"
login = "home"
"#
        );
        assert_eq!(parse(&file.read()).unwrap(), current);
    }

    #[test]
    fn save_adds_and_removes_tables() {
        let file = TempFile::new("add", Some(FILE));
        let mut current = parse(FILE).unwrap();

        let added = Source::forgejo("codeberg.org", Some("forgejo".into()), None);
        save(&file.0, &current, &Change::Add(added.clone())).unwrap();
        current.push(added);
        assert_eq!(parse(&file.read()).unwrap(), current);

        save(&file.0, &current, &Change::Remove(0)).unwrap();
        current.remove(0);
        assert_eq!(parse(&file.read()).unwrap(), current);
        assert!(file.read().contains("# The Forgejo at home"));
    }

    #[test]
    fn save_writes_the_default_source_into_a_new_file() {
        let file = TempFile::new("new", None);
        let current = vec![Source::github(DEFAULT_QUERY, None, None)];
        let added = Source::forgejo("home", None, None);
        save(&file.0, &current, &Change::Add(added.clone())).unwrap();
        assert_eq!(parse(&file.read()).unwrap(), [current[0].clone(), added]);
    }

    #[test]
    fn save_refuses_when_the_file_changed() {
        let file = TempFile::new("changed", Some(FILE));
        let stale = vec![Source::github("org:other", None, None)];
        let error = save(&file.0, &stale, &Change::Remove(0)).unwrap_err();
        assert!(format!("{error:#}").contains("changed since open_prs read it"));
        assert_eq!(file.read(), FILE);
    }
}
