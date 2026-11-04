use std::collections::HashSet;

use chrono::{DateTime, Utc};
use ratatui::style::{Color, Style};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Approved,
    Changes,
    Needs,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Approved => "approved",
            Status::Changes => "changes requested",
            Status::Needs => "needs review",
        }
    }

    pub fn color(self) -> Color {
        match self {
            Status::Approved => Color::Green,
            Status::Changes => Color::Red,
            Status::Needs => Color::Yellow,
        }
    }
}

/// The combined state of the checks on a PR's last commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ci {
    Passed,
    Failing,
    Pending,
}

impl Ci {
    pub fn label(self) -> &'static str {
        match self {
            Ci::Passed => "passed",
            Ci::Failing => "failing",
            Ci::Pending => "pending",
        }
    }

    pub fn color(self) -> Color {
        match self {
            Ci::Passed => Color::Green,
            Ci::Failing => Color::Red,
            Ci::Pending => Color::Yellow,
        }
    }
}

/// Something to know about a PR, in the Notes column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Note {
    /// It asks for your review
    ForYou,
    Draft,
    /// It can't merge into its base branch
    Conflict,
}

impl Note {
    pub fn label(self) -> &'static str {
        match self {
            Note::ForYou => "for you",
            Note::Draft => "draft",
            Note::Conflict => "conflict",
        }
    }

    pub fn style(self) -> Style {
        match self {
            Note::ForYou => Style::new().magenta().bold(),
            Note::Draft => Style::new().dim(),
            Note::Conflict => Style::new().red(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Group {
    Repo,
    User,
}

impl Group {
    pub fn name(self) -> &'static str {
        match self {
            Group::Repo => "repo",
            Group::User => "user",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Group::Repo => Group::User,
            Group::User => Group::Repo,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Pr {
    /// The name of the source that found it
    pub host: String,
    /// owner/name
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    pub status: Status,
    /// None when the last commit has no checks
    pub ci: Option<Ci>,
    /// The page with the PR's CI runs
    pub checks_url: String,
    pub updated: DateTime<Utc>,
    pub mine: bool,
    /// It asks for your review
    pub requested: bool,
    pub draft: bool,
    pub conflict: bool,
}

impl Pr {
    pub fn owner(&self) -> &str {
        self.repo.split_once('/').map_or("", |(owner, _)| owner)
    }

    /// The repo without `owner/` in front, when the owner is the given one.
    pub fn repo_without<'a>(&'a self, owner: Option<&str>) -> &'a str {
        owner
            .and_then(|o| self.repo.strip_prefix(o))
            .and_then(|r| r.strip_prefix('/'))
            .unwrap_or(&self.repo)
    }

    pub fn notes(&self) -> Vec<Note> {
        [
            (self.requested, Note::ForYou),
            (self.draft, Note::Draft),
            (self.conflict, Note::Conflict),
        ]
        .into_iter()
        .filter_map(|(on, note)| on.then_some(note))
        .collect()
    }
}

/// Which PRs to show. The flags set it, and keys in the TUI toggle each part.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    /// Only PRs you opened
    pub mine: bool,
    pub status: Option<Status>,
    /// Only PRs that ask for your review
    pub for_you: bool,
    /// Only PRs with failing CI
    pub failing: bool,
    pub hide_drafts: bool,
}

impl Filter {
    pub fn matches(&self, p: &Pr) -> bool {
        (p.mine || !self.mine)
            && self.status.is_none_or(|s| s == p.status)
            && (p.requested || !self.for_you)
            && (p.ci == Some(Ci::Failing) || !self.failing)
            && !(p.draft && self.hide_drafts)
    }

    /// What it leaves out, for the subtitle.
    pub fn describe(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if self.mine {
            parts.push("mine".into());
        }
        if let Some(status) = self.status {
            parts.push(format!("{} only", status.label()));
        }
        if self.for_you {
            parts.push("for you".into());
        }
        if self.failing {
            parts.push("failing CI".into());
        }
        if self.hide_drafts {
            parts.push("no drafts".into());
        }
        parts
    }
}

/// The owner of every PR, if they all have the same one, so the table can leave it out.
pub fn shared_owner<'a>(prs: impl IntoIterator<Item = &'a Pr>) -> Option<String> {
    let mut owners = prs.into_iter().map(Pr::owner);
    let first = owners.next()?;
    owners.all(|o| o == first).then(|| first.to_owned())
}

/// The PRs to show, in the order to show them. When two sources find the same PR, the
/// first one's copy stays.
pub fn pick<'a>(prs: impl IntoIterator<Item = &'a Pr>, filter: &Filter, group: Group) -> Vec<Pr> {
    let mut seen = HashSet::new();
    let mut prs: Vec<Pr> = prs
        .into_iter()
        .filter(|p| filter.matches(p))
        .filter(|p| seen.insert(p.url.as_str()))
        .cloned()
        .collect();
    match group {
        Group::Repo => {
            prs.sort_by(|a, b| (&a.repo, &a.host, a.number).cmp(&(&b.repo, &b.host, b.number)))
        }
        Group::User => prs.sort_by(|a, b| {
            (&a.author, &a.repo, &a.host, a.number).cmp(&(&b.author, &b.repo, &b.host, b.number))
        }),
    }
    prs
}

pub fn age(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - t).num_seconds();
    for (unit, size) in [("w", 604800), ("d", 86400), ("h", 3600), ("m", 60)] {
        if seconds >= size {
            return format!("{}{unit}", seconds / size);
        }
    }
    "now".into()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A PR in `repo`, which is `owner/name` or a name in the `acme` org.
    pub fn pr(host: &str, repo: &str, number: u64, author: &str, status: Status) -> Pr {
        let repo = if repo.contains('/') {
            repo.to_owned()
        } else {
            format!("acme/{repo}")
        };
        let url = format!("https://{host}.example.com/{repo}/{number}");
        Pr {
            host: host.into(),
            checks_url: format!("{url}/checks"),
            url,
            repo,
            number,
            title: format!("PR {number}"),
            author: author.into(),
            status,
            ci: None,
            updated: DateTime::UNIX_EPOCH,
            mine: author == "me",
            requested: false,
            draft: false,
            conflict: false,
        }
    }

    fn numbers(prs: &[Pr], filter: &Filter) -> Vec<u64> {
        pick(prs, filter, Group::Repo)
            .iter()
            .map(|p| p.number)
            .collect()
    }

    #[test]
    fn pick_sorts_by_repo_then_host_then_number() {
        let prs = [
            pr("GitHub", "b", 1, "x", Status::Needs),
            pr("GitHub", "a", 2, "x", Status::Needs),
            pr("Forgejo", "a", 9, "x", Status::Needs),
            pr("GitHub", "a", 1, "x", Status::Needs),
        ];
        let got: Vec<_> = pick(&prs, &Filter::default(), Group::Repo)
            .into_iter()
            .map(|p| (p.repo, p.host, p.number))
            .collect();
        let want = [
            ("a", "Forgejo", 9),
            ("a", "GitHub", 1),
            ("a", "GitHub", 2),
            ("b", "GitHub", 1),
        ]
        .map(|(r, h, n)| (format!("acme/{r}"), h.to_owned(), n));
        assert_eq!(got, want);
    }

    #[test]
    fn pick_sorts_by_author_first_when_grouped_by_user() {
        let prs = [
            pr("GitHub", "a", 1, "zed", Status::Needs),
            pr("GitHub", "b", 1, "amy", Status::Needs),
        ];
        let got: Vec<_> = pick(&prs, &Filter::default(), Group::User)
            .into_iter()
            .map(|p| p.author)
            .collect();
        assert_eq!(got, ["amy", "zed"]);
    }

    #[test]
    fn pick_filters_mine_and_status() {
        let prs = [
            pr("GitHub", "a", 1, "me", Status::Approved),
            pr("GitHub", "a", 2, "me", Status::Needs),
            pr("GitHub", "a", 3, "you", Status::Approved),
        ];
        let filter = |mine, status| Filter {
            mine,
            status,
            ..Filter::default()
        };
        assert_eq!(numbers(&prs, &filter(false, None)), [1, 2, 3]);
        assert_eq!(numbers(&prs, &filter(true, None)), [1, 2]);
        assert_eq!(
            numbers(&prs, &filter(false, Some(Status::Approved))),
            [1, 3]
        );
        assert_eq!(numbers(&prs, &filter(true, Some(Status::Approved))), [1]);
    }

    #[test]
    fn pick_filters_review_requests_failing_ci_and_drafts() {
        let mut prs = [1, 2, 3, 4].map(|n| pr("GitHub", "a", n, "x", Status::Needs));
        prs[0].requested = true;
        prs[1].ci = Some(Ci::Failing);
        prs[2].ci = Some(Ci::Pending);
        prs[3].draft = true;
        prs[3].ci = Some(Ci::Failing);
        let for_you = Filter {
            for_you: true,
            ..Filter::default()
        };
        let failing = Filter {
            failing: true,
            ..Filter::default()
        };
        let no_drafts = Filter {
            hide_drafts: true,
            ..Filter::default()
        };
        assert_eq!(numbers(&prs, &for_you), [1]);
        assert_eq!(numbers(&prs, &failing), [2, 4]);
        assert_eq!(numbers(&prs, &no_drafts), [1, 2, 3]);
        assert_eq!(
            numbers(
                &prs,
                &Filter {
                    hide_drafts: true,
                    ..failing.clone()
                }
            ),
            [2]
        );
        assert_eq!(
            Filter {
                mine: true,
                failing: true,
                hide_drafts: true,
                ..for_you
            }
            .describe(),
            ["mine", "for you", "failing CI", "no drafts"]
        );
    }

    #[test]
    fn notes_list_what_is_true_in_order() {
        let mut p = pr("GitHub", "a", 1, "x", Status::Needs);
        assert!(p.notes().is_empty());
        p.conflict = true;
        p.requested = true;
        assert_eq!(p.notes(), [Note::ForYou, Note::Conflict]);
    }

    #[test]
    fn pick_keeps_one_copy_of_a_pr_that_two_sources_find() {
        let mut copy = pr("GitHub", "a", 1, "x", Status::Needs);
        copy.host = "Work GitHub".into();
        let prs = [pr("GitHub", "a", 1, "x", Status::Needs), copy];
        let hosts: Vec<_> = pick(&prs, &Filter::default(), Group::Repo)
            .into_iter()
            .map(|p| p.host)
            .collect();
        assert_eq!(hosts, ["GitHub"]);
    }

    #[test]
    fn the_owner_is_left_out_only_when_every_pr_shares_it() {
        let a = pr("GitHub", "acme/a", 1, "x", Status::Needs);
        let b = pr("GitHub", "acme/b", 1, "x", Status::Needs);
        let c = pr("GitHub", "other/c", 1, "x", Status::Needs);
        assert_eq!(shared_owner([&a, &b]).as_deref(), Some("acme"));
        assert_eq!(shared_owner([&a, &c]), None);
        assert_eq!(shared_owner([]), None);
        assert_eq!(a.repo_without(Some("acme")), "a");
        assert_eq!(c.repo_without(Some("acme")), "other/c");
        assert_eq!(a.repo_without(None), "acme/a");
        // Only a whole owner comes off
        assert_eq!(
            pr("GitHub", "acme2/x", 1, "x", Status::Needs).repo_without(Some("acme")),
            "acme2/x"
        );
    }

    #[test]
    fn age_uses_the_largest_whole_unit() {
        let now = DateTime::UNIX_EPOCH + chrono::Duration::weeks(10);
        let ago = |s| age(now - chrono::Duration::seconds(s), now);
        assert_eq!(ago(0), "now");
        assert_eq!(ago(59), "now");
        assert_eq!(ago(60), "1m");
        assert_eq!(ago(3599), "59m");
        assert_eq!(ago(7200), "2h");
        assert_eq!(ago(86400 * 3), "3d");
        assert_eq!(ago(604800 * 2 + 5), "2w");
        assert_eq!(ago(-30), "now");
    }
}
