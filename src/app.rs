use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Padding, Paragraph, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};

use crate::config::{self, Change, Origin, Source};
use crate::manage::{self, Form, FormAction, List, ListRow};
use crate::pr::{Filter, Group, Pr, Status, age, pick, shared_owner};
use crate::sources;

const TITLE: &str = "Open PRs";
const COPIED_TIMEOUT: Duration = Duration::from_secs(5);
const ERROR_TIMEOUT: Duration = Duration::from_secs(15);

/// The keys in the footer, most useful first; it shows as many as fit.
const FOOTER: [(&str, &str); 13] = [
    ("o", "Open"),
    ("/", "Filter"),
    ("c", "Checks"),
    ("w", "For you"),
    ("f", "Failing CI"),
    ("m", "Mine"),
    ("s", "Sources"),
    ("a", "Approved"),
    ("n", "Needs review"),
    ("d", "Drafts"),
    ("g", "Group"),
    ("y", "Copy URL"),
    ("r", "Reload"),
];

/// The keys the footer always shows, at its end.
const FOOTER_END: [(&str, &str); 2] = [("?", "Help"), ("q", "Quit")];

/// Every key, for the help panel.
const HELP: [(&str, &str); 17] = [
    ("enter, o", "Open the PR in the browser"),
    (
        "/",
        "Filter by words in the repo, number, title, author or host",
    ),
    ("esc", "Clear the words"),
    ("c", "Open the PR's CI runs"),
    ("y", "Copy the PR's URL"),
    ("r", "Reload now"),
    ("m", "Only PRs you opened"),
    ("a", "Only approved PRs"),
    ("n", "Only PRs that need review"),
    ("w", "Only PRs that ask for your review"),
    ("f", "Only PRs with failing CI"),
    ("d", "Hide drafts"),
    ("g", "Group by repo or by author"),
    ("s", "Add, edit and delete sources"),
    ("↑ ↓ PgUp PgDn Home End", "Move the cursor"),
    ("?", "Show this help"),
    ("q, ctrl+c", "Quit"),
];

const NOTES_HELP: &str =
    "Notes: for you, the PR asks for your review; draft, it isn't ready; conflict, it can't merge.";

enum Msg {
    Input(io::Result<Event>),
    /// The PRs from the feed with this id
    Loaded(u64, Result<Vec<Pr>, String>),
}

/// A source and what it found.
struct Feed {
    /// Changes when the source does, so a load that started before the change is dropped
    id: u64,
    source: Source,
    prs: Vec<Pr>,
    state: State,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Loading,
    Loaded,
    /// It keeps the PRs from the last load that worked
    Failed(String),
}

enum Mode {
    Table,
    /// Typing the words of the filter
    Search,
    Help,
    Sources(List),
    Form(Form),
}

struct Toast {
    title: String,
    message: String,
    error: bool,
    until: Instant,
}

pub struct App {
    filter: Filter,
    group: Group,
    interval: Duration,
    feeds: Vec<Feed>,
    next_id: u64,
    origin: Origin,
    mode: Mode,
    updated: Option<DateTime<Local>>,
    /// False until every source first loads; see `show`
    follow: bool,
    /// The PRs on screen, in order
    rows: Vec<Pr>,
    /// The owner every PR has, which the table leaves out
    owner: Option<String>,
    table: TableState,
    /// Where the table was last drawn, to map mouse clicks to rows
    table_area: Rect,
    toasts: Vec<Toast>,
    tx: Sender<Msg>,
    quit: bool,
    /// Loads a source; the tests replace it, so they don't run gh or tea
    load: fn(&Source) -> Result<Vec<Pr>>,
    /// The tea logins for the form's hint
    logins: fn() -> Vec<String>,
}

pub struct Options {
    pub filter: Filter,
    pub group: Group,
    pub interval: Duration,
}

pub fn run(options: Options, sources: Vec<Source>, origin: Origin) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(options, sources, origin, tx.clone());

    let mut terminal = ratatui::init();
    let result = execute!(io::stdout(), EnableMouseCapture)
        .map_err(anyhow::Error::from)
        .and_then(|()| {
            read_input(tx);
            app.run(&mut terminal, rx)
        });
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Sends each key and mouse event to the main loop, so it can wait on input and loads together.
fn read_input(tx: Sender<Msg>) {
    thread::spawn(move || {
        loop {
            let event = event::read();
            let failed = event.is_err();
            if tx.send(Msg::Input(event)).is_err() || failed {
                break;
            }
        }
    });
}

impl App {
    fn new(options: Options, sources: Vec<Source>, origin: Origin, tx: Sender<Msg>) -> Self {
        let feeds = sources
            .into_iter()
            .zip(0..)
            .map(|(source, id)| Feed {
                id,
                source,
                prs: Vec::new(),
                state: State::Loaded,
            })
            .collect::<Vec<_>>();
        App {
            filter: options.filter,
            group: options.group,
            interval: options.interval,
            next_id: feeds.len() as u64,
            feeds,
            origin,
            mode: Mode::Table,
            updated: None,
            follow: false,
            rows: Vec::new(),
            owner: None,
            table: TableState::new(),
            table_area: Rect::default(),
            toasts: Vec::new(),
            tx,
            quit: false,
            load: sources::fetch,
            // Without tea, the form shows no hint
            logins: || sources::tea_logins().unwrap_or_default(),
        }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal, rx: Receiver<Msg>) -> Result<()> {
        self.reload();
        let mut next_reload = Instant::now() + self.interval;
        while !self.quit {
            terminal.draw(|frame| self.draw(frame))?;

            // Wake for the next reload, the next toast to expire, or each second to update the ages
            let now = Instant::now();
            let wake = self
                .toasts
                .iter()
                .map(|t| t.until)
                .fold(next_reload.min(now + Duration::from_secs(1)), Instant::min);
            match rx.recv_timeout(wake.saturating_duration_since(now)) {
                Ok(Msg::Input(event)) => self.on_event(event?),
                Ok(Msg::Loaded(id, result)) => self.loaded(id, result),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            let now = Instant::now();
            if now >= next_reload {
                self.reload();
                while next_reload <= now {
                    next_reload += self.interval;
                }
            }
            self.toasts.retain(|t| t.until > now);
        }
        Ok(())
    }

    fn loading(&self) -> bool {
        self.feeds.iter().any(|f| f.state == State::Loading)
    }

    fn reload(&mut self) {
        // A slow source must not stack up behind the timer
        if self.loading() {
            return;
        }
        for i in 0..self.feeds.len() {
            self.fetch(i);
        }
        self.show();
    }

    /// Loads one source in the background; each shows as soon as it answers.
    fn fetch(&mut self, i: usize) {
        let feed = &mut self.feeds[i];
        feed.state = State::Loading;
        let (id, source, tx, load) = (feed.id, feed.source.clone(), self.tx.clone(), self.load);
        thread::spawn(move || {
            let result = load(&source).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Loaded(id, result));
        });
    }

    fn loaded(&mut self, id: u64, result: Result<Vec<Pr>, String>) {
        // The source changed or went away while it loaded
        let Some(feed) = self.feeds.iter_mut().find(|f| f.id == id) else {
            return;
        };
        match result {
            Ok(prs) => {
                feed.prs = prs;
                feed.state = State::Loaded;
            }
            Err(error) => {
                feed.state = State::Failed(error.clone());
                let title = format!("{} failed", feed.source.name());
                self.notify(title, error, true, ERROR_TIMEOUT);
            }
        }
        let done = !self.loading();
        if done {
            self.updated = Some(Local::now());
        }
        self.show();
        if done {
            self.follow = true;
        }
    }

    /// Picks the rows to show, and puts the cursor back.
    fn show(&mut self) {
        let current = self.selected().map(|p| p.url.clone());
        let row = self.table.selected().unwrap_or(0);
        let all = || self.feeds.iter().flat_map(|f| &f.prs);
        self.owner = shared_owner(all());
        self.rows = pick(all(), &self.filter, self.group);
        if self.rows.is_empty() {
            self.table.select(None);
            return;
        }
        // Follow the PR under the cursor once every source has loaded. Before that, keep the
        // row, so the cursor doesn't follow one source's first PR down when another's arrive above it
        let mut index = row.min(self.rows.len() - 1);
        if self.follow
            && let Some(i) = current.and_then(|url| self.rows.iter().position(|p| p.url == url))
        {
            index = i;
        }
        self.table.select(Some(index));
    }

    fn selected(&self) -> Option<&Pr> {
        self.table.selected().and_then(|i| self.rows.get(i))
    }

    fn notify(&mut self, title: String, message: String, error: bool, timeout: Duration) {
        self.toasts.push(Toast {
            title,
            message: clean(&message),
            error,
            until: Instant::now() + timeout,
        });
    }

    /// Makes a change to the sources, and saves it when they came from the config file.
    fn change(&mut self, change: Change) -> Result<(), String> {
        if let Origin::File(path) = &self.origin {
            let current: Vec<Source> = self.feeds.iter().map(|f| f.source.clone()).collect();
            config::save(path, &current, &change).map_err(|e| format!("{e:#}"))?;
        }
        match change {
            Change::Add(source) => {
                self.feeds.push(Feed {
                    id: self.next_id,
                    source,
                    prs: Vec::new(),
                    state: State::Loaded,
                });
                self.next_id += 1;
                self.fetch(self.feeds.len() - 1);
            }
            Change::Update(i, source) => {
                // The old PRs came from the old settings
                self.feeds[i] = Feed {
                    id: self.next_id,
                    source,
                    prs: Vec::new(),
                    state: State::Loaded,
                };
                self.next_id += 1;
                self.fetch(i);
            }
            Change::Remove(i) => {
                self.feeds.remove(i);
            }
        }
        self.show();
        Ok(())
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
            Event::Mouse(mouse) if matches!(self.mode, Mode::Table) => self.on_mouse(mouse),
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        match std::mem::replace(&mut self.mode, Mode::Table) {
            Mode::Table => self.on_table_key(key),
            Mode::Sources(list) => self.on_sources_key(list, key),
            Mode::Form(form) => self.on_form_key(form, key),
            Mode::Search => self.on_search_key(key),
            // Any key closes the help
            Mode::Help => {}
        }
    }

    fn on_table_key(&mut self, key: KeyEvent) {
        let page = usize::from(self.table_area.height.saturating_sub(1)).max(1);
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Enter | KeyCode::Char('o') => self.open(|p| &p.url),
            KeyCode::Char('c') => self.open(|p| &p.checks_url),
            KeyCode::Char('y') => self.copy_url(),
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char('m') => self.toggle(|f| &mut f.mine),
            KeyCode::Char('a') => self.toggle_status(Status::Approved),
            KeyCode::Char('n') => self.toggle_status(Status::Needs),
            KeyCode::Char('w') => self.toggle(|f| &mut f.for_you),
            KeyCode::Char('f') => self.toggle(|f| &mut f.failing),
            KeyCode::Char('d') => self.toggle(|f| &mut f.hide_drafts),
            KeyCode::Char('g') => {
                self.group = self.group.toggled();
                self.show();
            }
            KeyCode::Char('s') => self.mode = Mode::Sources(List::default()),
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('/') => self.mode = Mode::Search,
            KeyCode::Esc if !self.filter.text.is_empty() => {
                self.filter.text.clear();
                self.show();
            }
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::PageDown => self.move_cursor(page as isize),
            KeyCode::PageUp => self.move_cursor(-(page as isize)),
            KeyCode::Home => self.move_cursor(isize::MIN),
            KeyCode::End => self.move_cursor(isize::MAX),
            _ => {}
        }
    }

    /// The table follows each key, and enter keeps the words or esc clears them.
    fn on_search_key(&mut self, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => return,
            KeyCode::Esc => {
                self.filter.text.clear();
                self.show();
                return;
            }
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Backspace => {
                self.filter.text.pop();
                self.show();
            }
            KeyCode::Char('u') if control => {
                self.filter.text.clear();
                self.show();
            }
            KeyCode::Char(c) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.filter.text.push(c);
                self.show();
            }
            _ => {}
        }
        self.mode = Mode::Search;
    }

    fn on_sources_key(&mut self, mut list: List, key: KeyEvent) {
        list.message = None;
        if list.confirm {
            list.confirm = false;
            if key.code == KeyCode::Char('y') {
                if self.feeds.len() == 1 {
                    list.message = Some("Add another source before you delete the last one".into());
                } else if let Err(error) = self.change(Change::Remove(list.cursor)) {
                    list.message = Some(error);
                }
                list.cursor = list.cursor.min(self.feeds.len() - 1);
            }
            self.mode = Mode::Sources(list);
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('s' | 'q') => return,
            KeyCode::Down => list.cursor = (list.cursor + 1).min(self.feeds.len() - 1),
            KeyCode::Up => list.cursor = list.cursor.saturating_sub(1),
            KeyCode::Char('a') => {
                self.mode = Mode::Form(Form::add((self.logins)()));
                return;
            }
            KeyCode::Char('e') | KeyCode::Enter => {
                let source = &self.feeds[list.cursor].source;
                self.mode = Mode::Form(Form::edit(list.cursor, source, (self.logins)()));
                return;
            }
            KeyCode::Char('d') => list.confirm = true,
            _ => {}
        }
        self.mode = Mode::Sources(list);
    }

    fn on_form_key(&mut self, mut form: Form, key: KeyEvent) {
        let back = |cursor| {
            Mode::Sources(List {
                cursor,
                ..List::default()
            })
        };
        match form.on_key(key) {
            FormAction::None => self.mode = Mode::Form(form),
            FormAction::Cancel => self.mode = back(form.editing.unwrap_or(0)),
            FormAction::Save(source) => {
                let (change, cursor) = match form.editing {
                    Some(i) => (Change::Update(i, source), i),
                    None => (Change::Add(source), self.feeds.len()),
                };
                match self.change(change) {
                    Ok(()) => self.mode = back(cursor),
                    Err(error) => {
                        form.error = Some(error);
                        self.mode = Mode::Form(form);
                    }
                }
            }
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollDown => self.move_cursor(1),
            MouseEventKind::ScrollUp => self.move_cursor(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                let area = self.table_area;
                // The first line of the table is its header
                if mouse.column < area.x
                    || mouse.column >= area.right()
                    || mouse.row <= area.y
                    || mouse.row >= area.bottom()
                {
                    return;
                }
                let index = self.table.offset() + usize::from(mouse.row - area.y - 1);
                if index >= self.rows.len() {
                    return;
                }
                // Like Enter, a click on the row under the cursor opens it
                if self.table.selected() == Some(index) {
                    self.open(|p| &p.url);
                } else {
                    self.table.select(Some(index));
                }
            }
            _ => {}
        }
    }

    fn move_cursor(&mut self, by: isize) {
        if self.rows.is_empty() {
            return;
        }
        let row = self.table.selected().unwrap_or(0);
        let last = self.rows.len() - 1;
        self.table
            .select(Some(row.saturating_add_signed(by).min(last)));
    }

    fn toggle(&mut self, part: fn(&mut Filter) -> &mut bool) {
        let on = part(&mut self.filter);
        *on = !*on;
        self.show();
    }

    fn toggle_status(&mut self, status: Status) {
        // A PR can't be both approved and need review, so each filter replaces the other
        self.filter.status = if self.filter.status == Some(status) {
            None
        } else {
            Some(status)
        };
        self.show();
    }

    /// Opens a page of the PR under the cursor in the browser.
    fn open(&mut self, page: fn(&Pr) -> &String) {
        let Some(url) = self.selected().map(|p| page(p).clone()) else {
            return;
        };
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let child = Command::new(opener)
            .arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match child {
            // Wait in the background, so the TUI doesn't stop while the browser starts
            Ok(mut child) => {
                thread::spawn(move || child.wait());
            }
            Err(e) => self.notify(
                format!("Could not open {url}"),
                format!("{opener}: {e}"),
                true,
                ERROR_TIMEOUT,
            ),
        }
    }

    fn copy_url(&mut self) {
        let Some(url) = self.selected().map(|p| p.url.clone()) else {
            return;
        };
        let pbcopy = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match pbcopy {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(url.as_bytes());
                }
                let _ = child.wait();
            }
            // Without pbcopy, ask the terminal to copy it (OSC 52)
            Err(_) => {
                let mut out = io::stdout();
                let _ = write!(out, "\x1b]52;c;{}\x07", base64(url.as_bytes()));
                let _ = out.flush();
            }
        }
        self.notify("Copied".into(), url, false, COPIED_TIMEOUT);
    }

    fn subtitle(&self) -> String {
        let mut parts = vec![
            format!("{} PRs", self.rows.len()),
            format!("by {}", self.group.name()),
        ];
        parts.extend(self.filter.describe());
        let mut loading: Vec<_> = self
            .feeds
            .iter()
            .filter(|f| f.state == State::Loading)
            .map(|f| f.source.name())
            .collect();
        if !loading.is_empty() {
            loading.sort();
            parts.push(format!("loading {}…", loading.join(" and ")));
        } else if let Some(updated) = self.updated {
            parts.push(format!(
                "updated {}, every {}s",
                updated.format("%H:%M:%S"),
                self.interval.as_secs()
            ));
        }
        parts.join(" · ")
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [header, body, footer] = frame.area().layout(&Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ]));

        frame.render_widget(
            Paragraph::new(format!("{TITLE} — {}", self.subtitle()))
                .centered()
                .style(Style::new().reversed()),
            header,
        );

        self.table_area = body;
        let table = self.table_widget(Utc::now());
        frame.render_stateful_widget(table, body, &mut self.table);
        let footer_line = match self.mode {
            Mode::Search => {
                let mut spans = vec![
                    Span::styled(" / ", Style::new().bold().reversed()),
                    Span::raw(format!(" {}", self.filter.text)),
                    Span::raw("█   "),
                ];
                spans.extend(manage::keys(&[("enter", "Keep"), ("esc", "Clear")]).spans);
                Line::from(spans)
            }
            _ => footer_keys(footer.width),
        };
        frame.render_widget(footer_line, footer);

        match &self.mode {
            Mode::Table => {}
            Mode::Sources(list) => {
                manage::draw_list(frame, body, list, &self.source_rows(), &self.save_note());
            }
            Mode::Form(form) => form.draw(frame, body),
            Mode::Help => draw_help(frame, body),
            Mode::Search => {}
        }
        self.draw_toasts(frame, body);
    }

    fn source_rows(&self) -> Vec<ListRow> {
        self.feeds
            .iter()
            .map(|f| {
                let (state, style) = match &f.state {
                    State::Loading => ("loading…".into(), Style::new().dim()),
                    State::Loaded => (format!("{} PRs", f.prs.len()), Style::new().green()),
                    State::Failed(_) => ("failed".into(), Style::new().red()),
                };
                ListRow {
                    name: f.source.name().into(),
                    what: f.source.describe(),
                    state,
                    state_style: style,
                    error: match &f.state {
                        State::Failed(error) => Some(clean(error)),
                        _ => None,
                    },
                }
            })
            .collect()
    }

    /// Where changes to the sources go.
    fn save_note(&self) -> String {
        match &self.origin {
            Origin::File(path) => format!("Changes save to {}", tilde(&path.display().to_string())),
            Origin::Flags => {
                "These come from --github and --forgejo, so changes last until you quit".into()
            }
            Origin::Nowhere => {
                "There is no home folder to save to, so changes last until you quit".into()
            }
        }
    }

    fn table_widget(&self, now: DateTime<Utc>) -> Table<'static> {
        let by_repo = self.group == Group::Repo;
        let headers: &[&str] = if by_repo {
            &[
                "Repo", "Host", "#", "Status", "CI", "Notes", "Author", "Updated", "Title",
            ]
        } else {
            &[
                "Author", "Host", "PR", "Status", "CI", "Notes", "Updated", "Title",
            ]
        };
        let owner = self.owner.as_deref();

        let mut cells: Vec<Vec<Line<'static>>> = Vec::with_capacity(self.rows.len());
        let mut last: Option<(&str, Option<&str>)> = None;
        for p in &self.rows {
            // Name each group once, so the blank cells under it show where it ends
            let group = if by_repo {
                (p.repo.as_str(), Some(p.host.as_str()))
            } else {
                (p.author.as_str(), None)
            };
            let label = if Some(group) == last {
                String::new()
            } else if by_repo {
                p.repo_without(owner).to_owned()
            } else {
                p.author.clone()
            };
            last = Some(group);

            let mut row = vec![Line::raw(label), Line::raw(p.host.clone()).dim()];
            let status = Line::styled(p.status.label(), Style::new().fg(p.status.color()));
            // Blank when the last commit has no checks
            let ci = p.ci.map_or_else(Line::default, |ci| {
                Line::styled(ci.label(), Style::new().fg(ci.color()))
            });
            let notes = Line::from(
                p.notes()
                    .into_iter()
                    .enumerate()
                    .flat_map(|(i, note)| {
                        let gap = Span::raw(if i == 0 { "" } else { " " });
                        [gap, Span::styled(note.label(), note.style())]
                    })
                    .collect::<Vec<_>>(),
            );
            if by_repo {
                row.extend([
                    Line::raw(format!("#{}", p.number)),
                    status,
                    ci,
                    notes,
                    Line::raw(p.author.clone()),
                ]);
            } else {
                row.extend([
                    Line::raw(format!("{}#{}", p.repo_without(owner), p.number)),
                    status,
                    ci,
                    notes,
                ]);
            }
            row.extend([Line::raw(age(p.updated, now)), Line::raw(p.title.clone())]);
            cells.push(row);
        }

        // Size each column to fit, and give the rest of the width to the title
        let widths: Vec<Constraint> = (0..headers.len())
            .map(|i| {
                if i == headers.len() - 1 {
                    return Constraint::Fill(1);
                }
                let widest = cells.iter().map(|row| row[i].width()).max().unwrap_or(0);
                Constraint::Length(widest.max(headers[i].len()) as u16)
            })
            .collect();

        let rows = cells.into_iter().zip(&self.rows).map(|(row, p)| {
            let row = Row::new(row.into_iter().map(Cell::from));
            // A draft isn't ready for review yet
            if p.draft { row.dim() } else { row }
        });
        Table::new(rows, widths)
            .header(Row::new(headers.iter().map(|h| Cell::from(*h))).bold())
            .column_spacing(2)
            .row_highlight_style(
                Style::new()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
    }

    fn draw_toasts(&self, frame: &mut Frame, area: Rect) {
        // Newest at the bottom, stacked up from the bottom right corner
        let width = area.width.min(60);
        let inner = width.saturating_sub(4).max(1);
        let mut bottom = area.bottom();
        for toast in self.toasts.iter().rev() {
            let lines = wrap(&toast.message, inner);
            let height = (lines.len() as u16).min(10) + 2;
            if bottom < area.y + height {
                break;
            }
            let rect = Rect::new(area.right() - width, bottom - height, width, height);
            bottom -= height;
            let color = if toast.error {
                Color::Red
            } else {
                Color::Green
            };
            let block = Block::bordered()
                .border_style(Style::new().fg(color))
                .title(Span::styled(
                    format!(" {} ", toast.title),
                    Style::new().fg(color).bold(),
                ));
            frame.render_widget(Clear, rect);
            let text: Vec<Line> = lines.into_iter().map(Line::from).collect();
            frame.render_widget(
                Paragraph::new(text).block(block.padding(Padding::horizontal(1))),
                rect,
            );
        }
    }
}

/// As many footer keys as fit in `width`, then the ones it always shows.
fn footer_keys(width: u16) -> Line<'static> {
    // manage::keys draws each as " key " then " label  "
    let size =
        |(key, label): &(&str, &str)| Span::raw(*key).width() + Span::raw(*label).width() + 5;
    let mut room = usize::from(width).saturating_sub(FOOTER_END.iter().map(size).sum());
    let mut shown: Vec<(&str, &str)> = FOOTER
        .iter()
        .take_while(|k| {
            let fits = size(k) <= room;
            room = room.saturating_sub(size(k));
            fits
        })
        .copied()
        .collect();
    shown.extend(FOOTER_END);
    manage::keys(&shown)
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let key_width = HELP
        .iter()
        .map(|(k, _)| Span::raw(*k).width())
        .max()
        .unwrap_or(0);
    let mut lines: Vec<Line> = HELP
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!("{key:<key_width$}  "), Style::new().bold()),
                Span::raw(*what),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::from(NOTES_HELP).dim());
    lines.push(Line::from("Any key closes this.").dim());
    let rect = manage::popup(area, 100, lines.len() as u16 + 2);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Keys ".bold())
                .padding(Padding::horizontal(1)),
        ),
        rect,
    );
}

/// `path` with the home folder as `~`.
fn tilde(path: &str) -> String {
    match std::env::home_dir().map(|h| h.display().to_string()) {
        Some(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_owned(),
    }
}

/// Word-wraps `text` to `width` columns, and splits words that are too long.
fn wrap(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width);
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let mut word = word.to_owned();
            loop {
                let used = Span::raw(line.as_str()).width();
                let needed = Span::raw(word.as_str()).width() + usize::from(used > 0);
                if used + needed <= width {
                    if used > 0 {
                        line.push(' ');
                    }
                    line.push_str(&word);
                    break;
                }
                if used > 0 {
                    lines.push(std::mem::take(&mut line));
                    continue;
                }
                // The word alone is wider than the line
                let split = word
                    .char_indices()
                    .nth(width)
                    .map_or(word.len(), |(i, _)| i);
                lines.push(word[..split].to_owned());
                word = word[split..].to_owned();
                if word.is_empty() {
                    break;
                }
            }
        }
        lines.push(line);
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Removes terminal escape codes and other control characters, which would break the layout.
fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                // A CSI sequence ends at its first byte in @..~
                if chars.next_if_eq(&'[').is_some() {
                    while chars.next().is_some_and(|c| !('@'..='~').contains(&c)) {}
                }
            }
            '\n' => out.push('\n'),
            '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::Ci;
    use crate::pr::tests::pr;

    fn github() -> Source {
        Source::github("org:acme", None, None)
    }

    fn forgejo() -> Source {
        Source::forgejo("Forgejo", None, None)
    }

    /// An app with these sources, whose changes last until the test ends.
    fn app(group: Group, sources: Vec<Source>) -> (App, Receiver<Msg>) {
        let (tx, rx) = mpsc::channel();
        let options = Options {
            filter: Filter::default(),
            group,
            interval: Duration::from_secs(60),
        };
        let mut app = App::new(options, sources, Origin::Flags, tx);
        app.load = |_| Ok(Vec::new());
        app.logins = || vec!["home".into()];
        (app, rx)
    }

    fn selected_number(app: &App) -> Option<u64> {
        app.selected().map(|p| p.number)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn cursor_keeps_its_row_until_every_source_loads_then_follows_the_pr() {
        let (mut app, _rx) = app(Group::Repo, vec![github(), forgejo()]);
        app.feeds.iter_mut().for_each(|f| f.state = State::Loading);
        app.loaded(
            0,
            Ok(vec![
                pr("GitHub", "b", 1, "x", Status::Needs),
                pr("GitHub", "b", 2, "x", Status::Needs),
            ]),
        );
        app.move_cursor(1);
        assert_eq!(selected_number(&app), Some(2));

        // Forgejo's PR sorts above, so the row stays and the PR under it changes
        app.loaded(1, Ok(vec![pr("Forgejo", "a", 7, "x", Status::Needs)]));
        assert_eq!(app.table.selected(), Some(1));
        assert!(app.follow);

        // Now the cursor follows the PR when a filter moves the rows
        app.toggle_status(Status::Needs);
        app.feeds[1]
            .prs
            .push(pr("Forgejo", "a", 3, "x", Status::Needs));
        app.show();
        assert_eq!(selected_number(&app), Some(1));
        assert_eq!(app.table.selected(), Some(2));
    }

    #[test]
    fn a_failed_source_keeps_its_last_list() {
        let (mut app, _rx) = app(Group::Repo, vec![github()]);
        app.feeds[0].prs = vec![pr("GitHub", "a", 1, "x", Status::Needs)];
        app.feeds[0].state = State::Loading;
        app.loaded(0, Err("HTTP 502".into()));
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.toasts.len(), 1);
        assert_eq!(app.toasts[0].title, "GitHub failed");
        assert_eq!(app.feeds[0].state, State::Failed("HTTP 502".into()));
        assert!(app.updated.is_some());
    }

    #[test]
    fn a_load_for_a_source_that_changed_is_dropped() {
        let (mut app, _rx) = app(Group::Repo, vec![github(), forgejo()]);
        app.change(Change::Update(0, Source::github("org:other", None, None)))
            .unwrap();
        // 0 was the id before the change
        app.loaded(0, Ok(vec![pr("GitHub", "a", 1, "x", Status::Needs)]));
        assert!(app.rows.is_empty());
        assert_eq!(app.feeds[0].state, State::Loading);
    }

    #[test]
    fn subtitle_lists_the_filters_and_the_load_state() {
        let (mut app, _rx) = app(Group::User, vec![github(), forgejo()]);
        app.filter.mine = true;
        app.filter.status = Some(Status::Approved);
        app.feeds.iter_mut().for_each(|f| f.state = State::Loading);
        assert_eq!(
            app.subtitle(),
            "0 PRs · by user · mine · approved only · loading Forgejo and GitHub…"
        );
    }

    #[test]
    fn group_labels_show_once_per_group_and_leave_out_a_shared_owner() {
        let (mut app, _rx) = app(Group::Repo, vec![github(), forgejo()]);
        app.feeds[0].prs = vec![
            pr("GitHub", "a", 1, "x", Status::Needs),
            pr("GitHub", "a", 2, "x", Status::Needs),
            pr("GitHub", "b", 1, "x", Status::Needs),
        ];
        app.feeds[1].prs = vec![pr("Forgejo", "a", 1, "x", Status::Needs)];
        app.show();
        let first_column = |lines: &[String]| -> Vec<String> {
            lines[2..6]
                .iter()
                .map(|l| l.split("  ").next().unwrap().trim().to_owned())
                .collect()
        };
        // Forgejo's a and GitHub's a are separate groups
        assert_eq!(first_column(&screen(&mut app, 80, 8)), ["a", "a", "", "b"]);

        // With a second owner, every repo shows its owner
        app.feeds[1].prs[0].repo = "other/a".into();
        app.show();
        assert_eq!(
            first_column(&screen(&mut app, 80, 8)),
            ["acme/a", "", "acme/b", "other/a"]
        );
    }

    #[test]
    fn the_sources_panel_adds_edits_and_deletes() {
        let (mut app, _rx) = app(Group::Repo, vec![github()]);
        app.feeds[0].state = State::Failed("HTTP 401: Bad credentials\nTry gh auth login".into());
        app.on_key(key(KeyCode::Char('s')));
        let lines = screen(&mut app, 100, 16);
        let shows = |text: &str| lines.iter().any(|l| l.contains(text));
        assert!(shows("github.com: org:acme"));
        assert!(shows("changes last until you quit"));
        // Only the first line of the error fits
        assert!(shows("GitHub failed: HTTP 401: Bad credentials "));

        // Add a Forgejo source
        app.on_key(key(KeyCode::Char('a')));
        app.on_key(key(KeyCode::Right));
        for _ in 0..2 {
            app.on_key(key(KeyCode::Tab));
        }
        for c in "home".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(&app.mode, Mode::Sources(list) if list.cursor == 1));
        assert_eq!(app.feeds[1].source, Source::forgejo("home", None, None));
        assert_eq!(app.feeds[1].state, State::Loading);

        // Rename it
        app.on_key(key(KeyCode::Char('e')));
        for c in "Home".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.feeds[1].source.name(), "Home");

        // Delete the first one; d asks first, and another key keeps it
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Char('n')));
        assert_eq!(app.feeds.len(), 2);
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Char('y')));
        assert_eq!(app.feeds.len(), 1);
        assert_eq!(app.feeds[0].source.name(), "Home");

        // The last one stays
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Char('y')));
        assert_eq!(app.feeds.len(), 1);
        assert!(matches!(&app.mode, Mode::Sources(list) if list.message.is_some()));

        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Table));
    }

    #[test]
    fn wrap_breaks_on_words_and_splits_long_ones() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\nb", 10), ["a", "b"]);
        assert_eq!(wrap("", 10), [""]);
    }

    #[test]
    fn clean_removes_escape_codes() {
        assert_eq!(clean("\x1b[1mbold\x1b[0m\tx\r\ny"), "bold x\ny");
    }

    #[test]
    fn base64_matches_the_rfc_examples() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded);
        }
    }

    #[test]
    fn draw_shows_the_header_table_footer_and_toasts() {
        let (mut app, _rx) = app(Group::Repo, vec![github()]);
        app.feeds[0].prs = vec![
            pr("GitHub", "billing", 4012, "amy", Status::Needs),
            pr("GitHub", "billing", 4018, "me", Status::Approved),
            pr("GitHub", "web", 1207, "zed", Status::Changes),
        ];
        app.feeds[0].prs[0].ci = Some(Ci::Failing);
        app.feeds[0].prs[0].requested = true;
        app.feeds[0].prs[0].conflict = true;
        app.feeds[0].prs[1].ci = Some(Ci::Passed);
        app.feeds[0].prs[2].draft = true;
        app.show();
        app.notify(
            "GitHub failed".into(),
            "HTTP 502 from https://api.github.com/graphql, try again later".into(),
            true,
            ERROR_TIMEOUT,
        );
        let lines = screen(&mut app, 130, 12);
        assert!(lines[0].contains("Open PRs — 3 PRs · by repo"));
        assert!(lines[1].starts_with(
            "Repo     Host    #      Status             CI       Notes             Author"
        ));
        assert!(lines[2].starts_with(
            "billing  GitHub  #4012  needs review       failing  for you conflict  amy"
        ));
        assert!(lines[3].starts_with(
            "         GitHub  #4018  approved           passed                     me"
        ));
        // No checks leaves the CI cell blank
        assert!(lines[4].starts_with(
            "web      GitHub  #1207  changes requested           draft             zed"
        ));
        assert!(lines.iter().any(|l| l.contains(" GitHub failed ")));
        assert!(lines[11].starts_with(" o  Open"));
        assert!(lines[11].contains(" s  Sources"));
        assert!(lines[11].trim_end().ends_with(" ?  Help   q  Quit"));
    }

    #[test]
    fn the_footer_drops_keys_that_dont_fit_but_keeps_help_and_quit() {
        let text = |width| -> String {
            footer_keys(width)
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect()
        };
        // / Filter needs 12 columns, and 10 are left
        assert_eq!(text(40), " o  Open   ?  Help   q  Quit  ");
        assert_eq!(text(42), " o  Open   /  Filter   ?  Help   q  Quit  ");
        assert!(text(500).contains(" r  Reload  "));
        assert_eq!(text(5), " ?  Help   q  Quit  ");
    }

    #[test]
    fn question_mark_shows_every_key_until_any_key() {
        let (mut app, _rx) = app(Group::Repo, vec![github()]);
        app.on_key(key(KeyCode::Char('?')));
        let lines = screen(&mut app, 110, 30);
        assert!(lines.iter().any(|l| l.contains("Only PRs with failing CI")));
        app.on_key(key(KeyCode::Char('x')));
        assert!(matches!(app.mode, Mode::Table));
    }

    #[test]
    fn slash_filters_as_you_type_and_esc_clears() {
        let (mut app, _rx) = app(Group::Repo, vec![github()]);
        app.feeds[0].prs = [1, 2, 3]
            .map(|n| pr("GitHub", "a", n, "x", Status::Needs))
            .into();
        app.feeds[0].prs[1].title = "Fix the login page".into();
        app.show();
        app.on_key(key(KeyCode::Char('/')));
        // q types, and doesn't quit
        for c in "login q".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert!(!app.quit);
        assert!(app.rows.is_empty());
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.rows.len(), 1);
        let lines = screen(&mut app, 100, 8);
        assert!(lines[7].starts_with(" /  login █"));

        // enter keeps the words
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Table));
        assert_eq!(app.filter.text, "login ");
        assert!(app.subtitle().contains("matching \"login\""));

        // esc in the table clears them
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.rows.len(), 3);

        app.on_key(key(KeyCode::Char('/')));
        app.on_key(key(KeyCode::Char('x')));
        app.on_key(key(KeyCode::Esc));
        assert!(app.filter.text.is_empty());
        assert!(matches!(app.mode, Mode::Table));
    }

    #[test]
    fn w_f_and_d_toggle_their_filters() {
        let (mut app, _rx) = app(Group::Repo, vec![github()]);
        for c in ['w', 'f', 'd'] {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert!(app.filter.for_you && app.filter.failing && app.filter.hide_drafts);
        assert!(app.subtitle().contains("for you · failing CI · no drafts"));
        app.on_key(key(KeyCode::Char('f')));
        assert!(!app.filter.failing);
    }
}
