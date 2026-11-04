//! The panel that asks people to review a PR, or stops asking them.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Padding, Paragraph};

use crate::app::{clean, wrap};
use crate::manage;
use crate::pr::Pr;

/// Who reviews a PR, and who can.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reviewers {
    /// Who the PR asks for a review now: logins, and org/team for a GitHub team
    pub requested: Vec<String>,
    /// Who can review it, the host's suggestions first, without the author
    pub people: Vec<String>,
    /// What each person said in their last review
    pub verdicts: HashMap<String, &'static str>,
}

/// The reviews to ask for, and the ones to stop asking for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

impl Change {
    pub fn is_empty(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty()
    }

    /// What it does, as in "asks amy and bao, and stops asking chidi", or with `done`, what
    /// it did: "asked amy and bao, and stopped asking chidi".
    pub fn describe(&self, done: bool) -> String {
        let (ask, stop) = if done {
            ("asked", "stopped asking")
        } else {
            ("asks", "stops asking")
        };
        let mut parts = Vec::new();
        if !self.add.is_empty() {
            parts.push(format!("{ask} {}", names(&self.add)));
        }
        if !self.remove.is_empty() {
            parts.push(format!("{stop} {}", names(&self.remove)));
        }
        parts.join(", and ")
    }
}

/// "amy", "amy and bao", "amy, bao and chidi".
fn names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Whether a person or team has the words in their name. A team, org/slug, matches on its
/// slug, as each team has the org's name in it, or on all of it from the start.
fn matches(name: &str, text: &str) -> bool {
    let name = name.to_lowercase();
    match name.split_once('/') {
        Some((_, slug)) => slug.contains(text) || name.starts_with(text),
        None => name.contains(text),
    }
}

pub enum Action {
    None,
    Close,
    Save(Change),
}

pub struct Picker {
    /// The PR as it was in the table
    pub pr: Pr,
    /// None while it loads
    pub content: Option<Result<Reviewers, String>>,
    /// Who it asks when you save; it starts as who the PR asks now
    chosen: Vec<String>,
    /// The words that filter the people
    text: String,
    cursor: usize,
    /// It is saving the change
    pub saving: bool,
    /// Why the last save failed
    pub error: Option<String>,
}

/// One row of the list.
#[derive(Debug, PartialEq, Eq)]
enum Row {
    Person(String),
    /// A name that isn't in the list, as typed
    Typed(String),
}

impl Picker {
    pub fn new(pr: Pr) -> Self {
        Picker {
            pr,
            content: None,
            chosen: Vec::new(),
            text: String::new(),
            cursor: 0,
            saving: false,
            error: None,
        }
    }

    pub fn loaded(&mut self, result: Result<Reviewers, String>) {
        if let Ok(reviewers) = &result {
            self.chosen = reviewers.requested.clone();
        }
        self.content = Some(result);
    }

    fn reviewers(&self) -> Option<&Reviewers> {
        self.content.as_ref()?.as_ref().ok()
    }

    /// Who it asks now, the people, and the names ticked as typed, that match the words; then
    /// what was typed when no one has that name. Ticking a name doesn't move it.
    fn rows(&self) -> Vec<Row> {
        let Some(reviewers) = self.reviewers() else {
            return Vec::new();
        };
        let text = self.text.trim().to_lowercase();
        let mut names: Vec<&String> = reviewers.requested.iter().collect();
        for name in reviewers.people.iter().chain(&self.chosen) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        let mut rows: Vec<Row> = names
            .into_iter()
            .filter(|n| matches(n, &text))
            .map(|n| Row::Person(n.clone()))
            .collect();
        let typed = self.text.trim();
        let known = rows
            .iter()
            .any(|r| matches!(r, Row::Person(n) if n.eq_ignore_ascii_case(typed)));
        if !typed.is_empty() && !known {
            rows.push(Row::Typed(typed.to_owned()));
        }
        rows
    }

    /// The reviews to ask for and stop asking for, in the order they show.
    pub fn change(&self) -> Change {
        let Some(reviewers) = self.reviewers() else {
            return Change::default();
        };
        Change {
            add: self
                .chosen
                .iter()
                .filter(|n| !reviewers.requested.contains(n))
                .cloned()
                .collect(),
            remove: reviewers
                .requested
                .iter()
                .filter(|n| !self.chosen.contains(n))
                .cloned()
                .collect(),
        }
    }

    fn toggle(&mut self) {
        let name = match self.rows().into_iter().nth(self.cursor) {
            Some(Row::Person(name) | Row::Typed(name)) => name,
            None => return,
        };
        if let Some(i) = self.chosen.iter().position(|n| *n == name) {
            self.chosen.remove(i);
        } else {
            self.chosen.push(name);
        }
    }

    /// Typing filters the people, space ticks the one under the cursor, and enter saves.
    /// With nothing ticked yet, enter ticks the one under the cursor first, so a name and
    /// enter is enough.
    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if self.saving {
            return Action::None;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc if !self.text.is_empty() => self.set_text(String::new()),
            KeyCode::Esc => return Action::Close,
            KeyCode::Enter => {
                if self.reviewers().is_none() {
                    return Action::None;
                }
                if self.change().is_empty() {
                    self.toggle();
                }
                let change = self.change();
                if !change.is_empty() {
                    self.error = None;
                    return Action::Save(change);
                }
            }
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Down => {
                self.cursor = (self.cursor + 1).min(self.rows().len().saturating_sub(1));
            }
            KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Backspace => {
                let mut text = self.text.clone();
                text.pop();
                self.set_text(text);
            }
            KeyCode::Char('u') if control => self.set_text(String::new()),
            KeyCode::Char(c) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                let mut text = self.text.clone();
                text.push(c);
                self.set_text(text);
            }
            _ => {}
        }
        Action::None
    }

    fn set_text(&mut self, text: String) {
        self.text = text;
        self.cursor = 0;
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let width = area.width.min(64);
        let inner = width.saturating_sub(4).max(1);
        let mut lines = vec![
            Line::from(vec![
                Span::styled(" > ", Style::new().bold().reversed()),
                Span::raw(format!(" {}", self.text)),
                Span::raw("█"),
            ]),
            Line::default(),
        ];

        // The rows that fit, with the cursor in them
        // The borders, the words, two blank lines, a line for what enter does and one for
        // the keys, and one more for when that says more
        let room = usize::from(area.height.saturating_sub(9)).max(3);
        match &self.content {
            None => lines.push(Line::raw("Loading…").dim()),
            Some(Err(error)) => {
                lines.extend(
                    wrap(&clean(error), inner)
                        .into_iter()
                        .map(|l| Line::raw(l).red()),
                );
            }
            Some(Ok(reviewers)) => {
                let rows = self.rows();
                if rows.is_empty() {
                    lines.push(Line::raw("No one; type a name").dim());
                }
                // With too many, the last line says how many more there are
                let shown = if rows.len() > room { room - 1 } else { room };
                let first = self.cursor.saturating_sub(shown - 1);
                for (i, row) in rows.iter().enumerate().skip(first).take(shown) {
                    lines.push(self.row(row, i == self.cursor, reviewers, inner));
                }
                let more = rows.len().saturating_sub(first + shown);
                if rows.len() > room {
                    let hidden = if more > 0 {
                        format!("  ↓ {more} more; type to find them")
                    } else {
                        format!("  ↑ {first} more above")
                    };
                    lines.push(Line::raw(hidden).dim());
                }
            }
        }

        lines.push(Line::default());
        let change = self.change();
        if self.saving {
            lines.push(Line::raw("Saving…").dim());
        } else if let Some(error) = &self.error {
            lines.extend(
                wrap(&clean(error), inner)
                    .into_iter()
                    .map(|l| Line::raw(l).red()),
            );
        } else if change.is_empty() {
            lines.push(Line::raw("space ticks a name, and enter asks them").dim());
        } else {
            let what = format!("enter {}", change.describe(false));
            lines.extend(wrap(&what, inner).into_iter().map(Line::raw));
        }
        lines.push(manage::keys(&[
            ("space", "Tick"),
            ("enter", "Save"),
            ("esc", "Close"),
        ]));

        let rect = manage::popup(area, width, lines.len() as u16 + 2);
        let title = format!(" Reviewers of {} #{} ", self.pr.repo, self.pr.number);
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::bordered()
                    .title(Span::styled(title, Style::new().bold()))
                    .padding(Padding::horizontal(1)),
            ),
            rect,
        );
    }

    fn row(&self, row: &Row, cursor: bool, reviewers: &Reviewers, width: u16) -> Line<'static> {
        let (name, typed) = match row {
            Row::Person(name) => (name, false),
            Row::Typed(name) => (name, true),
        };
        let tick = if self.chosen.contains(name) {
            "✓"
        } else {
            " "
        };
        let mut spans = vec![
            Span::styled(format!("{tick} "), Style::new().green().bold()),
            Span::raw(clean(name)),
        ];
        if name.contains('/') {
            spans.push(Span::styled("  team", Style::new().dim()));
        }
        if typed {
            spans.push(Span::styled(" (as typed)", Style::new().dim()));
        }
        if let Some(verdict) = reviewers.verdicts.get(name) {
            let color = match *verdict {
                "approved" => Color::Green,
                "changes requested" => Color::Red,
                _ => Color::Reset,
            };
            spans.push(Span::styled(format!("  {verdict}"), Style::new().fg(color)));
        }
        if reviewers.requested.contains(name) {
            spans.push(Span::styled("  asked", Style::new().dim()));
        }
        let mut line = Line::from(spans);
        if cursor {
            // The highlight goes across the panel
            let rest = usize::from(width).saturating_sub(line.width());
            line.push_span(Span::raw(" ".repeat(rest)));
            line = line.style(Style::new().bg(Color::DarkGray).bold());
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::Status;
    use crate::pr::tests::pr;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn picker() -> Picker {
        let mut picker = Picker::new(pr("GitHub", "api", 418, "amy", Status::Needs));
        picker.loaded(Ok(Reviewers {
            requested: vec!["chidi".into()],
            people: vec!["bao".into(), "chidi".into(), "dana".into()],
            verdicts: HashMap::from([("bao".into(), "approved")]),
        }));
        picker
    }

    fn type_text(picker: &mut Picker, text: &str) {
        for c in text.chars() {
            picker.on_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn rows_put_who_it_asks_first_and_filter_by_the_words() {
        let mut picker = picker();
        assert_eq!(
            picker.rows(),
            ["chidi", "bao", "dana"].map(|n| Row::Person(n.into()))
        );
        type_text(&mut picker, "DA");
        assert_eq!(
            picker.rows(),
            [Row::Person("dana".into()), Row::Typed("DA".into())]
        );
        type_text(&mut picker, "na");
        assert_eq!(picker.rows(), [Row::Person("dana".into())]);
    }

    #[test]
    fn teams_match_on_their_slug() {
        let mut picker = Picker::new(pr("GitHub", "api", 418, "amy", Status::Needs));
        picker.loaded(Ok(Reviewers {
            people: ["bao", "big-engine/engineering", "big-engine/product"]
                .map(String::from)
                .into(),
            ..Reviewers::default()
        }));
        type_text(&mut picker, "eng");
        assert_eq!(
            picker.rows()[0],
            Row::Person("big-engine/engineering".into())
        );
        assert_eq!(picker.rows().len(), 2, "and eng as typed");
        picker.set_text("big-engine/p".into());
        assert_eq!(picker.rows()[0], Row::Person("big-engine/product".into()));
        let Action::Save(change) = picker.on_key(key(KeyCode::Enter)) else {
            panic!("expected a save");
        };
        assert_eq!(change.add, ["big-engine/product"]);
    }

    #[test]
    fn a_name_and_enter_asks_them() {
        let mut picker = picker();
        type_text(&mut picker, "dan");
        let Action::Save(change) = picker.on_key(key(KeyCode::Enter)) else {
            panic!("expected a save");
        };
        assert_eq!(change.add, ["dana"]);
        assert!(change.remove.is_empty());
    }

    #[test]
    fn space_ticks_more_than_one_and_unticks() {
        let mut picker = picker();
        // chidi is asked, so space stops asking them
        picker.on_key(key(KeyCode::Char(' ')));
        picker.on_key(key(KeyCode::Down));
        picker.on_key(key(KeyCode::Char(' ')));
        type_text(&mut picker, "org/team");
        picker.on_key(key(KeyCode::Char(' ')));
        let change = picker.change();
        assert_eq!(change.add, ["bao", "org/team"]);
        assert_eq!(change.remove, ["chidi"]);
        assert_eq!(
            change.describe(false),
            "asks bao and org/team, and stops asking chidi"
        );
        assert_eq!(
            change.describe(true),
            "asked bao and org/team, and stopped asking chidi"
        );
        assert!(matches!(
            picker.on_key(key(KeyCode::Enter)),
            Action::Save(_)
        ));
    }

    #[test]
    fn esc_clears_the_words_then_closes() {
        let mut picker = picker();
        type_text(&mut picker, "x");
        assert!(matches!(picker.on_key(key(KeyCode::Esc)), Action::None));
        assert_eq!(picker.rows().len(), 3);
        assert!(matches!(picker.on_key(key(KeyCode::Esc)), Action::Close));
    }

    #[test]
    fn enter_does_nothing_while_it_loads_or_without_a_change() {
        let mut picker = Picker::new(pr("GitHub", "api", 418, "amy", Status::Needs));
        assert!(matches!(picker.on_key(key(KeyCode::Enter)), Action::None));
        picker.loaded(Ok(Reviewers::default()));
        assert!(matches!(picker.on_key(key(KeyCode::Enter)), Action::None));
    }

    #[test]
    fn the_list_says_how_many_more_names_there_are() {
        let mut picker = Picker::new(pr("GitHub", "api", 418, "amy", Status::Needs));
        picker.loaded(Ok(Reviewers {
            people: (0..20).map(|i| format!("user{i}")).collect(),
            ..Reviewers::default()
        }));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(64, 16)).unwrap();
        terminal.draw(|f| picker.draw(f, f.area())).unwrap();
        let buffer = terminal.backend().buffer();
        let text: Vec<String> = (0..16)
            .map(|y| (0..64).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let text = text.join("\n");
        // 16 lines leave 7 for names: 6, and the line that says so
        assert!(text.contains("user5 "), "{text}");
        assert!(!text.contains("user6 "), "{text}");
        assert!(text.contains("↓ 14 more; type to find them"), "{text}");

        picker.cursor = 19;
        terminal.draw(|f| picker.draw(f, f.area())).unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = (0..16)
            .flat_map(|y| (0..64).map(move |x| (x, y)))
            .map(|xy| buffer[xy].symbol().to_owned())
            .collect();
        assert!(text.contains("user19 "), "{text}");
        assert!(text.contains("↑ 14 more above"), "{text}");
    }

    #[test]
    fn names_join_with_and() {
        let n = |v: &[&str]| names(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(n(&["amy"]), "amy");
        assert_eq!(n(&["amy", "bao"]), "amy and bao");
        assert_eq!(n(&["amy", "bao", "chidi"]), "amy, bao and chidi");
    }
}
