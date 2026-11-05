//! Desktop notifications for what changed between two loads of a source.

use std::collections::HashMap;
use std::io::{self, Write};
use std::process::Stdio;
use std::thread;

use crate::app::clean;
use crate::desktop;
use crate::pr::{Ci, Pr, Status};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub body: String,
    pub url: String,
}

/// How many notices one load sends before it sends one that counts the rest.
const MOST: usize = 3;

/// What changed from `old` to `new`, the same source's PRs one load apart: a PR that asks for
/// your review, and your PRs that got approved, got changes requested, or started to fail CI.
pub fn changes(old: &[Pr], new: &[Pr]) -> Vec<Notice> {
    let old: HashMap<&str, &Pr> = old.iter().map(|p| (p.url.as_str(), p)).collect();
    let mut notices = Vec::new();
    for p in new {
        let before = old.get(p.url.as_str());
        let became = |now: bool, then: fn(&Pr) -> bool| now && before.is_some_and(|b| !then(b));
        // A PR that opens asking for your review counts too
        let title = if p.requested && before.is_none_or(|b| !b.requested) {
            "Asks for your review"
        } else if p.mine
            && became(p.status == Status::Approved, |b| {
                b.status == Status::Approved
            })
        {
            "Approved"
        } else if p.mine && became(p.status == Status::Changes, |b| b.status == Status::Changes) {
            "Changes requested"
        } else if p.mine && became(p.ci == Some(Ci::Failing), |b| b.ci == Some(Ci::Failing)) {
            "CI failed"
        } else {
            continue;
        };
        notices.push(Notice {
            title: title.into(),
            body: format!("{}#{} {} · {}", p.repo, p.number, clean(&p.title), p.author),
            url: p.url.clone(),
        });
    }
    if notices.len() > MOST {
        let rest = notices.len() - (MOST - 1);
        notices.truncate(MOST - 1);
        notices.push(Notice {
            title: format!("{rest} more PRs changed"),
            body: "See open_prs".into(),
            url: String::new(),
        });
    }
    notices
}

/// Shows `notice` on the desktop, else asks the terminal to (OSC 9). It doesn't wait.
pub fn send(notice: &Notice) {
    let child = if cfg!(target_os = "macos") {
        // The text goes in as arguments, so a PR's title can't change the script
        desktop::command("osascript")
            .args([
                "-e",
                "on run argv",
                "-e",
                "display notification (item 2 of argv) with title (item 1 of argv)",
                "-e",
                "end run",
                &notice.title,
                &notice.body,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    } else {
        desktop::command("notify-send")
            .args(["--app-name=open_prs", &notice.title, &notice.body])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    };
    match child {
        Ok(mut child) => {
            thread::spawn(move || child.wait());
        }
        Err(_) => {
            let text = format!("{}: {}", notice.title, notice.body).replace(['\x07', '\x1b'], "");
            let mut out = io::stdout();
            let _ = write!(out, "\x1b]9;{text}\x07");
            let _ = out.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::tests::pr;

    fn titles(notices: &[Notice]) -> Vec<&str> {
        notices.iter().map(|n| n.title.as_str()).collect()
    }

    #[test]
    fn changes_finds_requests_and_what_happened_to_your_prs() {
        let mut old = [1, 2, 3, 4].map(|n| pr("GitHub", "api", n, "me", Status::Needs));
        old[3].author = "bao".into();
        old[3].mine = false;
        let mut new = old.clone();
        new[0].status = Status::Approved;
        new[1].status = Status::Changes;
        new[2].ci = Some(Ci::Failing);
        new[3].requested = true;
        let notices = changes(&old, &new);
        // Four is more than one load sends, so the last two are one notice
        assert_eq!(
            titles(&notices),
            ["Approved", "Changes requested", "2 more PRs changed"]
        );

        // Under the limit, each has its own
        let notices = changes(&old[..2], &new[..2]);
        assert_eq!(titles(&notices), ["Approved", "Changes requested"]);
        assert_eq!(notices[0].body, "acme/api#1 PR 1 · me");
        assert_eq!(notices[0].url, old[0].url);
    }

    #[test]
    fn changes_ignores_what_was_already_so_and_other_peoples_prs() {
        let mut p = pr("GitHub", "api", 1, "bao", Status::Approved);
        p.ci = Some(Ci::Failing);
        // Not yours, so its approval and CI are not news, and nothing changed
        let mut later = p.clone();
        later.status = Status::Changes;
        assert!(changes(&[p.clone()], &[later]).is_empty());
        assert!(changes(&[p.clone()], &[p.clone()]).is_empty());

        // A PR of yours that shows up approved wasn't approved since the last load
        let mut mine = pr("GitHub", "api", 2, "me", Status::Approved);
        assert!(changes(&[], std::slice::from_ref(&mine)).is_empty());
        // But a new PR that asks for your review is news
        mine.mine = false;
        mine.requested = true;
        assert_eq!(titles(&changes(&[], &[mine])), ["Asks for your review"]);
    }
}
