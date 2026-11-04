//! The screenshots in the README: the real TUI, drawn with made-up PRs, saved as SVG.
//!
//! cargo test screenshots -- --ignored

use std::fmt::Write as _;
use std::fs;

use chrono::{Duration as Ago, Local, TimeZone};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

use super::*;
use crate::pr::Ci;
use crate::pr::tests::{on, pr};
use crate::preview::{Check, Comment, FileChange, Files};

/// Sources and PRs for a made-up org, acme.
fn demo() -> App {
    let (tx, _rx) = mpsc::channel();
    let sources = vec![
        Source::github("org:acme", None, None),
        Source::forgejo("work", Some("acme".into()), Some("Forgejo".into())),
        Source::github("org:acme-labs", Some("ghe.acme.dev".into()), None),
    ];
    let config = std::env::home_dir()
        .unwrap_or_default()
        .join(".config/open_prs/config.toml");
    let options = Options {
        filter: Filter::default(),
        group: Group::Repo,
        interval: Duration::from_secs(60),
    };
    let mut app = App::new(options, sources, Origin::File(config), Seen::memory(), tx);
    app.load = |_| Ok(Vec::new());
    app.load_preview = |_, _| Ok(preview(Utc::now()));
    app.load_reviewers = |_, _, _| Ok(Reviewers::default());
    app.save_reviewers = |_, _, _| Ok(());
    app.logins = || vec!["work".into()];

    let now = Utc::now();
    let make = |host: &str, repo: &str, number: u64, author: &str, status: Status, title: &str| {
        let mut p = pr(host, repo, number, author, status);
        p.title = title.into();
        p.mine = author == "sam";
        p
    };
    let mut github = vec![
        make(
            "GitHub",
            "api",
            412,
            "amy",
            Status::Approved,
            "Add rate limits to the public API",
        ),
        on(
            make(
                "GitHub",
                "api",
                418,
                "amy",
                Status::Needs,
                "Return Retry-After with each 429",
            ),
            412,
        ),
        on(
            make(
                "GitHub",
                "api",
                421,
                "bao",
                Status::Needs,
                "Document the rate limits",
            ),
            418,
        ),
        make(
            "GitHub",
            "billing",
            88,
            "chidi",
            Status::Changes,
            "Move invoices to the new tax service",
        ),
        make(
            "GitHub",
            "billing",
            91,
            "dependabot",
            Status::Approved,
            "Bump stripe from 14.2.0 to 14.3.0",
        ),
        make(
            "GitHub",
            "web",
            1290,
            "sam",
            Status::Needs,
            "Show the new dashboard to everyone",
        ),
        on(
            make(
                "GitHub",
                "web",
                1291,
                "sam",
                Status::Needs,
                "Remove the old dashboard",
            ),
            1290,
        ),
        make(
            "GitHub",
            "web",
            1302,
            "bao",
            Status::Needs,
            "Fix the date picker in Safari",
        ),
    ];
    let details: [(Option<Ci>, u64, i64, bool, bool); 8] = [
        // CI, comments, minutes ago, for you, conflict
        (Some(Ci::Passed), 4, 50, false, false),
        (Some(Ci::Pending), 1, 12, true, false),
        (Some(Ci::Passed), 0, 3, true, false),
        (Some(Ci::Failing), 12, 60 * 26, false, true),
        (Some(Ci::Passed), 0, 60 * 24 * 3, false, false),
        (Some(Ci::Passed), 7, 90, false, false),
        (Some(Ci::Failing), 2, 85, false, false),
        (Some(Ci::Passed), 3, 60 * 5, true, false),
    ];
    for (p, (ci, comments, minutes, for_you, conflict)) in github.iter_mut().zip(details) {
        p.ci = ci;
        p.comments = comments;
        p.updated = now - Ago::minutes(minutes);
        p.requested = for_you;
        p.conflict = conflict;
    }
    let mut forgejo = vec![
        make(
            "Forgejo",
            "docs",
            23,
            "dana",
            Status::Needs,
            "WIP: Rewrite the quick start guide",
        ),
        make(
            "Forgejo",
            "docs",
            24,
            "erin",
            Status::Approved,
            "Fix the broken links in the FAQ",
        ),
    ];
    forgejo[0].draft = true;
    forgejo[0].comments = 2;
    forgejo[0].updated = now - Ago::days(9);
    forgejo[1].ci = Some(Ci::Passed);
    forgejo[1].comments = 1;
    forgejo[1].updated = now - Ago::hours(20);
    for p in &mut forgejo {
        p.url = p.url.replace("/pull/", "/pulls/");
    }

    app.feeds[0].prs = github;
    app.feeds[1].prs = forgejo;
    // As after the first loads, and you last saw three of them before they changed
    let all: Vec<Pr> = app.feeds.iter().flat_map(|f| f.prs.clone()).collect();
    app.seen.start(&all);
    for p in all.iter().filter(|p| [418, 88, 1302].contains(&p.number)) {
        app.seen.unmark(p);
    }
    app.feeds[2].state =
        State::Failed("HTTP 401: Bad credentials (https://ghe.acme.dev/api/graphql)".into());
    app.updated = Local.with_ymd_and_hms(2026, 11, 4, 9, 41, 27).single();
    app.follow = true;
    app.show();
    app.table.select(Some(1));
    app
}

#[test]
#[ignore = "writes the README's screenshots to docs/"]
fn screenshots() {
    let mut app = demo();
    save("table", &mut app, 140, 15);

    app.on_key(key('s'));
    if let Mode::Sources(list) = &mut app.mode {
        list.cursor = 2;
    }
    save("sources", &mut app, 140, 15);
    app.mode = Mode::Table;

    for _ in 0..3 {
        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    }
    save("narrow", &mut app, 92, 15);
    app.scroll = 0;

    // api #418, with what the host says about it, as if it loaded
    app.on_key(key('p'));
    if let Mode::Preview(view) = &mut app.mode {
        view.content = Some(Ok(preview(Utc::now())));
        view.loading = false;
    }
    save("preview", &mut app, 100, 30);

    // Who api #418 asks, and a team ticked
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.on_key(key('v'));
    let url = app.selected().unwrap().url.clone();
    let reviewers = Reviewers {
        requested: vec!["bao".into()],
        people: [
            "chidi",
            "acme/backend",
            "acme/web",
            "bao",
            "dana",
            "erin",
            "sam",
        ]
        .map(String::from)
        .into(),
        verdicts: [("chidi".to_owned(), "commented")].into(),
    };
    app.reviewers_loaded(url, Ok(reviewers));
    for _ in 0..2 {
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    app.on_key(key(' '));
    save("reviewers", &mut app, 100, 18);
}

/// What the host says about api #418.
fn preview(now: DateTime<Utc>) -> Preview {
    let comment = |author: &str, minutes, body: &str, verdict| Comment {
        author: author.into(),
        when: now - Ago::minutes(minutes),
        body: body.into(),
        verdict,
    };
    let file = |path: &str, additions, deletions| FileChange {
        path: path.into(),
        additions,
        deletions,
    };
    let check = |name: &str, state| Check {
        name: name.into(),
        state,
    };
    Preview {
        body: "<!-- What does this change, and why? -->\n\
               A client that hits the rate limit now gets a Retry-After header with its 429, \
               so it knows when to try again.\n\n\
               - The value is the time until the bucket has a token again, in whole seconds\n\
               - The docs come in #421"
            .into(),
        files: Some(Files {
            additions: 48,
            deletions: 6,
            count: 3,
            changed: vec![
                file("src/limits/bucket.rs", 31, 4),
                file("src/limits/middleware.rs", 12, 2),
                file("tests/limits.rs", 5, 0),
            ],
        }),
        checks: vec![
            check("lint", Ci::Passed),
            check("unit", Ci::Passed),
            check("e2e (chrome)", Ci::Pending),
        ],
        comments: vec![
            comment(
                "chidi",
                40,
                "Seconds or an HTTP date? Seconds is easier for clients.",
                Some("reviewed"),
            ),
            comment(
                "amy",
                30,
                "Seconds. RFC 9110 allows both, and the clients we know read seconds.",
                None,
            ),
            comment(
                "bao",
                12,
                "Can we test the rounding? 0.2 seconds should give 1.",
                None,
            ),
        ],
    }
}

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn save(name: &str, app: &mut App, width: u16, height: u16) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| app.draw(f)).unwrap();
    fs::create_dir_all("docs").unwrap();
    fs::write(format!("docs/{name}.svg"), svg(terminal.backend().buffer())).unwrap();
}

const CELL_WIDTH: f32 = 8.4;
const LINE_HEIGHT: f32 = 18.0;
const PAD: f32 = 14.0;
const STROKE: f32 = 1.2;
const BAR: f32 = 30.0;
const FG: &str = "#c8d3f5";
const BG: &str = "#222436";

fn color(color: Color, default: &str) -> String {
    match color {
        Color::Reset => default,
        Color::Black => "#1b1d2b",
        Color::Red | Color::LightRed => "#ff757f",
        Color::Green | Color::LightGreen => "#c3e88d",
        Color::Yellow | Color::LightYellow => "#ffc777",
        Color::Blue | Color::LightBlue => "#82aaff",
        Color::Magenta | Color::LightMagenta => "#fca7ea",
        Color::Cyan | Color::LightCyan => "#86e1fc",
        Color::Gray => "#828bb8",
        Color::DarkGray => "#3b4261",
        Color::White => "#ffffff",
        Color::Rgb(r, g, b) => return format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(_) => default,
    }
    .into()
}

/// The look of a cell, with reversed colors swapped.
#[derive(PartialEq)]
struct Look {
    fg: String,
    bg: Option<String>,
    bold: bool,
    dim: bool,
}

fn look(cell: &ratatui::buffer::Cell) -> Look {
    let (mut fg, mut bg) = (
        color(cell.fg, FG),
        (cell.bg != Color::Reset).then(|| color(cell.bg, BG)),
    );
    if cell.modifier.contains(Modifier::REVERSED) {
        (fg, bg) = (bg.unwrap_or_else(|| BG.into()), Some(fg));
    }
    Look {
        fg,
        bg,
        bold: cell.modifier.contains(Modifier::BOLD),
        dim: cell.modifier.contains(Modifier::DIM),
    }
}

/// The arms of a box character: up, down, left and right.
fn arms(c: char) -> Option<[bool; 4]> {
    Some(match c {
        '─' => [false, false, true, true],
        '│' => [true, true, false, false],
        '┌' => [false, true, false, true],
        '┐' => [false, true, true, false],
        '└' => [true, false, false, true],
        '┘' => [true, false, true, false],
        '├' => [true, true, false, true],
        '┤' => [true, true, true, false],
        '┬' => [false, true, true, true],
        '┴' => [true, false, true, true],
        '┼' => [true, true, true, true],
        _ => return None,
    })
}

/// Path commands for a box character in the cell at `left`, `top`. The arms meet but don't
/// overlap, so no join is brighter than the line.
fn box_path([up, down, west, east]: [bool; 4], left: f32, top: f32) -> String {
    let (x, y) = (left + CELL_WIDTH / 2.0, top + LINE_HEIGHT / 2.0);
    let (right, bottom, h) = (left + CELL_WIDTH, top + LINE_HEIGHT, STROKE / 2.0);
    let mut d = String::new();
    let mut line = |x1: f32, y1: f32, x2: f32, y2: f32| {
        let _ = write!(d, "M{} {}L{} {}", n(x1), n(y1), n(x2), n(y2));
    };
    if up && down {
        line(x, top, x, bottom);
        if west {
            line(left, y, x - h, y);
        }
        if east {
            line(x + h, y, right, y);
        }
        return d;
    }
    let across = west || east;
    match (west, east) {
        (true, true) => line(left, y, right, y),
        (true, false) => line(left, y, x + if up || down { h } else { 0.0 }, y),
        (false, true) => line(x - if up || down { h } else { 0.0 }, y, right, y),
        (false, false) => {}
    }
    let gap = if across { h } else { 0.0 };
    if up {
        line(x, top, x, y - gap);
    }
    if down {
        line(x, y + gap, x, bottom);
    }
    d
}

/// A number for an SVG attribute, with at most one decimal.
fn n(value: f32) -> String {
    let text = format!("{value:.1}");
    text.strip_suffix(".0").unwrap_or(&text).to_owned()
}

/// A terminal window with the buffer in it. Each character has its own x, so the columns
/// and the box lines line up whatever monospace font the viewer has.
fn svg(buffer: &Buffer) -> String {
    let area = buffer.area;
    let width = n(f32::from(area.width) * CELL_WIDTH + 2.0 * PAD);
    let height = n(f32::from(area.height) * LINE_HEIGHT + 2.0 * PAD + BAR);
    let mid = n((f32::from(area.width) * CELL_WIDTH + 2.0 * PAD) / 2.0);
    let mut out = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" font-family="ui-monospace,SFMono-Regular,Menlo,Consolas,'DejaVu Sans Mono',monospace" font-size="14" xml:space="preserve">
<rect width="{width}" height="{height}" rx="10" fill="{BG}"/>
<rect width="{width}" height="{BAR}" rx="10" fill="#1b1d2b"/><rect y="20" width="{width}" height="10" fill="#1b1d2b"/>
<circle cx="18" cy="15" r="6" fill="#ff5f57"/><circle cx="38" cy="15" r="6" fill="#febc2e"/><circle cx="58" cy="15" r="6" fill="#28c840"/>
<text x="{mid}" y="20" fill="#828bb8" text-anchor="middle">open_prs</text>
"##
    );
    let left = |x: u16| PAD + f32::from(x) * CELL_WIDTH;
    for y in 0..area.height {
        let top = BAR + PAD + f32::from(y) * LINE_HEIGHT;
        let mut x = 0;
        while x < area.width {
            let first = look(&buffer[(x, y)]);
            let start = x;
            // Each character, and the x where it starts. Box characters are lines, so that
            // they join from row to row.
            let mut chars: Vec<(char, f32)> = Vec::new();
            let mut lines = String::new();
            while x < area.width && look(&buffer[(x, y)]) == first {
                for c in buffer[(x, y)].symbol().chars() {
                    if let Some(arms) = arms(c) {
                        lines.push_str(&box_path(arms, left(x), top));
                        chars.push((' ', left(x)));
                    } else {
                        chars.push((c, left(x)));
                    }
                }
                x += 1;
            }
            if let Some(bg) = &first.bg {
                let _ = writeln!(
                    out,
                    r#"<rect x="{}" y="{}" width="{}" height="{LINE_HEIGHT}" fill="{bg}"/>"#,
                    n(left(start)),
                    n(top),
                    n(f32::from(x - start) * CELL_WIDTH),
                );
            }
            let dim = if first.dim { r#" opacity="0.55""# } else { "" };
            if !lines.is_empty() {
                let _ = writeln!(
                    out,
                    r#"<path d="{}" stroke="{}" stroke-width="{STROKE}" fill="none"{dim}/>"#,
                    lines.trim_end(),
                    first.fg,
                );
            }
            while chars.last().is_some_and(|(c, _)| *c == ' ') {
                chars.pop();
            }
            let blank = chars.iter().take_while(|(c, _)| *c == ' ').count();
            let chars = &chars[blank..];
            if chars.is_empty() {
                continue;
            }
            let xs: Vec<String> = chars.iter().map(|(_, x)| n(*x)).collect();
            let text: String = chars.iter().map(|(c, _)| *c).collect();
            let _ = writeln!(
                out,
                r#"<text x="{}" y="{}" fill="{}"{}{}>{}</text>"#,
                xs.join(" "),
                n(top + 13.5),
                first.fg,
                if first.bold {
                    r#" font-weight="bold""#
                } else {
                    ""
                },
                dim,
                text.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;"),
            );
        }
    }
    out.push_str("</svg>\n");
    out
}
