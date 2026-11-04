use chrono::{DateTime, Utc};
use ratatui::style::Color;

pub const ORG: &str = "acme";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Host {
    GitHub,
    Forgejo,
}

/// Each host is loaded separately, so one that fails keeps its last list on screen.
pub const HOSTS: [Host; 2] = [Host::GitHub, Host::Forgejo];

impl Host {
    pub fn name(self) -> &'static str {
        match self {
            Host::GitHub => "GitHub",
            Host::Forgejo => "Forgejo",
        }
    }
}

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
    pub host: Host,
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    pub status: Status,
    pub updated: DateTime<Utc>,
    pub mine: bool,
}

impl Pr {
    /// The repo name without the org, which every PR shares.
    pub fn short_repo(&self) -> &str {
        self.repo
            .strip_prefix(ORG)
            .and_then(|r| r.strip_prefix('/'))
            .unwrap_or(&self.repo)
    }
}

/// The PRs to show, in the order to show them.
pub fn pick<'a>(
    prs: impl IntoIterator<Item = &'a Pr>,
    mine: bool,
    status: Option<Status>,
    group: Group,
) -> Vec<Pr> {
    let mut prs: Vec<Pr> = prs
        .into_iter()
        .filter(|p| (p.mine || !mine) && status.is_none_or(|s| s == p.status))
        .cloned()
        .collect();
    match group {
        Group::Repo => prs.sort_by(|a, b| {
            (&a.repo, a.host.name(), a.number).cmp(&(&b.repo, b.host.name(), b.number))
        }),
        Group::User => prs.sort_by(|a, b| {
            (&a.author, &a.repo, a.host.name(), a.number).cmp(&(
                &b.author,
                &b.repo,
                b.host.name(),
                b.number,
            ))
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

    pub fn pr(host: Host, repo: &str, number: u64, author: &str, status: Status) -> Pr {
        Pr {
            host,
            repo: format!("{ORG}/{repo}"),
            number,
            title: format!("PR {number}"),
            url: format!("https://{}.example.com/{repo}/{number}", host.name()),
            author: author.into(),
            status,
            updated: DateTime::UNIX_EPOCH,
            mine: author == "me",
        }
    }

    #[test]
    fn pick_sorts_by_repo_then_host_then_number() {
        let prs = [
            pr(Host::GitHub, "b", 1, "x", Status::Needs),
            pr(Host::GitHub, "a", 2, "x", Status::Needs),
            pr(Host::Forgejo, "a", 9, "x", Status::Needs),
            pr(Host::GitHub, "a", 1, "x", Status::Needs),
        ];
        let got: Vec<_> = pick(&prs, false, None, Group::Repo)
            .iter()
            .map(|p| (p.short_repo().to_owned(), p.host, p.number))
            .collect();
        assert_eq!(
            got,
            [
                ("a".into(), Host::Forgejo, 9),
                ("a".into(), Host::GitHub, 1),
                ("a".into(), Host::GitHub, 2),
                ("b".into(), Host::GitHub, 1),
            ]
        );
    }

    #[test]
    fn pick_sorts_by_author_first_when_grouped_by_user() {
        let prs = [
            pr(Host::GitHub, "a", 1, "zed", Status::Needs),
            pr(Host::GitHub, "b", 1, "amy", Status::Needs),
        ];
        let got: Vec<_> = pick(&prs, false, None, Group::User)
            .into_iter()
            .map(|p| p.author)
            .collect();
        assert_eq!(got, ["amy", "zed"]);
    }

    #[test]
    fn pick_filters_mine_and_status() {
        let prs = [
            pr(Host::GitHub, "a", 1, "me", Status::Approved),
            pr(Host::GitHub, "a", 2, "me", Status::Needs),
            pr(Host::GitHub, "a", 3, "you", Status::Approved),
        ];
        let numbers = |mine, status| -> Vec<u64> {
            pick(&prs, mine, status, Group::Repo)
                .iter()
                .map(|p| p.number)
                .collect()
        };
        assert_eq!(numbers(false, None), [1, 2, 3]);
        assert_eq!(numbers(true, None), [1, 2]);
        assert_eq!(numbers(false, Some(Status::Approved)), [1, 3]);
        assert_eq!(numbers(true, Some(Status::Approved)), [1]);
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
