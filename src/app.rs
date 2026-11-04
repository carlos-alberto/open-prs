use std::collections::{HashMap, HashSet};
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
use crate::notify::{self, Notice};
use crate::pr::{Filter, Group, Note, Pr, Status, age, pick, shared_owner};
use crate::preview::{self, Preview};
use crate::reviewers::{self, Picker, Reviewers};
use crate::seen::Seen;
use crate::sources;

const TITLE: &str = "Open PRs";
const COPIED_TIMEOUT: Duration = Duration::from_secs(5);
const ERROR_TIMEOUT: Duration = Duration::from_secs(15);

/// The keys in the footer, most useful first; it shows as many as fit.
const FOOTER: [(&str, &str); 18] = [
    ("o", "Open"),
    ("p", "Preview"),
    ("/", "Filter"),
    ("←→", "Columns"),
    ("c", "Checks"),
    ("u", "New"),
    ("v", "Reviewers"),
    ("w", "For you"),
    ("f", "Failing CI"),
    ("m", "Mine"),
    ("s", "Sources"),
    ("a", "Approved"),
    ("n", "Needs review"),
    ("d", "Drafts"),
    ("g", "Group"),
    ("space", "Seen"),
    ("y", "Copy URL"),
    ("r", "Reload"),
];

/// The keys the footer always shows, at its end.
const FOOTER_END: [(&str, &str); 2] = [("?", "Help"), ("q", "Quit")];

/// The fewest columns the title gets before the table saves space.
const TITLE_MIN: usize = 25;

/// How the table saves space: none of it hides anything, as ← and → scroll to what doesn't fit.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fit {
    host: bool,
    spacing: u16,
    /// "changes" for "changes requested"
    short_status: bool,
}

impl Default for Fit {
    fn default() -> Self {
        Fit {
            host: true,
            spacing: 2,
            short_status: false,
        }
    }
}

/// The table takes these in order while the title has less than `TITLE_MIN` columns.
const FIT_STEPS: [fn(&mut Fit); 2] = [|f| f.spacing = 1, |f| f.short_status = true];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Col {
    /// The repo, or the author when grouped by user
    Group,
    Host,
    Number,
    Status,
    Ci,
    Notes,
    Author,
    Comments,
    Updated,
    Title,
}

impl Col {
    fn header(self, by_repo: bool) -> &'static str {
        match self {
            Col::Group if by_repo => "Repo",
            Col::Group => "Author",
            Col::Host => "Host",
            Col::Number if by_repo => "#",
            Col::Number => "PR",
            Col::Status => "Status",
            Col::Ci => "CI",
            Col::Notes => "Notes",
            Col::Author => "Author",
            Col::Comments => "Comments",
            Col::Updated => "Updated",
            Col::Title => "Title",
        }
    }
}

/// The columns that always show, on the left: they say which PR a row is.
const FROZEN: [Col; 2] = [Col::Group, Col::Number];

impl Fit {
    /// The columns that scroll, in order; the title is last and always shows.
    fn scrolling(&self, by_repo: bool) -> Vec<Col> {
        [
            (self.host, Col::Host),
            (true, Col::Status),
            (true, Col::Ci),
            (true, Col::Notes),
            // Grouped by user, the author is the group
            (by_repo, Col::Author),
            (true, Col::Comments),
            (true, Col::Updated),
            (true, Col::Title),
        ]
        .into_iter()
        .filter_map(|(show, col)| show.then_some(col))
        .collect()
    }
}

/// The table's cells, and the width of each column but the title.
struct TableLayout {
    headers: Vec<String>,
    cells: Vec<Vec<Line<'static>>>,
    widths: Vec<usize>,
    spacing: u16,
}

impl TableLayout {
    /// The columns left for the title in `width`.
    fn title_room(&self, width: u16) -> usize {
        let used =
            self.widths.iter().sum::<usize>() + usize::from(self.spacing) * self.widths.len();
        usize::from(width).saturating_sub(used)
    }
}

/// Every key, for the help panel.
const HELP: [(&str, &str); 22] = [
    (
        "enter, o",
        "Open the PR in the browser, and mark it as seen",
    ),
    (
        "p",
        "Preview the PR's description, checks, files and comments, and mark it as seen",
    ),
    (
        "/",
        "Filter by words in the repo, number, title, author or host",
    ),
    ("esc", "Clear the words"),
    ("c", "Open the PR's CI runs"),
    ("v", "Ask people to review the PR, or stop asking them"),
    ("y", "Copy the PR's URL"),
    ("r", "Reload now"),
    ("m", "Only PRs you opened"),
    ("a", "Only approved PRs"),
    ("n", "Only PRs that need review"),
    ("w", "Only PRs that ask for your review"),
    ("u", "Only PRs that changed since you last saw them"),
    ("f", "Only PRs with failing CI"),
    ("d", "Hide drafts"),
    ("g", "Group by repo or by author"),
    ("s", "Add, edit and delete sources"),
    ("space", "Mark the PR as seen, or as new again"),
    ("↑ ↓ PgUp PgDn Home End", "Move the cursor"),
    ("← →", "Scroll the columns; the first two stay"),
    ("?", "Show this help"),
    ("q, ctrl+c", "Quit"),
];

const NOTES_HELP: &str = "Notes: new, the PR changed since you last saw it; for you, it asks \
    for your review; draft, it isn't ready; \
    conflict, it can't merge; on #N, it is stacked on PR N, which merges first. Grouped by repo, \
    a stacked PR shows under the PR it is on.";

enum Msg {
    Input(io::Result<Event>),
    /// The PRs from the feed with this id
    Loaded(u64, Result<Vec<Pr>, String>),
    /// The preview of the PR with this URL
    Previewed(String, Result<Preview, String>),
    /// Who reviews the PR with this URL, and who can
    ReviewersLoaded(String, Result<Reviewers, String>),
    /// The change to the reviewers of the PR with this URL is made, or failed
    ReviewersSaved(String, Result<(), String>),
}

/// A source and what it found.
struct Feed {
    /// Changes when the source does, so a load that started before the change is dropped
    id: u64,
    source: Source,
    prs: Vec<Pr>,
    state: State,
    /// Its next load is its first, which puts its PRs down as seen
    start: bool,
    /// It loaded since the source last changed, so a load can say what changed
    loaded: bool,
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
    Preview(Box<View>),
    Reviewers(Box<Picker>),
}

/// The preview of a PR, as it shows.
struct View {
    /// The PR as it was in the table
    pr: Pr,
    /// None while it loads the first time
    content: Option<Result<Preview, String>>,
    /// It shows an earlier preview while it loads again
    loading: bool,
    /// The first line that shows
    scroll: usize,
    /// The most `scroll` can be, from the last draw
    max_scroll: usize,
    /// The lines in a page, from the last draw
    page: usize,
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
    /// How many of the columns that scroll are hidden on the left
    scroll: usize,
    table: TableState,
    /// Where the table was last drawn, to map mouse clicks to rows
    table_area: Rect,
    toasts: Vec<Toast>,
    /// The last preview of each PR, to show while it loads again
    previews: HashMap<String, Preview>,
    seen: Seen,
    /// It said it could not save what you saw, so it doesn't say so on each change
    seen_failed: bool,
    tx: Sender<Msg>,
    quit: bool,
    /// Loads a source; the tests replace it, so they don't run gh or tea
    load: fn(&Source) -> Result<Vec<Pr>>,
    /// Loads the preview of a PR that this source found; the tests replace it too
    load_preview: fn(&Source, &Pr) -> Result<Preview>,
    /// Loads who reviews a PR and who can, with the people in the source's PRs
    load_reviewers: fn(&Source, &Pr, &[String]) -> Result<Reviewers>,
    /// Changes who a PR asks for a review
    save_reviewers: fn(&Source, &Pr, &reviewers::Change) -> Result<()>,
    /// Sends a desktop notification, or None not to; the tests replace it
    notifier: Option<fn(&Notice)>,
    /// The tea logins for the form's hint
    logins: fn() -> Vec<String>,
}

pub struct Options {
    pub filter: Filter,
    pub group: Group,
    pub interval: Duration,
    /// Send desktop notifications
    pub notify: bool,
}

pub fn run(options: Options, sources: Vec<Source>, origin: Origin, seen: Seen) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(options, sources, origin, seen, tx.clone());

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
    fn new(
        options: Options,
        sources: Vec<Source>,
        origin: Origin,
        seen: Seen,
        tx: Sender<Msg>,
    ) -> Self {
        // Once it knows the PRs, the ones that opened since it last ran are new
        let start = !seen.exists();
        let feeds = sources
            .into_iter()
            .zip(0..)
            .map(|(source, id)| Feed {
                id,
                source,
                prs: Vec::new(),
                state: State::Loaded,
                start,
                loaded: false,
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
            scroll: 0,
            table: TableState::new(),
            table_area: Rect::default(),
            toasts: Vec::new(),
            previews: HashMap::new(),
            seen,
            seen_failed: false,
            tx,
            quit: false,
            load: sources::fetch,
            load_preview: sources::preview,
            load_reviewers: sources::reviewers,
            save_reviewers: sources::request_reviews,
            notifier: options.notify.then_some(notify::send as fn(&Notice)),
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
                Ok(Msg::Previewed(url, result)) => self.previewed(url, result),
                Ok(Msg::ReviewersLoaded(url, result)) => self.reviewers_loaded(url, result),
                Ok(Msg::ReviewersSaved(url, result)) => self.reviewers_saved(url, result),
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
        let mut started = false;
        let mut notices = Vec::new();
        match result {
            Ok(prs) => {
                if feed.start {
                    self.seen.start(&prs);
                    feed.start = false;
                    started = true;
                }
                if feed.loaded {
                    notices = notify::changes(&feed.prs, &prs);
                }
                feed.prs = prs;
                feed.state = State::Loaded;
                feed.loaded = true;
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
        if started {
            self.save_seen();
        }
        self.announce(id, notices);
        self.show();
        if done {
            self.follow = true;
        }
    }

    /// Sends each notice, but not for a PR that an earlier source found too, as that source
    /// sends it.
    fn announce(&self, id: u64, notices: Vec<Notice>) {
        let Some(send) = self.notifier else {
            return;
        };
        let earlier: HashSet<&str> = self
            .feeds
            .iter()
            .take_while(|f| f.id != id)
            .flat_map(|f| &f.prs)
            .map(|p| p.url.as_str())
            .collect();
        for notice in notices.iter().filter(|n| !earlier.contains(n.url.as_str())) {
            send(notice);
        }
    }

    /// Picks the rows to show, and puts the cursor back.
    fn show(&mut self) {
        let current = self.selected().map(|p| p.url.clone());
        let row = self.table.selected().unwrap_or(0);
        for p in self.feeds.iter_mut().flat_map(|f| &mut f.prs) {
            p.new = self.seen.is_new(p);
        }
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

    /// Marks the PR under the cursor as seen, or as new again with `toggle` when it is seen.
    fn see(&mut self, toggle: bool) {
        if let Some(p) = self.selected().cloned() {
            self.see_pr(&p, toggle);
        }
    }

    fn see_pr(&mut self, p: &Pr, toggle: bool) {
        if toggle && !p.new {
            self.seen.unmark(p);
        } else {
            self.seen.mark(p);
        }
        self.save_seen();
        self.show();
    }

    fn save_seen(&mut self) {
        // A source that didn't load is missing its PRs, which must not count as closed
        let all_loaded = self.feeds.iter().all(|f| f.state == State::Loaded);
        let open: HashSet<&str> = self
            .feeds
            .iter()
            .flat_map(|f| &f.prs)
            .map(|p| p.url.as_str())
            .collect();
        let result = self.seen.save(all_loaded.then_some(&open), Utc::now());
        match result {
            Ok(()) => self.seen_failed = false,
            Err(e) if !self.seen_failed => {
                self.seen_failed = true;
                let message = format!("{e:#}");
                self.notify(
                    "Could not save what you saw".into(),
                    message,
                    true,
                    ERROR_TIMEOUT,
                );
            }
            Err(_) => {}
        }
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
                    start: true,
                    loaded: false,
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
                    start: true,
                    loaded: false,
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
            Event::Mouse(mouse) => {
                if let Mode::Preview(view) = &mut self.mode {
                    match mouse.kind {
                        MouseEventKind::ScrollDown => view.scroll_by(3),
                        MouseEventKind::ScrollUp => view.scroll_by(-3),
                        _ => {}
                    }
                }
            }
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
            Mode::Preview(view) => self.on_preview_key(view, key),
            Mode::Reviewers(picker) => self.on_reviewers_key(picker, key),
            // Any key closes the help
            Mode::Help => {}
        }
    }

    fn on_table_key(&mut self, key: KeyEvent) {
        let page = usize::from(self.table_area.height.saturating_sub(1)).max(1);
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Enter | KeyCode::Char('o') => self.open_pr(),
            KeyCode::Char('p') => self.preview(),
            KeyCode::Char('v') => self.pick_reviewers(),
            KeyCode::Char('c') => self.open(|p| &p.checks_url),
            KeyCode::Char('y') => self.copy_url(),
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char('m') => self.toggle(|f| &mut f.mine),
            KeyCode::Char('a') => self.toggle_status(Status::Approved),
            KeyCode::Char('n') => self.toggle_status(Status::Needs),
            KeyCode::Char('w') => self.toggle(|f| &mut f.for_you),
            KeyCode::Char('u') => self.toggle(|f| &mut f.new),
            KeyCode::Char(' ') => self.see(true),
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
            KeyCode::Right => self.scroll_columns(1),
            KeyCode::Left => self.scroll_columns(-1),
            KeyCode::PageDown => self.move_cursor(page as isize),
            KeyCode::PageUp => self.move_cursor(-(page as isize)),
            KeyCode::Home => self.move_cursor(isize::MIN),
            KeyCode::End => self.move_cursor(isize::MAX),
            _ => {}
        }
    }

    /// Shows the preview of the PR under the cursor, which counts as seeing it, and loads it
    /// again in the background.
    fn preview(&mut self) {
        let Some(p) = self.selected().cloned() else {
            return;
        };
        let Some((_, source)) = self.source_of(&p) else {
            return;
        };
        let (tx, load, pr) = (self.tx.clone(), self.load_preview, p.clone());
        thread::spawn(move || {
            let result = load(&source, &pr).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Previewed(pr.url, result));
        });
        self.see(false);
        self.mode = Mode::Preview(Box::new(View {
            content: self.previews.get(&p.url).cloned().map(Ok),
            loading: true,
            pr: p,
            scroll: 0,
            max_scroll: 0,
            page: 1,
        }));
    }

    /// The first source that found `p`, as in the table, and where it is in the feeds.
    fn source_of(&self, p: &Pr) -> Option<(usize, Source)> {
        self.feeds
            .iter()
            .position(|f| f.prs.iter().any(|q| q.url == p.url))
            .map(|i| (i, self.feeds[i].source.clone()))
    }

    /// Shows who reviews the PR under the cursor, and loads who can.
    fn pick_reviewers(&mut self) {
        let Some(p) = self.selected().cloned() else {
            return;
        };
        let Some((i, source)) = self.source_of(&p) else {
            return;
        };
        let mut known: Vec<String> = self.feeds[i].prs.iter().map(|q| q.author.clone()).collect();
        known.sort();
        known.dedup();
        let (tx, load, pr) = (self.tx.clone(), self.load_reviewers, p.clone());
        thread::spawn(move || {
            let result = load(&source, &pr, &known).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::ReviewersLoaded(pr.url, result));
        });
        self.mode = Mode::Reviewers(Box::new(Picker::new(p)));
    }

    fn on_reviewers_key(&mut self, mut picker: Box<Picker>, key: KeyEvent) {
        match picker.on_key(key) {
            reviewers::Action::None => {}
            reviewers::Action::Close => return,
            reviewers::Action::Save(change) => {
                let Some((_, source)) = self.source_of(&picker.pr) else {
                    picker.error = Some("The PR is gone from the table".into());
                    self.mode = Mode::Reviewers(picker);
                    return;
                };
                picker.saving = true;
                let (tx, save, pr) = (self.tx.clone(), self.save_reviewers, picker.pr.clone());
                thread::spawn(move || {
                    let result = save(&source, &pr, &change).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Msg::ReviewersSaved(pr.url, result));
                });
            }
        }
        self.mode = Mode::Reviewers(picker);
    }

    fn reviewers_loaded(&mut self, url: String, result: Result<Reviewers, String>) {
        if let Mode::Reviewers(picker) = &mut self.mode
            && picker.pr.url == url
        {
            picker.loaded(result);
        }
    }

    fn reviewers_saved(&mut self, url: String, result: Result<(), String>) {
        let Mode::Reviewers(picker) = &mut self.mode else {
            return;
        };
        if picker.pr.url != url {
            return;
        }
        picker.saving = false;
        if let Err(error) = result {
            picker.error = Some(error);
            return;
        }
        let done = picker.change().describe(true);
        let title = format!("Reviewers of {} #{}", picker.pr.repo, picker.pr.number);
        let pr = picker.pr.clone();
        self.mode = Mode::Table;
        // Capitalized, as it starts the message
        let mut message = done;
        if let Some(first) = message.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        self.notify(title, message, false, COPIED_TIMEOUT);
        // Load its source again, so the table shows the change
        if let Some((i, _)) = self.source_of(&pr)
            && self.feeds[i].state != State::Loading
        {
            self.fetch(i);
        }
    }

    fn previewed(&mut self, url: String, result: Result<Preview, String>) {
        if let Ok(preview) = &result {
            self.previews.insert(url.clone(), preview.clone());
        }
        let Mode::Preview(view) = &mut self.mode else {
            return;
        };
        if view.pr.url != url {
            return;
        }
        view.loading = false;
        match result {
            // Keep showing the last preview that loaded, and say why it is old
            Err(error) if matches!(view.content, Some(Ok(_))) => {
                self.notify(
                    "Could not load the preview".into(),
                    error,
                    true,
                    ERROR_TIMEOUT,
                );
            }
            result => view.content = Some(result),
        }
    }

    fn on_preview_key(&mut self, mut view: Box<View>, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('p' | 'q') => return,
            KeyCode::Down | KeyCode::Char('j') => view.scroll_by(1),
            KeyCode::Up | KeyCode::Char('k') => view.scroll_by(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => view.scroll_by(view.page as isize),
            KeyCode::PageUp => view.scroll_by(-(view.page as isize)),
            KeyCode::Home => view.scroll = 0,
            KeyCode::End => view.scroll = view.max_scroll,
            // These act on the PR in the preview, which can be gone from the table, as with u
            KeyCode::Enter | KeyCode::Char('o') => {
                self.open_url(view.pr.url.clone());
                self.see_pr(&view.pr, false);
            }
            KeyCode::Char('c') => self.open_url(view.pr.checks_url.clone()),
            KeyCode::Char('y') => self.copy(view.pr.url.clone()),
            // The next or previous PR in the table
            KeyCode::Right | KeyCode::Left => {
                let here = self.rows.iter().position(|p| p.url == view.pr.url);
                let next = match (here, key.code == KeyCode::Right) {
                    (Some(row), true) => Some(row + 1),
                    (Some(row), false) => row.checked_sub(1),
                    // It left the table, so the next PR is on the row it was on
                    (None, true) => self.table.selected(),
                    (None, false) => self.table.selected().and_then(|r| r.checked_sub(1)),
                };
                if let Some(next) = next.filter(|n| *n < self.rows.len()) {
                    self.table.select(Some(next));
                    self.preview();
                    return;
                }
            }
            _ => {}
        }
        self.mode = Mode::Preview(view);
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
            MouseEventKind::ScrollRight => self.scroll_columns(1),
            MouseEventKind::ScrollLeft => self.scroll_columns(-1),
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
                    self.open_pr();
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

    /// Opens the PR under the cursor in the browser, which counts as seeing it.
    fn open_pr(&mut self) {
        self.open(|p| &p.url);
        self.see(false);
    }

    /// Opens a page of the PR under the cursor in the browser.
    fn open(&mut self, page: fn(&Pr) -> &String) {
        if let Some(url) = self.selected().map(|p| page(p).clone()) {
            self.open_url(url);
        }
    }

    fn open_url(&mut self, url: String) {
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
        if let Some(url) = self.selected().map(|p| p.url.clone()) {
            self.copy(url);
        }
    }

    fn copy(&mut self, url: String) {
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
        let new = self.rows.iter().filter(|p| p.new).count();
        let count = match new {
            0 => format!("{} PRs", self.rows.len()),
            new => format!("{} PRs, {new} new", self.rows.len()),
        };
        let mut parts = vec![count, format!("by {}", self.group.name())];
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
        let table = self.table_widget(Utc::now(), body.width);
        frame.render_stateful_widget(table, body, &mut self.table);
        let footer_line = match &self.mode {
            Mode::Search => {
                let mut spans = vec![
                    Span::styled(" / ", Style::new().bold().reversed()),
                    Span::raw(format!(" {}", self.filter.text)),
                    Span::raw("█   "),
                ];
                spans.extend(manage::keys(&[("enter", "Keep"), ("esc", "Clear")]).spans);
                Line::from(spans)
            }
            Mode::Preview(_) => manage::keys(&[
                ("↑↓", "Scroll"),
                ("←→", "Other PRs"),
                ("o", "Open"),
                ("c", "Checks"),
                ("y", "Copy URL"),
                ("esc", "Close"),
            ]),
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
            Mode::Preview(_) => self.draw_preview(frame, body),
            Mode::Reviewers(picker) => picker.draw(frame, body),
        }
        self.draw_toasts(frame, body);
    }

    fn draw_preview(&mut self, frame: &mut Frame, area: Rect) {
        let Mode::Preview(view) = &mut self.mode else {
            return;
        };
        // All of it, as a description needs the room
        let rect = area;
        // Inside the borders and the padding
        let width = rect.width.saturating_sub(4).max(1);
        let height = usize::from(rect.height.saturating_sub(2));
        let lines = preview::lines(&view.pr, view.content.as_ref(), width, Utc::now());
        view.max_scroll = lines.len().saturating_sub(height);
        view.page = height.max(1);
        view.scroll = view.scroll.min(view.max_scroll);
        let mut title = vec![Span::styled(
            format!(" {} #{} ", view.pr.repo, view.pr.number),
            Style::new().bold(),
        )];
        if view.loading && view.content.is_some() {
            title.push(Span::styled("loading… ", Style::new().dim()));
        }
        // ↓ says there is more below
        let more = if view.scroll < view.max_scroll {
            Line::raw(" ↓ more ").right_aligned()
        } else {
            Line::default()
        };
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(lines)
                .scroll((view.scroll.min(usize::from(u16::MAX)) as u16, 0))
                .block(
                    Block::bordered()
                        .title(Line::from(title))
                        .title_bottom(more)
                        .padding(Padding::horizontal(1)),
                ),
            rect,
        );
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

    /// The table, with as much as fits in `width`; see `FIT_STEPS`.
    fn table_widget(&self, now: DateTime<Utc>, width: u16) -> Table<'static> {
        let layout = self.fit(now, width);
        let widths: Vec<Constraint> = layout
            .widths
            .iter()
            .map(|w| Constraint::Length(*w as u16))
            .chain([Constraint::Fill(1)])
            .collect();
        let rows = layout.cells.into_iter().zip(&self.rows).map(|(row, p)| {
            let row = Row::new(row.into_iter().map(Cell::from));
            // A draft isn't ready for review yet
            if p.draft { row.dim() } else { row }
        });
        Table::new(rows, widths)
            .header(Row::new(layout.headers).bold())
            .column_spacing(layout.spacing)
            .row_highlight_style(
                Style::new()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
    }

    /// Takes the steps in `FIT_STEPS` until the title has `TITLE_MIN` columns, or there are
    /// no more.
    fn fit(&self, now: DateTime<Utc>, width: u16) -> TableLayout {
        let mut fit = self.fit_start();
        let mut layout = self.layout(&fit, now);
        for step in FIT_STEPS {
            if layout.title_room(width) >= TITLE_MIN {
                break;
            }
            step(&mut fit);
            layout = self.layout(&fit, now);
        }
        layout
    }

    fn fit_start(&self) -> Fit {
        let hosts: HashSet<&str> = self.rows.iter().map(|p| p.host.as_str()).collect();
        Fit {
            // With one host, the column says nothing
            host: hosts.len() > 1,
            ..Fit::default()
        }
    }

    /// How many columns → can hide, as the title always shows.
    fn max_scroll(&self) -> usize {
        self.fit_start().scrolling(self.group == Group::Repo).len() - 1
    }

    fn scroll_columns(&mut self, by: isize) {
        self.scroll = self.scroll.saturating_add_signed(by).min(self.max_scroll());
    }

    fn layout(&self, fit: &Fit, now: DateTime<Utc>) -> TableLayout {
        let by_repo = self.group == Group::Repo;
        let owner = self.owner.as_deref();
        let scrolling = fit.scrolling(by_repo);
        let scroll = self.scroll.min(scrolling.len() - 1);
        let cols: Vec<Col> = FROZEN
            .into_iter()
            .chain(scrolling[scroll..].iter().copied())
            .collect();
        let headers: Vec<String> = cols
            .iter()
            .enumerate()
            .map(|(i, c)| {
                // ‹ says columns are hidden to the left
                if i == FROZEN.len() && scroll > 0 {
                    format!("‹ {}", c.header(by_repo))
                } else {
                    c.header(by_repo).to_owned()
                }
            })
            .collect();

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

            let row = cols
                .iter()
                .map(|col| match col {
                    Col::Group => Line::raw(label.clone()),
                    Col::Host => Line::raw(p.host.clone()).dim(),
                    Col::Number if by_repo => Line::from(vec![
                        Span::styled(p.tree.clone(), Style::new().dim()),
                        Span::raw(format!("#{}", p.number)),
                    ]),
                    Col::Number => Line::raw(format!("{}#{}", p.repo_without(owner), p.number)),
                    Col::Status => {
                        let label = if fit.short_status {
                            p.status.short_label()
                        } else {
                            p.status.label()
                        };
                        Line::styled(label, Style::new().fg(p.status.color()))
                    }
                    // Blank when the last commit has no checks
                    Col::Ci => p.ci.map_or_else(Line::default, |ci| {
                        Line::styled(ci.label(), Style::new().fg(ci.color()))
                    }),
                    // The tree shows what the PR is stacked on, when it can
                    Col::Notes => Line::from(
                        p.notes()
                            .into_iter()
                            .filter(|note| !matches!(note, Note::On(_)) || p.tree.is_empty())
                            .enumerate()
                            .flat_map(|(i, note)| {
                                let gap = Span::raw(if i == 0 { "" } else { " " });
                                [gap, Span::styled(note.label(), note.style())]
                            })
                            .collect::<Vec<_>>(),
                    ),
                    Col::Author => Line::raw(p.author.clone()),
                    // Blank for none, so the PRs with comments stand out
                    Col::Comments => match p.comments {
                        0 => Line::default(),
                        n => Line::raw(n.to_string()).right_aligned(),
                    },
                    Col::Updated => Line::raw(age(p.updated, now)),
                    // Like an unread email
                    Col::Title if p.new => Line::raw(p.title.clone()).bold(),
                    Col::Title => Line::raw(p.title.clone()),
                })
                .collect();
            cells.push(row);
        }

        // Each column but the title is as wide as its widest cell or header
        let widths = (0..cols.len() - 1)
            .map(|i| {
                let widest = cells.iter().map(|row| row[i].width()).max().unwrap_or(0);
                widest.max(Span::raw(headers[i].as_str()).width())
            })
            .collect();
        TableLayout {
            headers,
            cells,
            widths,
            spacing: fit.spacing,
        }
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

impl View {
    fn scroll_by(&mut self, by: isize) {
        self.scroll = self.scroll.saturating_add_signed(by).min(self.max_scroll);
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
    // Inside the borders and the padding
    let width = 100.min(area.width);
    let notes = wrap(NOTES_HELP, width.saturating_sub(4).max(1));
    lines.extend(notes.into_iter().map(|line| Line::from(line).dim()));
    lines.push(Line::from("Any key closes this.").dim());
    let rect = manage::popup(area, width, lines.len() as u16 + 2);
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
pub(crate) fn wrap(text: &str, width: u16) -> Vec<String> {
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
pub(crate) fn clean(text: &str) -> String {
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
            notify: false,
        };
        let mut app = App::new(options, sources, Origin::Flags, Seen::memory(), tx);
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
    fn a_pr_is_new_after_the_first_load_until_you_see_it() {
        let (mut app, _rx) = app(Group::Repo, vec![github(), forgejo()]);
        let first = pr("GitHub", "a", 1, "x", Status::Needs);
        app.loaded(0, Ok(vec![first.clone()]));
        app.loaded(1, Ok(vec![pr("Forgejo", "b", 9, "x", Status::Needs)]));
        assert!(app.rows.iter().all(|p| !p.new), "the first loads are seen");

        // A PR that opened since, and one that changed since
        let mut changed = first.clone();
        changed.updated += chrono::TimeDelta::minutes(5);
        let opened = pr("GitHub", "a", 2, "x", Status::Needs);
        app.loaded(0, Ok(vec![changed, opened]));
        let new: Vec<u64> = app
            .rows
            .iter()
            .filter(|p| p.new)
            .map(|p| p.number)
            .collect();
        assert_eq!(new, [1, 2]);
        assert!(app.subtitle().starts_with("3 PRs, 2 new · "));
        assert_eq!(app.rows[0].notes()[0], Note::New);

        // u shows only those, and seeing one takes it out
        app.on_key(key(KeyCode::Char('u')));
        assert_eq!(app.rows.len(), 2);
        app.see(false);
        assert_eq!(app.rows.len(), 1);
        assert_eq!(selected_number(&app), Some(2));

        // space marks it as seen, and as new again
        app.on_key(key(KeyCode::Char('u')));
        app.on_key(key(KeyCode::Char(' ')));
        assert!(!app.rows.iter().any(|p| p.number == 2 && p.new));
        app.on_key(key(KeyCode::Char(' ')));
        assert!(app.rows.iter().any(|p| p.number == 2 && p.new));
    }

    #[test]
    fn a_new_source_starts_with_its_prs_seen() {
        let (mut app, _rx) = app(Group::Repo, vec![github()]);
        app.loaded(0, Ok(vec![pr("GitHub", "a", 1, "x", Status::Needs)]));
        app.change(Change::Add(forgejo())).unwrap();
        app.loaded(1, Ok(vec![pr("Forgejo", "b", 9, "x", Status::Needs)]));
        assert!(app.rows.iter().all(|p| !p.new));
        app.loaded(1, Ok(vec![pr("Forgejo", "b", 10, "x", Status::Needs)]));
        assert!(app.rows.iter().any(|p| p.number == 10 && p.new));
    }

    #[test]
    fn p_previews_the_pr_and_the_arrows_move_to_the_next() {
        let (mut app, rx) = app(Group::Repo, vec![github()]);
        app.load_preview = |_, p| {
            Ok(Preview {
                body: format!("About #{}", p.number),
                ..Preview::default()
            })
        };
        app.loaded(
            0,
            Ok(vec![
                pr("GitHub", "a", 1, "x", Status::Needs),
                pr("GitHub", "a", 2, "x", Status::Needs),
            ]),
        );
        let answer = |app: &mut App| match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Msg::Previewed(url, result) => app.previewed(url, result),
            _ => panic!("expected a preview"),
        };
        let text = |app: &mut App| screen(app, 80, 20).join("\n");

        app.on_key(key(KeyCode::Char('p')));
        assert!(text(&mut app).contains("Loading…"));
        answer(&mut app);
        let shown = text(&mut app);
        assert!(shown.contains(" acme/a #1 "), "{shown}");
        assert!(shown.contains("About #1"), "{shown}");
        assert!(shown.contains(" esc  Close"), "{shown}");

        app.on_key(key(KeyCode::Right));
        answer(&mut app);
        assert!(text(&mut app).contains("About #2"));
        assert_eq!(selected_number(&app), Some(2));

        // esc closes it, and p shows the last preview at once while it loads again
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Table));
        app.move_cursor(-1);
        app.on_key(key(KeyCode::Char('p')));
        let shown = text(&mut app);
        assert!(
            shown.contains("About #1") && shown.contains("loading…"),
            "{shown}"
        );
        answer(&mut app);
        assert!(!text(&mut app).contains("loading…"));
    }

    /// Handles the next message from a background load or save.
    fn answer(app: &mut App, rx: &Receiver<Msg>) {
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Msg::Loaded(id, result) => app.loaded(id, result),
            Msg::Previewed(url, result) => app.previewed(url, result),
            Msg::ReviewersLoaded(url, result) => app.reviewers_loaded(url, result),
            Msg::ReviewersSaved(url, result) => app.reviewers_saved(url, result),
            Msg::Input(_) => panic!("no input in tests"),
        }
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    fn reviewers_app() -> (App, Receiver<Msg>) {
        let (mut app, rx) = app(Group::Repo, vec![github()]);
        app.load_reviewers = |_, _, known| {
            Ok(Reviewers {
                requested: vec!["chidi".into()],
                people: known.to_vec(),
                verdicts: HashMap::new(),
            })
        };
        app.loaded(
            0,
            Ok(vec![
                pr("GitHub", "a", 1, "amy", Status::Needs),
                pr("GitHub", "a", 2, "bao", Status::Needs),
            ]),
        );
        (app, rx)
    }

    #[test]
    fn v_asks_for_a_review_and_loads_the_source_again() {
        let (mut app, rx) = reviewers_app();
        app.save_reviewers = |_, _, change| {
            assert_eq!(change.add, ["bao"]);
            Ok(())
        };
        app.on_key(key(KeyCode::Char('v')));
        assert!(screen(&mut app, 80, 24).join("\n").contains("Loading…"));
        answer(&mut app, &rx);
        let shown = screen(&mut app, 80, 24).join("\n");
        assert!(shown.contains(" Reviewers of acme/a #1 "), "{shown}");
        assert!(shown.contains("│ ✓ chidi  asked"), "{shown}");

        type_text(&mut app, "ba");
        app.on_key(key(KeyCode::Enter));
        assert!(screen(&mut app, 80, 24).join("\n").contains("Saving…"));
        answer(&mut app, &rx);
        assert!(matches!(app.mode, Mode::Table));
        assert_eq!(app.toasts[0].title, "Reviewers of acme/a #1");
        assert_eq!(app.toasts[0].message, "Asked bao");
        assert_eq!(app.feeds[0].state, State::Loading);
    }

    #[test]
    fn a_failed_save_shows_why_and_keeps_the_panel() {
        let (mut app, rx) = reviewers_app();
        app.save_reviewers =
            |_, _, _| anyhow::bail!("Reviews may only be requested from collaborators");
        app.on_key(key(KeyCode::Char('v')));
        answer(&mut app, &rx);
        type_text(&mut app, "zed");
        let shown = screen(&mut app, 80, 24).join("\n");
        assert!(shown.contains("  zed (as typed)"), "{shown}");
        app.on_key(key(KeyCode::Enter));
        answer(&mut app, &rx);
        let shown = screen(&mut app, 80, 24).join("\n");
        assert!(matches!(app.mode, Mode::Reviewers(_)));
        assert!(
            shown.contains("Reviews may only be requested from"),
            "{shown}"
        );
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Table));
    }

    thread_local! {
        static SENT: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    #[test]
    fn a_reload_notifies_what_changed_once_per_pr() {
        let (mut app, _rx) = app(Group::Repo, vec![github(), forgejo()]);
        app.notifier = Some(|n| SENT.with(|s| s.borrow_mut().push(n.title.clone())));
        // The first loads say nothing, and the second source finds the same PR
        let mine = pr("GitHub", "a", 1, "me", Status::Needs);
        app.loaded(0, Ok(vec![mine.clone()]));
        app.loaded(1, Ok(vec![mine.clone()]));
        let mut approved = mine.clone();
        approved.status = Status::Approved;
        app.loaded(0, Ok(vec![approved.clone()]));
        app.loaded(1, Ok(vec![approved]));
        SENT.with(|s| assert_eq!(*s.borrow(), ["Approved"]));
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
        let lines = screen(&mut app, 120, 8);
        assert_eq!(first_column(&lines), ["a", "a", "", "b"]);
        // Two hosts, so the Host column shows
        assert!(lines[1].starts_with("Repo  #   Host     Status"));

        // With a second owner, every repo shows its owner
        app.feeds[1].prs[0].repo = "other/a".into();
        app.show();
        assert_eq!(
            first_column(&screen(&mut app, 120, 8)),
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
        app.feeds[0].prs[0].comments = 12;
        app.show();
        app.notify(
            "GitHub failed".into(),
            "HTTP 502 from https://api.github.com/graphql, try again later".into(),
            true,
            ERROR_TIMEOUT,
        );
        let lines = screen(&mut app, 130, 12);
        assert!(lines[0].contains("Open PRs — 3 PRs · by repo"));
        // Every PR is from one host, so there is no Host column
        assert!(
            lines[1].starts_with(
                "Repo     #      Status             CI       Notes             Author"
            )
        );
        assert!(
            lines[2]
                .starts_with("billing  #4012  needs review       failing  for you conflict  amy")
        );
        assert!(
            lines[3]
                .starts_with("         #4018  approved           passed                     me")
        );
        // No checks leaves the CI cell blank
        assert!(
            lines[4]
                .starts_with("web      #1207  changes requested           draft             zed")
        );
        // Comments are right-aligned under their 8-wide header, and blank for none
        assert!(lines[1].contains("Author  Comments  Updated"));
        assert!(lines[2].contains(&format!("amy{}12  2965w", " ".repeat(11))));
        assert!(lines[3].contains(&format!("me{}2965w", " ".repeat(16))));
        assert!(lines.iter().any(|l| l.contains(" GitHub failed ")));
        assert!(lines[11].starts_with(" o  Open"));
        assert!(lines[11].contains(" p  Preview"));
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
        // p Preview needs 13 columns, and 10 are left
        assert_eq!(text(42), " o  Open   ?  Help   q  Quit  ");
        assert_eq!(text(43), " o  Open   p  Preview   ?  Help   q  Quit  ");
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
    fn the_notes_in_the_help_wrap_to_its_width() {
        let (mut app, _rx) = app(Group::Repo, vec![github()]);
        app.on_key(key(KeyCode::Char('?')));
        let text = screen(&mut app, 80, 40).join("\n");
        assert!(text.contains("│ Notes: new, the PR changed"), "{text}");
        assert!(text.contains("│ under the PR it is on."), "{text}");
    }

    #[test]
    fn a_stack_shows_as_a_tree_by_repo_and_as_notes_by_user() {
        use crate::pr::tests::on;
        let (mut app, _rx) = app(Group::Repo, vec![github()]);
        let pr = |n, author| pr("GitHub", "app", n, author, Status::Needs);
        app.feeds[0].prs = vec![
            pr(1, "amy"),
            on(pr(2, "amy"), 1),
            on(pr(3, "zed"), 2),
            on(pr(4, "amy"), 1),
        ];
        app.show();
        let lines = screen(&mut app, 110, 8);
        // Repo is 4 wide, with 2 before the next column, so # starts at character 6
        let numbers: Vec<String> = lines[2..6]
            .iter()
            .map(|l| {
                l.chars()
                    .skip(6)
                    .take(8)
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect();
        assert_eq!(numbers, ["#1", "├─ #2", "│  └─ #3", "└─ #4"]);
        assert!(!lines[2..6].iter().any(|l| l.contains("on #")));

        app.on_key(key(KeyCode::Char('g')));
        let lines = screen(&mut app, 110, 8);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("app#2") && l.contains("on #1"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("app#3") && l.contains("on #2"))
        );
    }

    #[test]
    fn a_narrow_table_saves_space_then_scrolls_with_the_arrows() {
        let (mut app, _rx) = app(Group::Repo, vec![github()]);
        let mut p = pr("GitHub", "billing", 4012, "AlexandraNoble", Status::Changes);
        p.title = "Show the new invoice page to the beta testers".into();
        p.ci = Some(Ci::Passed);
        app.feeds[0].prs = vec![p];
        app.show();

        // Wide: two spaces between columns, and the whole status
        let wide = screen(&mut app, 140, 6);
        assert!(wide[1].starts_with("Repo     #      Status             CI      Notes  Author"));
        assert!(wide[2].contains("changes requested"));

        // Narrow: one space between columns, and the short status, but every column stays
        let narrow = screen(&mut app, 80, 6);
        assert!(narrow[1].starts_with(
            "Repo    #     Status  CI     Notes Author         Comments Updated Title"
        ));
        assert!(narrow[2].contains("changes "));
        assert!(!narrow[2].contains("changes requested"));
        assert!(!narrow[2].contains("Show the new invoice page to"));

        // → hides Status, and the frozen Repo and # stay
        app.on_key(key(KeyCode::Right));
        let lines = screen(&mut app, 80, 6);
        assert!(lines[1].starts_with("Repo    #     ‹ CI   Notes Author"));
        assert!(!lines[2].contains("changes"));

        // As far as it goes, only the title is left, with all its room
        for _ in 0..20 {
            app.on_key(key(KeyCode::Right));
        }
        assert_eq!(app.scroll, 6);
        let lines = screen(&mut app, 80, 6);
        assert!(lines[1].starts_with("Repo     #      ‹ Title"));
        assert!(lines[2].contains("Show the new invoice page to the beta testers"));

        // ← brings the columns back
        for _ in 0..20 {
            app.on_key(key(KeyCode::Left));
        }
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn grouped_by_user_there_is_no_author_column_to_scroll() {
        let (mut app, _rx) = app(Group::User, vec![github(), forgejo()]);
        app.feeds[0].prs = vec![pr("GitHub", "a", 1, "x", Status::Needs)];
        app.feeds[1].prs = vec![pr("Forgejo", "a", 1, "x", Status::Needs)];
        app.show();
        // Host, Status, CI, Notes, Comments, Updated hide; the title stays
        assert_eq!(app.max_scroll(), 6);
        app.group = Group::Repo;
        app.show();
        assert_eq!(app.max_scroll(), 7);
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

#[cfg(test)]
#[path = "screenshots.rs"]
mod screenshots;
