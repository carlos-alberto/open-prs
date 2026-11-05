//! The panel to see, add, edit and delete sources, and its form.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Padding, Paragraph, Row, Table, TableState};

use crate::config::{DEFAULT_QUERY, Kind, Login, Source, Token, check_url};
use crate::sources::encode;

/// The list of sources, over the table.
#[derive(Default)]
pub struct List {
    pub cursor: usize,
    /// Waiting for y to delete the source under the cursor
    pub confirm: bool,
    /// A deleted source whose token is in the keychain: y deletes the token too
    pub forget: Option<Source>,
    /// What the last change did, or why it didn't happen
    pub message: Option<String>,
}

/// One line of the list.
pub struct ListRow {
    pub name: String,
    pub what: String,
    pub state: String,
    pub state_style: Style,
    /// Why the last load failed
    pub error: Option<String>,
    /// gh or tea, when it uses their login and not a token of its own
    pub cli: Option<&'static str>,
}

pub fn draw_list(frame: &mut Frame, area: Rect, list: &List, rows: &[ListRow], note: &str) {
    let help = if let Some(source) = &list.forget {
        Line::from(format!(
            "Delete the token of {} from the keychain too? y deletes it, any other key keeps it",
            source.name()
        ))
        .red()
        .bold()
    } else if list.confirm {
        let name = rows.get(list.cursor).map_or("", |r| r.name.as_str());
        Line::from(format!(
            "Delete {name}? y deletes it, any other key keeps it"
        ))
        .red()
        .bold()
    } else {
        keys(&[
            ("a", "Add"),
            ("e", "Edit"),
            ("d", "Delete"),
            ("esc", "Close"),
        ])
    };
    let mut lines = vec![Line::from(note.to_owned()).dim()];
    if let Some(row) = rows.get(list.cursor)
        && let Some(cli) = row.cli
    {
        lines.push(
            Line::from(format!(
                "{} uses {cli}'s login, with all its rights",
                row.name
            ))
            .dim(),
        );
    }
    // The first line of the error of the source under the cursor
    if let Some(error) = rows.get(list.cursor).and_then(|r| r.error.as_deref()) {
        let first = error.lines().next().unwrap_or_default();
        lines.push(Line::from(format!("{} failed: {first}", rows[list.cursor].name)).red());
    }
    if let Some(message) = &list.message {
        lines.push(Line::from(message.clone()).red());
    }
    lines.push(help);

    let height = rows.len() as u16 + 1 + lines.len() as u16 + 3;
    let rect = popup(area, 100, height);
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .title(" Sources ".bold())
        .padding(Padding::horizontal(1));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let [table_area, _, text_area] = inner.layout(&Layout::vertical([
        Constraint::Length(rows.len() as u16 + 1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ]));
    let widest = |f: fn(&ListRow) -> &str, header: &str| {
        rows.iter()
            .map(|r| Span::raw(f(r)).width())
            .max()
            .unwrap_or(0)
            .max(header.len()) as u16
    };
    let widths = [
        Constraint::Length(widest(|r| &r.name, "Name")),
        Constraint::Fill(1),
        Constraint::Length(5),
        Constraint::Length(widest(|r| &r.state, "State")),
    ];
    let table = Table::new(
        rows.iter().map(|r| {
            Row::new([
                Cell::from(r.name.clone()),
                Cell::from(r.what.clone()),
                Cell::from(r.cli.unwrap_or("token")),
                Cell::from(Span::styled(r.state.clone(), r.state_style)),
            ])
        }),
        widths,
    )
    .header(Row::new(["Name", "Searches", "Auth", "State"]).bold())
    .column_spacing(2)
    .row_highlight_style(Style::new().bg(Color::DarkGray).bold());
    let mut state = TableState::new().with_selected(Some(list.cursor));
    frame.render_stateful_widget(table, table_area, &mut state);
    frame.render_widget(Paragraph::new(lines), text_area);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Kind,
    /// A token of its own, or gh's or tea's login
    Auth,
    Name,
    Query,
    Hostname,
    Login,
    Url,
    Owner,
    Token,
}

impl Field {
    fn label(self) -> &'static str {
        match self {
            Field::Kind => "Kind",
            Field::Auth => "Auth",
            Field::Name => "Name",
            Field::Query => "Query",
            Field::Hostname => "Hostname",
            Field::Login => "Login",
            Field::Url => "URL",
            Field::Owner => "Owner",
            Field::Token => "Token",
        }
    }

    /// It is a choice of two, which ← → and space change.
    fn choice(self) -> bool {
        matches!(self, Field::Kind | Field::Auth)
    }
}

/// What a token field shows in place of the token, whatever its length.
pub const HIDDEN: &str = "••••";

/// The form to add a source or edit one.
pub struct Form {
    /// The index of the source it edits, or None to add one
    pub editing: Option<usize>,
    github: bool,
    /// It uses a token of its own, not gh's or tea's login
    token_auth: bool,
    name: String,
    query: String,
    hostname: String,
    login: String,
    url: String,
    owner: String,
    /// A new token, typed or pasted; it never shows
    token: String,
    /// The keychain has a token for the source it edits
    saved: bool,
    /// What the form keeps but doesn't show: token_command and token_env, from the file
    kept: Token,
    /// allow_http, from the file
    allow_http: bool,
    focus: Field,
    pub error: Option<String>,
    /// From `tea logins list`, to show which logins there are
    logins: Vec<String>,
    /// Writes are on, so a new token may need write access
    writes: bool,
}

pub enum FormAction {
    None,
    Cancel,
    /// The source, and a new token for the keychain if one was typed
    Save(Source, Option<String>),
    /// Open this page, which makes a token, in the browser
    Open(String),
}

impl Form {
    /// A form for a new source, which has a token of its own unless the user picks gh or tea.
    pub fn add(logins: Vec<String>) -> Self {
        Form {
            editing: None,
            github: true,
            token_auth: true,
            name: String::new(),
            query: String::new(),
            hostname: String::new(),
            login: String::new(),
            url: String::new(),
            owner: String::new(),
            token: String::new(),
            saved: false,
            kept: Token::default(),
            allow_http: false,
            focus: Field::Kind,
            error: None,
            logins,
            writes: false,
        }
    }

    /// The same form, for when writes are on or off.
    pub fn with_writes(mut self, writes: bool) -> Self {
        self.writes = writes;
        self
    }

    /// A form for `source`; `saved` says if the keychain has its token.
    pub fn edit(index: usize, source: &Source, logins: Vec<String>, saved: bool) -> Self {
        let mut form = Form::add(logins);
        form.editing = Some(index);
        form.name = source.name.clone().unwrap_or_default();
        form.token_auth = source.token().is_some();
        form.kept = source.token().cloned().unwrap_or_default();
        form.saved = saved;
        match &source.kind {
            Kind::GitHub {
                query, hostname, ..
            } => {
                form.query = query.clone();
                form.hostname = hostname.clone().unwrap_or_default();
            }
            Kind::Forgejo { login, owner } => {
                form.github = false;
                match login {
                    Login::Tea(login) => form.login = login.clone(),
                    Login::Token {
                        url, allow_http, ..
                    } => {
                        form.url = url.clone();
                        form.allow_http = *allow_http;
                    }
                }
                form.owner = owner.clone().unwrap_or_default();
            }
        }
        form.focus = Field::Name;
        form
    }

    fn fields(&self) -> Vec<Field> {
        let mut fields = vec![Field::Kind, Field::Auth, Field::Name];
        fields.extend(match (self.github, self.token_auth) {
            (true, _) => [Field::Query, Field::Hostname],
            (false, true) => [Field::Url, Field::Owner],
            (false, false) => [Field::Login, Field::Owner],
        });
        if self.token_auth {
            fields.push(Field::Token);
        }
        fields
    }

    fn text(&self, field: Field) -> &str {
        match field {
            Field::Kind | Field::Auth => "",
            Field::Name => &self.name,
            Field::Query => &self.query,
            Field::Hostname => &self.hostname,
            Field::Login => &self.login,
            Field::Url => &self.url,
            Field::Owner => &self.owner,
            Field::Token => &self.token,
        }
    }

    fn text_mut(&mut self) -> Option<&mut String> {
        match self.focus {
            Field::Kind | Field::Auth => None,
            Field::Name => Some(&mut self.name),
            Field::Query => Some(&mut self.query),
            Field::Hostname => Some(&mut self.hostname),
            Field::Login => Some(&mut self.login),
            Field::Url => Some(&mut self.url),
            Field::Owner => Some(&mut self.owner),
            Field::Token => Some(&mut self.token),
        }
    }

    /// gh or tea, for this kind.
    fn cli(&self) -> &'static str {
        if self.github { "gh" } else { "tea" }
    }

    /// What an empty field means.
    fn placeholder(&self, field: Field) -> String {
        match field {
            Field::Kind | Field::Auth => String::new(),
            Field::Name => self
                .build()
                .map_or_else(|_| "the host".into(), |s| s.name().to_owned()),
            Field::Query => DEFAULT_QUERY.into(),
            Field::Hostname => "github.com".into(),
            Field::Login => "a name from `tea logins list`".into(),
            Field::Url => "such as https://git.example.com".into(),
            Field::Owner => "any".into(),
            Field::Token if self.saved => "in the keychain; type or paste to replace it".into(),
            Field::Token if self.kept != Token::default() => {
                "from token_command or token_env in the config file".into()
            }
            Field::Token => "paste a token; it goes in the keychain".into(),
        }
    }

    fn move_focus(&mut self, by: isize) {
        let fields = self.fields();
        let i = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        let n = fields.len() as isize;
        self.focus = fields[((i as isize + by).rem_euclid(n)) as usize];
    }

    pub fn on_key(&mut self, key: KeyEvent) -> FormAction {
        // The error was about the form as it was
        self.error = None;
        match key.code {
            KeyCode::Esc => return FormAction::Cancel,
            KeyCode::Char('o')
                if key.modifiers.contains(KeyModifiers::CONTROL) && self.token_auth =>
            {
                match self.token_page() {
                    Ok(url) => {
                        // Ready to paste the token, with the list of what it needs in view
                        self.focus = Field::Token;
                        return FormAction::Open(url);
                    }
                    Err((error, field)) => {
                        self.error = Some(error);
                        self.focus = field;
                    }
                }
            }
            KeyCode::Enter => return self.submit(),
            KeyCode::Tab | KeyCode::Down => self.move_focus(1),
            KeyCode::BackTab | KeyCode::Up => self.move_focus(-1),
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if self.focus.choice() => {
                if self.focus == Field::Kind {
                    self.github = !self.github;
                } else {
                    self.token_auth = !self.token_auth;
                }
            }
            KeyCode::Backspace => {
                if let Some(text) = self.text_mut() {
                    text.pop();
                }
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(text) = self.text_mut() {
                    text.push(c);
                }
            }
            _ => {}
        }
        FormAction::None
    }

    /// The source the form holds now, or what it needs and the field for it.
    fn source(&self) -> Result<Source, (String, Field)> {
        let source = self.build()?;
        let has_token =
            !self.token.trim().is_empty() || self.saved || self.kept != Token::default();
        if self.token_auth && !has_token {
            let message = format!("Paste a token, or pick {} for Auth", self.cli());
            return Err((message, Field::Token));
        }
        Ok(source)
    }

    /// The source in the fields, whether or not it has a token to go with it.
    fn build(&self) -> Result<Source, (String, Field)> {
        let given = |s: &str| Some(s.trim().to_owned()).filter(|s| !s.is_empty());
        let name = given(&self.name);
        let owner = given(&self.owner);
        let source = match (self.github, self.token_auth) {
            (true, token_auth) => {
                let query = given(&self.query).unwrap_or_else(|| DEFAULT_QUERY.into());
                let source = Source::github(&query, given(&self.hostname), name);
                if token_auth {
                    source.with_token(self.kept.clone())
                } else {
                    source
                }
            }
            (false, false) => {
                let login = given(&self.login).ok_or(("Give a tea login".into(), Field::Login))?;
                Source::forgejo(&login, owner, name)
            }
            (false, true) => {
                let url = given(&self.url).ok_or((
                    "Give the URL of the Forgejo or Gitea, such as https://git.example.com".into(),
                    Field::Url,
                ))?;
                let url = check_url(&url, self.allow_http)
                    .map_err(|e| (cap(&format!("{e:#}")), Field::Url))?;
                Source {
                    name,
                    kind: Kind::Forgejo {
                        login: Login::Token {
                            url,
                            allow_http: self.allow_http,
                            token: self.kept.clone(),
                        },
                        owner,
                    },
                }
            }
        };
        Ok(source)
    }

    /// The host's page that makes a token. GitHub's has the name, the owner from the query and
    /// the permissions filled in; a write permission only while writes are on.
    fn token_page(&self) -> Result<String, (String, Field)> {
        if !self.github {
            let source = self.build()?;
            let Kind::Forgejo {
                login: Login::Token { url, .. },
                ..
            } = &source.kind
            else {
                return Err(("Give the URL of the Forgejo or Gitea".into(), Field::Url));
            };
            return Ok(format!("{url}/user/settings/applications"));
        }
        let host = Some(self.hostname.trim())
            .filter(|h| !h.is_empty())
            .unwrap_or("github.com");
        let mut url = format!(
            "https://{host}/settings/personal-access-tokens/new?name=open_prs&description={}",
            encode("PRs, reviews and CI for open_prs")
        );
        for (_, param, level) in self.github_permissions() {
            url.push_str(&format!("&{param}={level}"));
        }
        if let Some(owner) = query_owner(&self.query) {
            url.push_str(&format!("&target_name={}", encode(owner)));
        }
        Ok(url)
    }

    /// The repository permissions a GitHub token needs: the name on GitHub's page, the name in
    /// the page's URL, and the level. It may change PRs only while writes are on.
    fn github_permissions(&self) -> [(&'static str, &'static str, &'static str); 3] {
        let pull_requests = if self.writes { "write" } else { "read" };
        [
            ("Pull requests", "pull_requests", pull_requests),
            ("Commit statuses", "statuses", "read"),
            ("Metadata", "metadata", "read"),
        ]
    }

    /// The scopes a Forgejo or Gitea token needs.
    fn forgejo_scopes(&self) -> [&'static str; 3] {
        let repository = if self.writes {
            "write:repository"
        } else {
            "read:repository"
        };
        [repository, "read:issue", "read:user"]
    }

    /// What the token field says while it has the focus: how to make a token, and what it
    /// needs, in case the host's page doesn't keep what the URL filled in.
    fn token_help(&self) -> Vec<String> {
        if self.github {
            let mut lines = vec![
                "ctrl+o opens GitHub's page for a new token, with these filled in.".to_owned(),
                "If the page lost them, give the token these repository permissions:".to_owned(),
            ];
            for (name, _, level) in self.github_permissions() {
                let level = if level == "write" {
                    "Read and write"
                } else {
                    "Read"
                };
                lines.push(format!("  {name}: {level}"));
            }
            return lines;
        }
        let mut lines = vec![
            "ctrl+o opens the host's page for tokens. Give the token these scopes:".to_owned(),
        ];
        lines.extend(self.forgejo_scopes().map(|scope| format!("  {scope}")));
        lines
    }

    fn submit(&mut self) -> FormAction {
        match self.source() {
            Ok(source) => {
                let token = Some(self.token.trim().to_owned()).filter(|t| !t.is_empty());
                FormAction::Save(source, token)
            }
            Err((error, field)) => {
                self.error = Some(error);
                self.focus = field;
                FormAction::None
            }
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let label_width = 10;
        let mut lines: Vec<Line> = self
            .fields()
            .into_iter()
            .map(|field| {
                let focused = field == self.focus;
                let marker = if focused { "› " } else { "  " };
                let label = Span::styled(
                    format!("{marker}{:<label_width$}", field.label()),
                    if focused {
                        Style::new().bold()
                    } else {
                        Style::new()
                    },
                );
                let mut spans = vec![label];
                if field.choice() {
                    let options = match field {
                        Field::Kind => [("github", self.github), ("forgejo", !self.github)],
                        _ => [("token", self.token_auth), (self.cli(), !self.token_auth)],
                    };
                    for (option, on) in options {
                        let style = if on {
                            Style::new().reversed().bold()
                        } else {
                            Style::new().dim()
                        };
                        spans.extend([Span::styled(format!(" {option} "), style), Span::raw(" ")]);
                    }
                } else {
                    let text = self.text(field);
                    // The token never shows, nor how long it is
                    let shown = if field == Field::Token && !text.is_empty() {
                        HIDDEN
                    } else {
                        text
                    };
                    spans.push(Span::raw(shown.to_owned()));
                    if focused {
                        spans.push(Span::raw("█"));
                    }
                    if text.is_empty() {
                        spans.push(Span::styled(self.placeholder(field), Style::new().dim()));
                    }
                }
                Line::from(spans)
            })
            .collect();

        lines.push(Line::raw(""));
        if !self.token_auth {
            let note = format!("Uses {}'s login, with all its rights", self.cli());
            lines.push(Line::from(note).dim());
        }
        if !self.github && !self.token_auth && !self.logins.is_empty() {
            lines.push(Line::from(format!("tea logins: {}", self.logins.join(", "))).dim());
        }
        if self.focus == Field::Token {
            for line in self.token_help() {
                lines.push(Line::from(line).dim());
            }
        }
        if let Some(error) = &self.error {
            lines.push(Line::from(error.clone()).red());
        }
        let mut shown = vec![("tab", "Next"), ("←→", "Choose"), ("enter", "Save")];
        if self.token_auth {
            shown.push(("ctrl+o", "New token"));
        }
        shown.push(("esc", "Cancel"));
        lines.push(keys(&shown));

        let title = if self.editing.is_some() {
            " Edit the source "
        } else {
            " Add a source "
        };
        let rect = popup(area, 80, lines.len() as u16 + 2);
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::bordered()
                    .title(title.bold())
                    .padding(Padding::horizontal(1)),
            ),
            rect,
        );
    }
}

/// The owner in a GitHub search: the first `org:`, `user:` or `owner:` qualifier, or the owner
/// in `repo:owner/name`.
fn query_owner(query: &str) -> Option<&str> {
    query.split_whitespace().find_map(|word| {
        let (qualifier, value) = word.split_once(':')?;
        let owner = match qualifier {
            "org" | "user" | "owner" => value,
            "repo" => value.split('/').next()?,
            _ => return None,
        };
        Some(owner).filter(|o| !o.is_empty() && *o != "@me")
    })
}

/// `text` with a capital letter at the start, as it starts a message.
fn cap(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Key help, in the same style as the footer.
pub fn keys(keys: &[(&str, &str)]) -> Line<'static> {
    Line::from(
        keys.iter()
            .flat_map(|(key, label)| {
                [
                    Span::styled(format!(" {key} "), Style::new().bold().reversed()),
                    Span::raw(format!(" {label}  ")),
                ]
            })
            .collect::<Vec<_>>(),
    )
}

/// A box in the middle of `area`, at most `width` wide.
pub fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(form: &mut Form, text: &str) {
        for c in text.chars() {
            form.on_key(key(KeyCode::Char(c)));
        }
    }

    fn saved(form: &mut Form) -> Option<Source> {
        match form.on_key(key(KeyCode::Enter)) {
            FormAction::Save(source, _) => Some(source),
            _ => None,
        }
    }

    #[test]
    fn a_new_source_has_a_token_of_its_own_unless_you_pick_gh() {
        let mut form = Form::add(vec![]);
        assert_eq!(saved(&mut form), None);
        assert_eq!(form.focus, Field::Token);
        assert_eq!(
            form.error.as_deref(),
            Some("Paste a token, or pick gh for Auth")
        );

        // The token goes to the keychain, not into the source, and it never shows
        typed(&mut form, "ghp_secret");
        let text = draw(&form);
        assert!(text.contains(&format!("Token     {HIDDEN}█")), "{text}");
        assert!(!text.contains("secret"));
        let FormAction::Save(source, token) = form.on_key(key(KeyCode::Enter)) else {
            panic!("expected a save");
        };
        let github = Source::github(DEFAULT_QUERY, None, None);
        assert_eq!(source, github.clone().with_token(Token::default()));
        assert_eq!(token.as_deref(), Some("ghp_secret"));

        // With gh, there is no token field, and a note says what that means
        let mut form = Form::add(vec![]);
        form.on_key(key(KeyCode::Tab));
        form.on_key(key(KeyCode::Right));
        assert!(!form.fields().contains(&Field::Token));
        assert!(draw(&form).contains("Uses gh's login, with all its rights"));
        assert_eq!(saved(&mut form), Some(github));
    }

    #[test]
    fn a_forgejo_form_needs_a_url_with_a_token_or_a_login_with_tea() {
        let mut form = Form::add(vec!["home".into()]);
        form.on_key(key(KeyCode::Right));
        assert_eq!(saved(&mut form), None);
        assert_eq!(form.focus, Field::Url);
        assert!(form.error.as_deref().unwrap().starts_with("Give the URL"));

        typed(&mut form, "http://git.example.com/");
        assert_eq!(saved(&mut form), None);
        let error = form.error.clone().unwrap();
        assert!(
            error.contains("put allow_http = true on the source"),
            "{error}"
        );
        for _ in 0.."http://git.example.com/".len() {
            form.on_key(key(KeyCode::Backspace));
        }
        typed(&mut form, "https://git.example.com/");
        form.on_key(key(KeyCode::Tab));
        typed(&mut form, " acme ");
        form.on_key(key(KeyCode::Tab));
        typed(&mut form, "tok");
        assert_eq!(
            saved(&mut form),
            Some(Source::forgejo_api(
                "https://git.example.com",
                Some("acme".into()),
                None
            ))
        );

        // With tea, it needs a login
        let mut form = Form::add(vec!["home".into()]);
        form.on_key(key(KeyCode::Right));
        form.on_key(key(KeyCode::Tab));
        form.on_key(key(KeyCode::Right));
        assert_eq!(saved(&mut form), None);
        assert_eq!(form.focus, Field::Login);
        assert!(form.error.is_some());
        assert!(draw(&form).contains("tea logins: home"));

        typed(&mut form, "home");
        assert!(form.error.is_none());
        form.on_key(key(KeyCode::Tab));
        typed(&mut form, " me ");
        assert_eq!(
            saved(&mut form),
            Some(Source::forgejo("home", Some("me".into()), None))
        );
    }

    #[test]
    fn editing_a_token_source_keeps_where_its_token_is() {
        let token = Token {
            command: Some("op read op://Work/GitHub/token".into()),
            env: None,
        };
        let source = Source::github("org:acme", None, None).with_token(token);
        let mut form = Form::edit(0, &source, vec![], false);
        let text = draw(&form);
        assert!(text.contains("from token_command or token_env"), "{text}");
        assert_eq!(saved(&mut form), Some(source));

        let source = Source::forgejo_api("https://git.example.com", None, None);
        let mut form = Form::edit(0, &source, vec![], true);
        assert!(draw(&form).contains("in the keychain; type or paste to replace it"));
        let FormAction::Save(again, token) = form.on_key(key(KeyCode::Enter)) else {
            panic!("expected a save");
        };
        assert_eq!((again, token), (source, None));
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_o_opens_githubs_token_page_with_the_permissions_filled_in() {
        let page = "https://github.com/settings/personal-access-tokens/new?name=open_prs\
                    &description=PRs%2C%20reviews%20and%20CI%20for%20open_prs";
        let mut form = Form::add(vec![]);
        form.on_key(key(KeyCode::Tab));
        form.on_key(key(KeyCode::Tab));
        form.on_key(key(KeyCode::Tab));
        typed(&mut form, "org:acme is:draft");
        let FormAction::Open(url) = form.on_key(ctrl('o')) else {
            panic!("expected a page to open");
        };
        assert_eq!(
            url,
            format!("{page}&pull_requests=read&statuses=read&metadata=read&target_name=acme")
        );

        // With writes on, the token may change PRs; GitHub Enterprise has its own page
        let mut form = Form::add(vec![]).with_writes(true);
        form.hostname = "ghe.acme.dev".into();
        let FormAction::Open(url) = form.on_key(ctrl('o')) else {
            panic!("expected a page to open");
        };
        assert!(url.starts_with("https://ghe.acme.dev/settings/personal-access-tokens/new?"));
        assert!(url.contains("&pull_requests=write&"), "{url}");
        assert!(!url.contains("target_name"), "{url}");

        // ctrl+o puts the focus on the token field, which lists what the token needs, in case
        // the page lost it, and the keys show ctrl+o
        assert_eq!(form.focus, Field::Token);
        let text = draw(&form);
        assert!(
            text.contains("ctrl+o opens GitHub's page for a new token"),
            "{text}"
        );
        for permission in [
            "  Pull requests: Read and write ",
            "  Commit statuses: Read ",
            "  Metadata: Read ",
        ] {
            assert!(text.contains(permission), "{permission} in {text}");
        }
        assert!(text.contains(" ctrl+o  New token"), "{text}");
        assert!(
            text.contains("If the page lost them, give the token these repository permissions:")
        );
        form.writes = false;
        assert!(draw(&form).contains("  Pull requests: Read "));
    }

    #[test]
    fn ctrl_o_opens_the_forgejo_token_page_once_there_is_a_url() {
        let mut form = Form::add(vec![]);
        form.on_key(key(KeyCode::Right));
        assert!(matches!(form.on_key(ctrl('o')), FormAction::None));
        assert_eq!(form.focus, Field::Url);
        typed(&mut form, "https://git.example.com/");
        let FormAction::Open(url) = form.on_key(ctrl('o')) else {
            panic!("expected a page to open");
        };
        assert_eq!(url, "https://git.example.com/user/settings/applications");
        assert_eq!(form.focus, Field::Token);
        let text = draw(&form);
        for scope in ["  read:repository ", "  read:issue ", "  read:user "] {
            assert!(text.contains(scope), "{scope} in {text}");
        }
        assert!(!text.contains("write:repository"), "{text}");
        form.writes = true;
        assert!(draw(&form).contains("  write:repository "));

        // A source that uses tea has no token, so ctrl+o does nothing
        form.token_auth = false;
        assert!(matches!(form.on_key(ctrl('o')), FormAction::None));
    }

    #[test]
    fn the_owner_comes_from_the_query() {
        assert_eq!(query_owner("is:draft org:acme"), Some("acme"));
        assert_eq!(query_owner("repo:acme/app"), Some("acme"));
        assert_eq!(query_owner("user:bao"), Some("bao"));
        assert_eq!(query_owner("involves:@me"), None);
        assert_eq!(query_owner("user:@me"), None);
    }

    fn draw(form: &Form) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 20)).unwrap();
        terminal.draw(|f| form.draw(f, f.area())).unwrap();
        let buffer = terminal.backend().buffer();
        (0..20)
            .map(|y| (0..90).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn editing_starts_from_the_source_and_keeps_what_isnt_changed() {
        let source = Source::github(
            "org:acme",
            Some("ghe.example.com".into()),
            Some("Work".into()),
        );
        let mut form = Form::edit(2, &source, vec![], false);
        assert_eq!(form.editing, Some(2));
        // Name has the focus; clear it, so the hostname becomes the name
        for _ in 0..4 {
            form.on_key(key(KeyCode::Backspace));
        }
        form.on_key(key(KeyCode::Tab));
        typed(&mut form, " is:draft");
        assert_eq!(
            saved(&mut form),
            Some(Source::github(
                "org:acme is:draft",
                Some("ghe.example.com".into()),
                None
            ))
        );
    }

    #[test]
    fn q_types_into_a_field_and_esc_cancels() {
        let mut form = Form::add(vec![]);
        form.on_key(key(KeyCode::Down));
        form.on_key(key(KeyCode::Down));
        typed(&mut form, "q");
        assert_eq!(form.name, "q");
        assert!(matches!(form.on_key(key(KeyCode::Esc)), FormAction::Cancel));
    }
}
