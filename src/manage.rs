//! The panel to see, add, edit and delete sources, and its form.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Padding, Paragraph, Row, Table, TableState};

use crate::config::{DEFAULT_QUERY, Kind, Source};

/// The list of sources, over the table.
#[derive(Default)]
pub struct List {
    pub cursor: usize,
    /// Waiting for y to delete the source under the cursor
    pub confirm: bool,
    /// Why the last change didn't happen
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
}

pub fn draw_list(frame: &mut Frame, area: Rect, list: &List, rows: &[ListRow], note: &str) {
    let help = if list.confirm {
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
        Constraint::Length(widest(|r| &r.state, "State")),
    ];
    let table = Table::new(
        rows.iter().map(|r| {
            Row::new([
                Cell::from(r.name.clone()),
                Cell::from(r.what.clone()),
                Cell::from(Span::styled(r.state.clone(), r.state_style)),
            ])
        }),
        widths,
    )
    .header(Row::new(["Name", "Searches", "State"]).bold())
    .column_spacing(2)
    .row_highlight_style(Style::new().bg(Color::DarkGray).bold());
    let mut state = TableState::new().with_selected(Some(list.cursor));
    frame.render_stateful_widget(table, table_area, &mut state);
    frame.render_widget(Paragraph::new(lines), text_area);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Kind,
    Name,
    Query,
    Hostname,
    Login,
    Owner,
}

impl Field {
    fn label(self) -> &'static str {
        match self {
            Field::Kind => "Kind",
            Field::Name => "Name",
            Field::Query => "Query",
            Field::Hostname => "Hostname",
            Field::Login => "Login",
            Field::Owner => "Owner",
        }
    }
}

/// The form to add a source or edit one.
pub struct Form {
    /// The index of the source it edits, or None to add one
    pub editing: Option<usize>,
    github: bool,
    name: String,
    query: String,
    hostname: String,
    login: String,
    owner: String,
    focus: Field,
    pub error: Option<String>,
    /// From `tea logins list`, to show which logins there are
    logins: Vec<String>,
}

pub enum FormAction {
    None,
    Cancel,
    Save(Source),
}

impl Form {
    pub fn add(logins: Vec<String>) -> Self {
        Form {
            editing: None,
            github: true,
            name: String::new(),
            query: String::new(),
            hostname: String::new(),
            login: String::new(),
            owner: String::new(),
            focus: Field::Kind,
            error: None,
            logins,
        }
    }

    pub fn edit(index: usize, source: &Source, logins: Vec<String>) -> Self {
        let mut form = Form::add(logins);
        form.editing = Some(index);
        form.name = source.name.clone().unwrap_or_default();
        match &source.kind {
            Kind::GitHub { query, hostname } => {
                form.query = query.clone();
                form.hostname = hostname.clone().unwrap_or_default();
            }
            Kind::Forgejo { login, owner } => {
                form.github = false;
                form.login = login.clone();
                form.owner = owner.clone().unwrap_or_default();
            }
        }
        form.focus = Field::Name;
        form
    }

    fn fields(&self) -> [Field; 4] {
        if self.github {
            [Field::Kind, Field::Name, Field::Query, Field::Hostname]
        } else {
            [Field::Kind, Field::Name, Field::Login, Field::Owner]
        }
    }

    fn text(&self, field: Field) -> &str {
        match field {
            Field::Kind => "",
            Field::Name => &self.name,
            Field::Query => &self.query,
            Field::Hostname => &self.hostname,
            Field::Login => &self.login,
            Field::Owner => &self.owner,
        }
    }

    fn text_mut(&mut self) -> Option<&mut String> {
        match self.focus {
            Field::Kind => None,
            Field::Name => Some(&mut self.name),
            Field::Query => Some(&mut self.query),
            Field::Hostname => Some(&mut self.hostname),
            Field::Login => Some(&mut self.login),
            Field::Owner => Some(&mut self.owner),
        }
    }

    /// What an empty field means.
    fn placeholder(&self, field: Field) -> String {
        match field {
            Field::Kind => String::new(),
            Field::Name => self
                .source()
                .map_or_else(|| "the login".into(), |s| s.name().to_owned()),
            Field::Query => DEFAULT_QUERY.into(),
            Field::Hostname => "github.com".into(),
            Field::Login => "a name from `tea logins list`".into(),
            Field::Owner => "any".into(),
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
            KeyCode::Enter => return self.submit(),
            KeyCode::Tab | KeyCode::Down => self.move_focus(1),
            KeyCode::BackTab | KeyCode::Up => self.move_focus(-1),
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if self.focus == Field::Kind => {
                self.github = !self.github;
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

    /// The source the form holds now, or None while it has no login.
    fn source(&self) -> Option<Source> {
        let given = |s: &str| Some(s.trim().to_owned()).filter(|s| !s.is_empty());
        let name = given(&self.name);
        if self.github {
            let query = given(&self.query).unwrap_or_else(|| DEFAULT_QUERY.into());
            Some(Source::github(&query, given(&self.hostname), name))
        } else {
            Some(Source::forgejo(
                &given(&self.login)?,
                given(&self.owner),
                name,
            ))
        }
    }

    fn submit(&mut self) -> FormAction {
        match self.source() {
            Some(source) => FormAction::Save(source),
            None => {
                self.error = Some("Give a tea login".into());
                self.focus = Field::Login;
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
                if field == Field::Kind {
                    for (kind, on) in [("github", self.github), ("forgejo", !self.github)] {
                        let style = if on {
                            Style::new().reversed().bold()
                        } else {
                            Style::new().dim()
                        };
                        spans.extend([Span::styled(format!(" {kind} "), style), Span::raw(" ")]);
                    }
                } else {
                    let text = self.text(field);
                    spans.push(Span::raw(text.to_owned()));
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
        if !self.github && !self.logins.is_empty() {
            lines.push(Line::from(format!("tea logins: {}", self.logins.join(", "))).dim());
        }
        if let Some(error) = &self.error {
            lines.push(Line::from(error.clone()).red());
        }
        lines.push(keys(&[
            ("tab", "Next"),
            ("←→", "Kind"),
            ("enter", "Save"),
            ("esc", "Cancel"),
        ]));

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
fn popup(area: Rect, width: u16, height: u16) -> Rect {
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
            FormAction::Save(source) => Some(source),
            _ => None,
        }
    }

    #[test]
    fn an_empty_github_form_saves_the_default_search() {
        let mut form = Form::add(vec![]);
        assert_eq!(
            saved(&mut form),
            Some(Source::github(DEFAULT_QUERY, None, None))
        );
    }

    #[test]
    fn a_forgejo_form_needs_a_login() {
        let mut form = Form::add(vec!["home".into()]);
        form.on_key(key(KeyCode::Right));
        assert_eq!(saved(&mut form), None);
        assert_eq!(form.focus, Field::Login);
        assert!(form.error.is_some());

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
    fn editing_starts_from_the_source_and_keeps_what_isnt_changed() {
        let source = Source::github(
            "org:acme",
            Some("ghe.example.com".into()),
            Some("Work".into()),
        );
        let mut form = Form::edit(2, &source, vec![]);
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
        typed(&mut form, "q");
        assert_eq!(form.name, "q");
        assert!(matches!(form.on_key(key(KeyCode::Esc)), FormAction::Cancel));
    }
}
