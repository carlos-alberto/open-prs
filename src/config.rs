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
    /// PRs that match a GitHub search
    GitHub {
        /// Search qualifiers, such as `org:rust-lang` or `repo:owner/name`
        query: String,
        /// For GitHub Enterprise; without it, github.com
        hostname: Option<String>,
        /// Its own token; without one, it uses gh's login
        token: Option<Token>,
    },
    /// The open PRs on Forgejo or Gitea
    Forgejo {
        login: Login,
        /// Only the repos of this user or org
        owner: Option<String>,
    },
}

/// How a Forgejo source logs in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Login {
    /// A name from `tea logins list`: tea calls the host, with all the rights of that login
    Tea(String),
    /// The API of the Forgejo at `url`, with a token of its own
    Token {
        /// Such as https://git.example.com, without a / at the end
        url: String,
        /// The user allows an http `url`, as `allow_http = true`
        allow_http: bool,
        token: Token,
    },
}

/// Where a source's token is, besides the keychain; see `token::find`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Token {
    /// A program that prints the token, such as `op read op://Work/GitHub/token`
    pub command: Option<String>,
    /// The environment variable that holds the token
    pub env: Option<String>,
}

impl Source {
    /// A GitHub search through gh's login.
    pub fn github(query: &str, hostname: Option<String>, name: Option<String>) -> Self {
        Source {
            name,
            kind: Kind::GitHub {
                query: query.into(),
                hostname,
                token: None,
            },
        }
    }

    /// A Forgejo or Gitea through a tea login.
    pub fn forgejo(login: &str, owner: Option<String>, name: Option<String>) -> Self {
        Source {
            name,
            kind: Kind::Forgejo {
                login: Login::Tea(login.into()),
                owner,
            },
        }
    }

    /// The Forgejo or Gitea at `url`, with a token from the keychain.
    #[cfg(test)]
    pub fn forgejo_api(url: &str, owner: Option<String>, name: Option<String>) -> Self {
        Source {
            name,
            kind: Kind::Forgejo {
                login: Login::Token {
                    url: url.into(),
                    allow_http: false,
                    token: Token::default(),
                },
                owner,
            },
        }
    }

    /// The same GitHub source with a token of its own, from `token`.
    pub fn with_token(mut self, new: Token) -> Self {
        if let Kind::GitHub { token, .. } = &mut self.kind {
            *token = Some(new);
        }
        self
    }

    /// Shown in the Host column: the name the user gave, else the GitHub hostname, the tea
    /// login or the Forgejo's host. tea names a login after its host by default, so the login
    /// makes a good name.
    pub fn name(&self) -> &str {
        if let Some(name) = &self.name {
            return name;
        }
        match &self.kind {
            Kind::GitHub {
                hostname: Some(hostname),
                ..
            } => hostname,
            Kind::GitHub { .. } => "GitHub",
            Kind::Forgejo {
                login: Login::Tea(login),
                ..
            } => login,
            Kind::Forgejo {
                login: Login::Token { url, .. },
                ..
            } => url_host(url),
        }
    }

    /// The host it calls; for a tea login, the login's name.
    pub fn host(&self) -> &str {
        match &self.kind {
            Kind::GitHub { hostname, .. } => hostname.as_deref().unwrap_or("github.com"),
            Kind::Forgejo {
                login: Login::Tea(login),
                ..
            } => login,
            Kind::Forgejo {
                login: Login::Token { url, .. },
                ..
            } => url_host(url),
        }
    }

    /// What it searches, in a few words.
    pub fn describe(&self) -> String {
        match &self.kind {
            Kind::GitHub { query, .. } => format!("{}: {query}", self.host()),
            Kind::Forgejo { login, owner } => {
                let at = match login {
                    Login::Tea(login) => format!("tea login {login}"),
                    Login::Token { .. } => self.host().to_owned(),
                };
                match owner {
                    Some(owner) => format!("{at}, {owner}'s repos"),
                    None => at,
                }
            }
        }
    }

    /// Where its token is, or None when it uses gh's or tea's login.
    pub fn token(&self) -> Option<&Token> {
        match &self.kind {
            Kind::GitHub { token, .. } => token.as_ref(),
            Kind::Forgejo {
                login: Login::Token { token, .. },
                ..
            } => Some(token),
            Kind::Forgejo { .. } => None,
        }
    }

    /// The program whose login it uses, gh or tea, or None when it has a token of its own.
    pub fn cli(&self) -> Option<&'static str> {
        match &self.kind {
            Kind::GitHub { token: None, .. } => Some("gh"),
            Kind::Forgejo {
                login: Login::Tea(_),
                ..
            } => Some("tea"),
            _ => None,
        }
    }

    /// Its token's account in the keychain: `<kind>:<host>:<name>`.
    pub fn account(&self) -> String {
        let kind = match self.kind {
            Kind::GitHub { .. } => "github",
            Kind::Forgejo { .. } => "forgejo",
        };
        format!("{kind}:{}:{}", self.host(), self.name())
    }
}

/// The host of `url`, with any port: git.example.com in https://git.example.com/forgejo.
pub fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

/// `url` without a / at the end, if it is a web address that open_prs may send a token to:
/// https, or http when the user allows it.
pub fn check_url(url: &str, allow_http: bool) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    let lower = url.to_ascii_lowercase();
    let host = url_host(url);
    if !lower.starts_with("https://") && !lower.starts_with("http://") {
        bail!("{url} isn't a web address, such as https://git.example.com");
    }
    if lower.starts_with("http://") && !allow_http {
        bail!(
            "{url} uses http, which sends the token as plain text; use https, or put \
             allow_http = true on the source"
        );
    }
    if host.is_empty() || host.contains(['@', ' ', '\\']) {
        bail!("{url} has no host that open_prs can use");
    }
    Ok(url.to_owned())
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
        auth: Option<Auth>,
        token_command: Option<String>,
        token_env: Option<String>,
    },
    #[serde(alias = "gitea")]
    Forgejo {
        name: Option<String>,
        login: Option<String>,
        url: Option<String>,
        owner: Option<String>,
        auth: Option<Auth>,
        token_command: Option<String>,
        token_env: Option<String>,
        allow_http: Option<bool>,
    },
}

/// The `auth` key: how a source logs in. Without it, a source uses gh or tea.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Auth {
    /// A token of its own
    Token,
    Gh,
    Tea,
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
    file.source
        .into_iter()
        .zip(1..)
        .map(|(entry, i)| source(entry).with_context(|| format!("[[source]] number {i}")))
        .collect()
}

fn source(entry: Entry) -> Result<Source> {
    // The token keys mean nothing to gh or tea, so they must not look as if they work
    let token = |auth: Option<Auth>, command: Option<String>, env: Option<String>| {
        if auth != Some(Auth::Token) && (command.is_some() || env.is_some()) {
            bail!("token_command and token_env need auth = \"token\"");
        }
        Ok(Token { command, env })
    };
    match entry {
        Entry::Github {
            name,
            query,
            hostname,
            auth,
            token_command,
            token_env,
        } => {
            let found = token(auth, token_command, token_env)?;
            let token = match auth {
                None | Some(Auth::Gh) => None,
                Some(Auth::Token) => Some(found),
                Some(Auth::Tea) => bail!("a github source has auth = \"token\" or \"gh\""),
            };
            let query = query.unwrap_or_else(|| DEFAULT_QUERY.into());
            Ok(Source {
                name,
                kind: Kind::GitHub {
                    query,
                    hostname,
                    token,
                },
            })
        }
        Entry::Forgejo {
            name,
            login,
            url,
            owner,
            auth,
            token_command,
            token_env,
            allow_http,
        } => {
            let found = token(auth, token_command, token_env)?;
            let login = match auth {
                None | Some(Auth::Tea) => {
                    if url.is_some() || allow_http.is_some() {
                        bail!(
                            "url and allow_http need auth = \"token\"; tea knows its login's host"
                        );
                    }
                    let login = login.context(
                        "a forgejo source needs login, a name from `tea logins list`, or \
                         auth = \"token\" and url",
                    )?;
                    Login::Tea(login)
                }
                Some(Auth::Token) => {
                    if login.is_some() {
                        bail!("login is for tea; with auth = \"token\", give url");
                    }
                    let url = url.context(
                        "a forgejo source with auth = \"token\" needs url, such as \
                         https://git.example.com",
                    )?;
                    let allow_http = allow_http.unwrap_or(false);
                    Login::Token {
                        url: check_url(&url, allow_http)?,
                        allow_http,
                        token: found,
                    }
                }
                Some(Auth::Gh) => bail!("a forgejo source has auth = \"token\" or \"tea\""),
            };
            Ok(Source {
                name,
                kind: Kind::Forgejo { login, owner },
            })
        }
    }
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
    let none = Token::default();
    let token = source.token().unwrap_or(&none);
    // A source that uses gh or tea keeps an auth key that says so, and gets none when it has none
    let auth = match source.cli() {
        Some(cli) => table
            .get("auth")
            .and_then(Item::as_str)
            .filter(|a| *a == cli)
            .map(|_| cli),
        None => Some("token"),
    };
    let (kind, keys) = match &source.kind {
        Kind::GitHub {
            query, hostname, ..
        } => (
            "github",
            [
                ("query", Some(query.as_str())),
                ("hostname", hostname.as_deref()),
                ("login", None),
                ("url", None),
                ("owner", None),
            ],
        ),
        Kind::Forgejo { login, owner } => {
            let (login, url) = match login {
                Login::Tea(login) => (Some(login.as_str()), None),
                Login::Token { url, .. } => (None, Some(url.as_str())),
            };
            (
                "forgejo",
                [
                    ("login", login),
                    ("url", url),
                    ("owner", owner.as_deref()),
                    ("query", None),
                    ("hostname", None),
                ],
            )
        }
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
    set(table, "auth", auth);
    set(table, "token_command", token.command.as_deref());
    set(table, "token_env", token.env.as_deref());
    let allow_http = matches!(
        &source.kind,
        Kind::Forgejo {
            login: Login::Token {
                allow_http: true,
                ..
            },
            ..
        }
    );
    if allow_http {
        if table.get("allow_http").and_then(Item::as_bool) != Some(true) {
            table.insert("allow_http", toml_edit::value(true));
        }
    } else {
        table.remove("allow_http");
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
        assert!(error("[[source]]\nkind = \"forgejo\"").contains("needs login"));
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

    #[test]
    fn parse_reads_the_token_keys() {
        let sources = parse(
            r#"
            [[source]]
            kind = "github"
            query = "org:acme"
            auth = "token"
            token_command = "op read op://Work/GitHub/token"

            [[source]]
            kind = "github"
            auth = "gh"

            [[source]]
            kind = "forgejo"
            url = "http://git.example.com:3000/"
            owner = "acme"
            auth = "token"
            token_env = "FORGEJO_TOKEN"
            allow_http = true
            "#,
        )
        .unwrap();
        let command = Token {
            command: Some("op read op://Work/GitHub/token".into()),
            env: None,
        };
        assert_eq!(
            sources[0],
            Source::github("org:acme", None, None).with_token(command)
        );
        assert_eq!(sources[1], Source::github(DEFAULT_QUERY, None, None));
        assert_eq!(sources[1].cli(), Some("gh"));
        assert_eq!(
            sources[2].kind,
            Kind::Forgejo {
                login: Login::Token {
                    url: "http://git.example.com:3000".into(),
                    allow_http: true,
                    token: Token {
                        command: None,
                        env: Some("FORGEJO_TOKEN".into()),
                    },
                },
                owner: Some("acme".into()),
            }
        );
        assert_eq!(sources[2].name(), "git.example.com:3000");
        assert_eq!(
            sources[2].account(),
            "forgejo:git.example.com:3000:git.example.com:3000"
        );
        assert_eq!(sources[0].account(), "github:github.com:GitHub");
    }

    #[test]
    fn parse_rejects_token_keys_that_would_do_nothing() {
        let error = |text: &str| format!("{:#}", parse(text).unwrap_err());
        let forgejo = "[[source]]\nkind = \"forgejo\"\n";
        assert!(error(&format!("{forgejo}auth = \"token\"")).contains("needs url"));
        assert!(
            error(&format!("{forgejo}auth = \"token\"\nurl = \"http://x\""))
                .contains("put allow_http = true on the source")
        );
        assert!(
            error(&format!("{forgejo}login = \"home\"\nurl = \"https://x\""))
                .contains("url and allow_http need auth = \"token\"")
        );
        assert!(
            error(&format!(
                "{forgejo}auth = \"token\"\nurl = \"https://x\"\nlogin = \"home\""
            ))
            .contains("login is for tea")
        );
        assert!(
            error("[[source]]\nkind = \"github\"\ntoken_env = \"T\"")
                .contains("[[source]] number 1: token_command and token_env need auth = \"token\"")
        );
        assert!(
            error("[[source]]\nkind = \"github\"\nauth = \"tea\"").contains("\"token\" or \"gh\"")
        );
        assert!(
            error("[[source]]\nkind = \"github\"\nauth = \"pat\"")
                .contains("unknown variant `pat`")
        );
    }

    #[test]
    fn save_adds_the_token_keys_and_keeps_comments() {
        let file = TempFile::new("token", Some(FILE));
        let mut current = parse(FILE).unwrap();

        // As the form makes them: a token for GitHub, and the Forgejo through its API
        let github = current[0].clone().with_token(Token::default());
        save(&file.0, &current, &Change::Update(0, github.clone())).unwrap();
        current[0] = github;
        let forgejo = Source::forgejo_api("https://git.example.com", Some("me".into()), None);
        save(&file.0, &current, &Change::Update(1, forgejo.clone())).unwrap();
        current[1] = forgejo;
        assert_eq!(
            file.read(),
            r#"# My sources
[[source]]
kind = "github"
query = "org:acme"  # work
auth = "token"

# The Forgejo at home
[[source]]
kind = "gitea"
owner = "me"
url = "https://git.example.com"
auth = "token"
"#
        );
        assert_eq!(parse(&file.read()).unwrap(), current);

        // Back to gh: the auth key goes, and a token_env that the file had goes with it
        let back = Source::github("org:acme", None, None);
        save(&file.0, &current, &Change::Update(0, back.clone())).unwrap();
        current[0] = back;
        assert!(!file.read().contains("auth = \"token\"\n\n# The Forgejo"));
        assert_eq!(parse(&file.read()).unwrap(), current);
    }

    #[test]
    fn save_keeps_an_auth_key_that_names_gh_or_tea() {
        let text = format!("{FILE}auth = \"tea\"  # the old way\n");
        let file = TempFile::new("auth", Some(&text));
        let current = parse(&text).unwrap();
        let renamed = Source::forgejo("home", Some("me".into()), Some("Home".into()));
        save(&file.0, &current, &Change::Update(1, renamed)).unwrap();
        assert!(file.read().contains("auth = \"tea\"  # the old way\n"));
    }
}
