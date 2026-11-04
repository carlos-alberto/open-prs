use std::collections::{BTreeMap, BTreeSet};
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

use crate::pr::{Group, HOSTS, Host, Pr, Status, age, pick};
use crate::sources;

const TITLE: &str = "Open PRs";
const COPIED_TIMEOUT: Duration = Duration::from_secs(5);
const ERROR_TIMEOUT: Duration = Duration::from_secs(15);

const KEYS: [(&str, &str); 8] = [
    ("o", "Open"),
    ("y", "Copy URL"),
    ("r", "Reload"),
    ("m", "Mine"),
    ("a", "Approved"),
    ("n", "Needs review"),
    ("g", "Group"),
    ("q", "Quit"),
];

enum Msg {
    Input(io::Result<Event>),
    Loaded(Host, Result<Vec<Pr>, String>),
}

struct Toast {
    title: String,
    message: String,
    error: bool,
    until: Instant,
}

pub struct App {
    mine: bool,
    status: Option<Status>,
    group: Group,
    interval: Duration,
    prs: BTreeMap<Host, Vec<Pr>>,
    loading: BTreeSet<Host>,
    updated: Option<DateTime<Local>>,
    /// False until both hosts first load; see `show`
    follow: bool,
    /// The PRs on screen, in order
    rows: Vec<Pr>,
    table: TableState,
    /// Where the table was last drawn, to map mouse clicks to rows
    table_area: Rect,
    toasts: Vec<Toast>,
    tx: Sender<Msg>,
    quit: bool,
}

pub fn run(mine: bool, status: Option<Status>, group: Group, interval: Duration) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut app = App {
        mine,
        status,
        group,
        interval,
        prs: BTreeMap::new(),
        loading: BTreeSet::new(),
        updated: None,
        follow: false,
        rows: Vec::new(),
        table: TableState::new(),
        table_area: Rect::default(),
        toasts: Vec::new(),
        tx: tx.clone(),
        quit: false,
    };

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
                Ok(Msg::Loaded(host, result)) => self.loaded(host, result),
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

    fn reload(&mut self) {
        // A slow Forgejo load must not stack up behind the timer
        if !self.loading.is_empty() {
            return;
        }
        self.loading = HOSTS.into_iter().collect();
        self.show();
        // Show each host as soon as it answers; GitHub is one call, Forgejo is one per PR
        for host in HOSTS {
            let tx = self.tx.clone();
            thread::spawn(move || {
                let result = sources::fetch(host).map_err(|e| format!("{e:#}"));
                let _ = tx.send(Msg::Loaded(host, result));
            });
        }
    }

    fn loaded(&mut self, host: Host, result: Result<Vec<Pr>, String>) {
        self.loading.remove(&host);
        match result {
            Ok(prs) => {
                self.prs.insert(host, prs);
            }
            // Keep the last list from this host on screen
            Err(error) => self.notify(
                format!("{} failed", host.name()),
                error,
                true,
                ERROR_TIMEOUT,
            ),
        }
        if self.loading.is_empty() {
            self.updated = Some(Local::now());
        }
        self.show();
        if self.loading.is_empty() {
            self.follow = true;
        }
    }

    /// Picks the rows to show, and puts the cursor back.
    fn show(&mut self) {
        let current = self.selected().map(|p| p.url.clone());
        let row = self.table.selected().unwrap_or(0);
        self.rows = pick(
            self.prs.values().flatten(),
            self.mine,
            self.status,
            self.group,
        );
        if self.rows.is_empty() {
            self.table.select(None);
            return;
        }
        // Follow the PR under the cursor once both hosts have loaded. Before that, keep the
        // row, so the cursor doesn't follow GitHub's first PR down when Forgejo's arrive above it
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

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
            Event::Mouse(mouse) => self.on_mouse(mouse),
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let page = usize::from(self.table_area.height.saturating_sub(1)).max(1);
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Enter | KeyCode::Char('o') => self.open_pr(),
            KeyCode::Char('y') => self.copy_url(),
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char('m') => {
                self.mine = !self.mine;
                self.show();
            }
            KeyCode::Char('a') => self.toggle_status(Status::Approved),
            KeyCode::Char('n') => self.toggle_status(Status::Needs),
            KeyCode::Char('g') => {
                self.group = self.group.toggled();
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

    fn toggle_status(&mut self, status: Status) {
        // A PR can't be both approved and need review, so each filter replaces the other
        self.status = if self.status == Some(status) {
            None
        } else {
            Some(status)
        };
        self.show();
    }

    fn open_pr(&mut self) {
        let Some(url) = self.selected().map(|p| p.url.clone()) else {
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
        if self.mine {
            parts.push("mine".into());
        }
        if let Some(status) = self.status {
            parts.push(format!("{} only", status.label()));
        }
        if !self.loading.is_empty() {
            let mut names: Vec<_> = self.loading.iter().map(|h| h.name()).collect();
            names.sort();
            parts.push(format!("loading {}…", names.join(" and ")));
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

        let keys: Vec<Span> = KEYS
            .iter()
            .flat_map(|(key, label)| {
                [
                    Span::styled(format!(" {key} "), Style::new().bold().reversed()),
                    Span::raw(format!(" {label}  ")),
                ]
            })
            .collect();
        frame.render_widget(Line::from(keys), footer);

        self.draw_toasts(frame, body);
    }

    fn table_widget(&self, now: DateTime<Utc>) -> Table<'static> {
        let by_repo = self.group == Group::Repo;
        let headers: &[&str] = if by_repo {
            &["Repo", "Host", "#", "Status", "Author", "Updated", "Title"]
        } else {
            &["Author", "Host", "PR", "Status", "Updated", "Title"]
        };

        let mut cells: Vec<Vec<(String, Style)>> = Vec::with_capacity(self.rows.len());
        let mut last: Option<(&str, Option<Host>)> = None;
        for p in &self.rows {
            // Name each group once, so the blank cells under it show where it ends
            let group = if by_repo {
                (p.repo.as_str(), Some(p.host))
            } else {
                (p.author.as_str(), None)
            };
            let label = if Some(group) == last {
                String::new()
            } else if by_repo {
                p.short_repo().to_owned()
            } else {
                p.author.clone()
            };
            last = Some(group);

            let plain = Style::new();
            let mut row = vec![
                (label, plain),
                (p.host.name().to_owned(), Style::new().dim()),
            ];
            let status = (
                p.status.label().to_owned(),
                Style::new().fg(p.status.color()),
            );
            if by_repo {
                row.extend([
                    (format!("#{}", p.number), plain),
                    status,
                    (p.author.clone(), plain),
                ]);
            } else {
                row.extend([(format!("{}#{}", p.short_repo(), p.number), plain), status]);
            }
            row.extend([(age(p.updated, now), plain), (p.title.clone(), plain)]);
            cells.push(row);
        }

        // Size each column to fit, and give the rest of the width to the title
        let widths: Vec<Constraint> = (0..headers.len())
            .map(|i| {
                if i == headers.len() - 1 {
                    return Constraint::Fill(1);
                }
                let widest = cells
                    .iter()
                    .map(|row| Span::raw(row[i].0.as_str()).width())
                    .max()
                    .unwrap_or(0);
                Constraint::Length(widest.max(headers[i].len()) as u16)
            })
            .collect();

        let rows = cells.into_iter().map(|row| {
            Row::new(
                row.into_iter()
                    .map(|(text, style)| Cell::from(Span::styled(text, style))),
            )
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
    use crate::pr::tests::pr;

    fn app(group: Group) -> App {
        let (tx, _rx) = mpsc::channel();
        App {
            mine: false,
            status: None,
            group,
            interval: Duration::from_secs(60),
            prs: BTreeMap::new(),
            loading: BTreeSet::new(),
            updated: None,
            follow: false,
            rows: Vec::new(),
            table: TableState::new(),
            table_area: Rect::default(),
            toasts: Vec::new(),
            tx,
            quit: false,
        }
    }

    fn selected_number(app: &App) -> Option<u64> {
        app.selected().map(|p| p.number)
    }

    #[test]
    fn cursor_keeps_its_row_until_both_hosts_load_then_follows_the_pr() {
        let mut app = app(Group::Repo);
        app.loading = HOSTS.into_iter().collect();
        app.loaded(
            Host::GitHub,
            Ok(vec![
                pr(Host::GitHub, "b", 1, "x", Status::Needs),
                pr(Host::GitHub, "b", 2, "x", Status::Needs),
            ]),
        );
        app.move_cursor(1);
        assert_eq!(selected_number(&app), Some(2));

        // Forgejo's PR sorts above, so the row stays and the PR under it changes
        app.loaded(
            Host::Forgejo,
            Ok(vec![pr(Host::Forgejo, "a", 7, "x", Status::Needs)]),
        );
        assert_eq!(app.table.selected(), Some(1));
        assert!(app.follow);

        // Now the cursor follows the PR when a filter moves the rows
        app.toggle_status(Status::Needs);
        app.prs.get_mut(&Host::Forgejo).unwrap().push(pr(
            Host::Forgejo,
            "a",
            3,
            "x",
            Status::Needs,
        ));
        app.show();
        assert_eq!(selected_number(&app), Some(1));
        assert_eq!(app.table.selected(), Some(2));
    }

    #[test]
    fn a_failed_host_keeps_its_last_list() {
        let mut app = app(Group::Repo);
        app.prs.insert(
            Host::GitHub,
            vec![pr(Host::GitHub, "a", 1, "x", Status::Needs)],
        );
        app.loading = [Host::GitHub].into_iter().collect();
        app.loaded(Host::GitHub, Err("HTTP 502".into()));
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.toasts.len(), 1);
        assert_eq!(app.toasts[0].title, "GitHub failed");
        assert!(app.updated.is_some());
    }

    #[test]
    fn subtitle_lists_the_filters_and_the_load_state() {
        let mut app = app(Group::User);
        app.mine = true;
        app.status = Some(Status::Approved);
        app.loading = HOSTS.into_iter().collect();
        assert_eq!(
            app.subtitle(),
            "0 PRs · by user · mine · approved only · loading Forgejo and GitHub…"
        );
    }

    #[test]
    fn group_labels_show_once_per_group() {
        let mut app = app(Group::Repo);
        app.prs.insert(
            Host::GitHub,
            vec![
                pr(Host::GitHub, "a", 1, "x", Status::Needs),
                pr(Host::GitHub, "a", 2, "x", Status::Needs),
                pr(Host::GitHub, "b", 1, "x", Status::Needs),
            ],
        );
        app.prs.insert(
            Host::Forgejo,
            vec![pr(Host::Forgejo, "a", 1, "x", Status::Needs)],
        );
        app.show();

        let backend = ratatui::backend::TestBackend::new(80, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut state = TableState::new();
        terminal
            .draw(|f| f.render_stateful_widget(app.table_widget(Utc::now()), f.area(), &mut state))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let first_column: Vec<String> = (1..5)
            .map(|y| {
                (0..4)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim()
                    .to_owned()
            })
            .collect();
        // Forgejo's a and GitHub's a are separate groups
        assert_eq!(first_column, ["a", "a", "", "b"]);
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
        let mut app = app(Group::Repo);
        app.prs.insert(
            Host::GitHub,
            vec![
                pr(Host::GitHub, "billing", 4012, "amy", Status::Needs),
                pr(Host::GitHub, "billing", 4018, "me", Status::Approved),
                pr(Host::GitHub, "web", 1207, "zed", Status::Changes),
            ],
        );
        app.show();
        app.notify(
            "GitHub failed".into(),
            "HTTP 502 from https://api.github.com/graphql, try again later".into(),
            true,
            ERROR_TIMEOUT,
        );

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let lines: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect();
        assert!(lines[0].contains("Open PRs — 3 PRs · by repo"));
        assert!(lines[1].starts_with("Repo     Host    #      Status"));
        assert!(lines[2].starts_with("billing  GitHub  #4012  needs review"));
        assert!(lines[3].starts_with("         GitHub  #4018  approved"));
        assert!(lines.iter().any(|l| l.contains(" GitHub failed ")));
        assert!(lines[11].starts_with(" o  Open"));
    }
}
