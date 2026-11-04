//! The preview of one PR: its description, checks, files and last comments.

use chrono::{DateTime, Utc};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};

use crate::app::{clean, wrap};
use crate::pr::{Ci, Pr, age};

/// What the host says about a PR beyond its row in the table.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Preview {
    pub body: String,
    /// None when the host doesn't say, as Forgejo through tea doesn't
    pub files: Option<Files>,
    /// Each check on the last commit; empty when the host doesn't say
    pub checks: Vec<Check>,
    /// The last comments and reviews, oldest first
    pub comments: Vec<Comment>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Files {
    pub additions: u64,
    pub deletions: u64,
    /// How many files changed, which can be more than `changed` lists
    pub count: u64,
    pub changed: Vec<FileChange>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileChange {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub name: String,
    pub state: Ci,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    pub author: String,
    pub when: DateTime<Utc>,
    pub body: String,
    /// For a review: approved, changes requested or reviewed
    pub verdict: Option<&'static str>,
}

/// How many comments and reviews the preview shows, the last ones.
pub const COMMENTS: usize = 10;

/// How many lines of each comment it shows.
const COMMENT_LINES: usize = 8;

/// The newest `COMMENTS`, oldest first.
pub fn last_comments(mut comments: Vec<Comment>) -> Vec<Comment> {
    comments.sort_by_key(|c| c.when);
    let skip = comments.len().saturating_sub(COMMENTS);
    comments.split_off(skip)
}

/// The lines of the preview, `width` wide. `content` is None while it loads.
pub fn lines(
    p: &Pr,
    content: Option<&Result<Preview, String>>,
    width: u16,
    now: DateTime<Utc>,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = wrap(&clean(&p.title), width)
        .into_iter()
        .map(|l| Line::raw(l).bold())
        .collect();
    out.push(about(p, now));
    out.push(Line::raw(p.url.clone()).dim());

    let preview = match content {
        None => {
            out.push(Line::default());
            out.push(Line::raw("Loading…").dim());
            return out;
        }
        Some(Err(error)) => {
            out.push(Line::default());
            for line in wrap(&clean(error), width) {
                out.push(Line::raw(line).red());
            }
            return out;
        }
        Some(Ok(preview)) => preview,
    };

    heading(&mut out, "Description", String::new());
    let description = body(&preview.body);
    if description.is_empty() {
        out.push(Line::raw("No description.").dim());
    } else {
        out.extend(wrap(&description, width).into_iter().map(Line::raw));
    }

    checks(&mut out, p, &preview.checks);

    if let Some(files) = &preview.files {
        let summary = format!(
            " · +{} −{} in {} {}",
            files.additions,
            files.deletions,
            files.count,
            if files.count == 1 { "file" } else { "files" }
        );
        heading(&mut out, "Files", summary);
        for f in &files.changed {
            out.push(Line::from(vec![
                Span::raw(format!("  {}  ", clean(&f.path))),
                Span::styled(format!("+{}", f.additions), Style::new().green()),
                Span::raw(" "),
                Span::styled(format!("−{}", f.deletions), Style::new().red()),
            ]));
        }
        let more = files.count.saturating_sub(files.changed.len() as u64);
        if more > 0 {
            out.push(Line::raw(format!("  and {more} more")).dim());
        }
    }

    let summary = if preview.comments.len() == COMMENTS {
        format!(" · the last {COMMENTS}")
    } else {
        String::new()
    };
    heading(&mut out, "Comments", summary);
    if preview.comments.is_empty() {
        out.push(Line::raw("No comments.").dim());
    }
    for (i, c) in preview.comments.iter().enumerate() {
        if i > 0 {
            out.push(Line::default());
        }
        let mut spans = vec![Span::styled(clean(&c.author), Style::new().bold())];
        if let Some(verdict) = c.verdict {
            let color = match verdict {
                "approved" => Color::Green,
                "changes requested" => Color::Red,
                _ => Color::Reset,
            };
            spans.push(Span::raw(" · "));
            spans.push(Span::styled(verdict, Style::new().fg(color)));
        }
        spans.push(Span::styled(
            format!(" · {}", age(c.when, now)),
            Style::new().dim(),
        ));
        out.push(Line::from(spans));
        // An approval often has no text
        let text = body(&c.body);
        if text.is_empty() {
            continue;
        }
        let lines = wrap(&text, width.saturating_sub(2).max(1));
        let cut = lines.len() > COMMENT_LINES;
        for line in lines.into_iter().take(COMMENT_LINES) {
            out.push(Line::raw(format!("  {line}")));
        }
        if cut {
            out.push(Line::raw("  …").dim());
        }
    }
    out
}

/// Who opened it, its status, CI and notes, and when it changed.
fn about(p: &Pr, now: DateTime<Utc>) -> Line<'static> {
    let gap = || Span::raw(" · ");
    let mut spans = vec![
        Span::raw(clean(&p.author)),
        gap(),
        Span::styled(p.status.label(), Style::new().fg(p.status.color())),
    ];
    if let Some(ci) = p.ci {
        spans.push(gap());
        spans.push(Span::styled(
            format!("CI {}", ci.label()),
            Style::new().fg(ci.color()),
        ));
    }
    for note in p.notes() {
        spans.push(gap());
        spans.push(Span::styled(note.label(), note.style()));
    }
    if !p.base.is_empty() {
        spans.push(gap());
        spans.push(Span::raw(format!("into {}", p.base)));
    }
    spans.push(gap());
    spans.push(Span::raw(format!("updated {}", age(p.updated, now))));
    Line::from(spans)
}

fn heading(out: &mut Vec<Line<'static>>, name: &'static str, summary: String) {
    out.push(Line::default());
    out.push(Line::from(vec![
        Span::styled(name, Style::new().bold().underlined()),
        Span::styled(summary, Style::new().dim()),
    ]));
}

/// The checks that didn't pass by name, and how many did. Without the checks, as from
/// Forgejo, the state of them all.
fn checks(out: &mut Vec<Line<'static>>, p: &Pr, checks: &[Check]) {
    if checks.is_empty() {
        if let Some(ci) = p.ci {
            heading(out, "Checks", String::new());
            out.push(Line::from(vec![
                Span::styled(ci.label(), Style::new().fg(ci.color())),
                Span::raw("; c opens the runs"),
            ]));
        }
        return;
    }
    let count = |state| checks.iter().filter(|c| c.state == state).count();
    let summary: Vec<String> = [Ci::Failing, Ci::Pending, Ci::Passed]
        .into_iter()
        .filter(|s| count(*s) > 0)
        .map(|s| format!("{} {}", count(s), s.label()))
        .collect();
    heading(out, "Checks", format!(" · {}", summary.join(", ")));
    for state in [Ci::Failing, Ci::Pending] {
        let mark = if state == Ci::Failing { "✗" } else { "●" };
        for c in checks.iter().filter(|c| c.state == state) {
            out.push(Line::styled(
                format!("  {mark} {}", clean(&c.name)),
                Style::new().fg(state.color()),
            ));
        }
    }
}

/// The text of a description or comment, without the HTML comments that PR templates
/// leave in, and without more than one blank line in a row.
fn body(text: &str) -> String {
    let mut rest = text.replace("\r\n", "\n");
    let mut kept = String::with_capacity(rest.len());
    while let Some(start) = rest.find("<!--") {
        kept.push_str(&rest[..start]);
        rest = match rest[start..].find("-->") {
            Some(end) => rest[start + end + 3..].to_owned(),
            None => String::new(),
        };
    }
    kept.push_str(&rest);

    let mut out = String::with_capacity(kept.len());
    let mut blank = 0;
    for line in kept.trim().lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::Status;
    use crate::pr::tests::pr;
    use chrono::TimeDelta;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn comment(author: &str, minutes: i64, body: &str, verdict: Option<&'static str>) -> Comment {
        Comment {
            author: author.into(),
            when: DateTime::UNIX_EPOCH + TimeDelta::minutes(minutes),
            body: body.into(),
            verdict,
        }
    }

    #[test]
    fn body_drops_html_comments_and_extra_blank_lines() {
        let text = "<!-- Describe it -->\r\nAdds limits.\r\n\r\n\r\n\r\n- per key\n<!-- unclosed";
        assert_eq!(body(text), "Adds limits.\n\n- per key\n");
    }

    #[test]
    fn last_comments_keeps_the_newest_oldest_first() {
        let comments: Vec<Comment> = (0..15)
            .rev()
            .map(|i| comment("amy", i, &i.to_string(), None))
            .collect();
        let last = last_comments(comments);
        assert_eq!(last.len(), COMMENTS);
        assert_eq!(last[0].body, "5");
        assert_eq!(last[COMMENTS - 1].body, "14");
    }

    #[test]
    fn lines_show_each_part() {
        let mut p = pr("GitHub", "api", 418, "amy", Status::Needs);
        p.title = "Return Retry-After".into();
        p.ci = Some(Ci::Failing);
        let preview = Preview {
            body: "Adds the header.".into(),
            files: Some(Files {
                additions: 12,
                deletions: 3,
                count: 3,
                changed: vec![FileChange {
                    path: "src/limits.rs".into(),
                    additions: 10,
                    deletions: 3,
                }],
            }),
            checks: vec![
                Check {
                    name: "test".into(),
                    state: Ci::Failing,
                },
                Check {
                    name: "lint".into(),
                    state: Ci::Passed,
                },
            ],
            comments: vec![
                comment("bao", 0, "Looks good", Some("approved")),
                comment("chidi", 5, "", Some("changes requested")),
            ],
        };
        let now = DateTime::UNIX_EPOCH + TimeDelta::hours(2);
        let lines = text(&lines(&p, Some(&Ok(preview)), 60, now));
        assert_eq!(
            lines,
            [
                "Return Retry-After",
                "amy · needs review · CI failing · into main · updated 2h",
                "https://GitHub.example.com/acme/api/pull/418",
                "",
                "Description",
                "Adds the header.",
                "",
                "Checks · 1 failing, 1 passed",
                "  ✗ test",
                "",
                "Files · +12 −3 in 3 files",
                "  src/limits.rs  +10 −3",
                "  and 2 more",
                "",
                "Comments",
                "bao · approved · 2h",
                "  Looks good",
                "",
                "chidi · changes requested · 1h",
            ]
        );
    }

    #[test]
    fn lines_say_when_it_loads_or_fails() {
        let p = pr("Forgejo", "docs", 23, "dana", Status::Needs);
        let now = DateTime::UNIX_EPOCH;
        assert_eq!(text(&lines(&p, None, 60, now)).last().unwrap(), "Loading…");
        let failed = Err("HTTP 404".to_owned());
        assert_eq!(
            text(&lines(&p, Some(&failed), 60, now)).last().unwrap(),
            "HTTP 404"
        );
        let empty = Ok(Preview::default());
        let lines = text(&lines(&p, Some(&empty), 60, now));
        assert!(lines.contains(&"No description.".to_owned()));
        assert!(lines.contains(&"No comments.".to_owned()));
        assert!(!lines.iter().any(|l| l.starts_with("Files")));
    }
}
