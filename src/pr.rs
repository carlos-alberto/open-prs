use std::collections::{HashMap, HashSet};

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

    /// For narrow tables.
    pub fn short_label(self) -> &'static str {
        match self {
            Status::Changes => "changes",
            _ => self.label(),
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
    /// It changed since you last saw it
    New,
    /// It asks for your review
    ForYou,
    Draft,
    /// It can't merge into its base branch
    Conflict,
    /// It is stacked on this open PR, so that one merges first
    On(u64),
}

impl Note {
    pub fn label(self) -> String {
        match self {
            Note::New => "new".into(),
            Note::ForYou => "for you".into(),
            Note::Draft => "draft".into(),
            Note::Conflict => "conflict".into(),
            Note::On(number) => format!("on #{number}"),
        }
    }

    pub fn style(self) -> Style {
        match self {
            Note::New => Style::new().blue().bold(),
            Note::ForYou => Style::new().magenta().bold(),
            Note::Draft => Style::new().dim(),
            Note::Conflict => Style::new().red(),
            Note::On(_) => Style::new().cyan(),
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
    /// It changed since you last saw it; the app sets it from `seen::Seen`
    pub new: bool,
    pub draft: bool,
    pub conflict: bool,
    /// What the host counts as comments; see the README
    pub comments: u64,
    /// The branch it merges into
    pub base: String,
    /// Its branch, or empty when that is in another repo, as then nothing here can stack on it
    pub head: String,
    /// The open PR it is stacked on; `pick` sets it
    pub parent: Option<u64>,
    /// The lines that join it to its parent in the table, when the parent shows; `pick`
    /// sets it
    pub tree: String,
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
            (self.new, Note::New),
            (self.requested, Note::ForYou),
            (self.draft, Note::Draft),
            (self.conflict, Note::Conflict),
        ]
        .into_iter()
        .filter_map(|(on, note)| on.then_some(note))
        .chain(self.parent.map(Note::On))
        .collect()
    }

    /// The repo's web page: the PR's URL without `/pull/N` or `/pulls/N`. It tells repos
    /// apart across hosts, which `repo` can't.
    pub fn repo_url(&self) -> &str {
        self.url.rsplitn(3, '/').nth(2).unwrap_or(&self.url)
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
    /// Only PRs that changed since you last saw them
    pub new: bool,
    /// Only PRs with failing CI
    pub failing: bool,
    pub hide_drafts: bool,
    /// Words that must all be in the PR's repo, number, title, author or host, in any case
    pub text: String,
}

impl Filter {
    pub fn matches(&self, p: &Pr) -> bool {
        (p.mine || !self.mine)
            && self.status.is_none_or(|s| s == p.status)
            && (p.requested || !self.for_you)
            && (p.new || !self.new)
            && (p.ci == Some(Ci::Failing) || !self.failing)
            && !(p.draft && self.hide_drafts)
            && self.matches_text(p)
    }

    fn matches_text(&self, p: &Pr) -> bool {
        let mut words = self.text.split_whitespace().peekable();
        if words.peek().is_none() {
            return true;
        }
        let haystack = format!(
            "{}#{} {} {} {}",
            p.repo, p.number, p.title, p.author, p.host
        )
        .to_lowercase();
        words.all(|word| haystack.contains(&word.to_lowercase()))
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
        if self.new {
            parts.push("new".into());
        }
        if self.failing {
            parts.push("failing CI".into());
        }
        if self.hide_drafts {
            parts.push("no drafts".into());
        }
        if !self.text.trim().is_empty() {
            parts.push(format!("matching \"{}\"", self.text.trim()));
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
/// first one's copy stays. Grouped by repo, a stacked PR comes after the PR it is stacked
/// on, with lines in `tree` that join them.
pub fn pick<'a>(prs: impl IntoIterator<Item = &'a Pr>, filter: &Filter, group: Group) -> Vec<Pr> {
    let mut seen = HashSet::new();
    let all: Vec<&Pr> = prs
        .into_iter()
        .filter(|p| seen.insert(p.url.as_str()))
        .collect();
    // A PR is stacked on the open PR whose branch is its base. The filter can hide that
    // PR, so this looks at all of them
    let heads: HashMap<(&str, &str), u64> = all
        .iter()
        .filter(|p| !p.head.is_empty())
        .map(|p| ((p.repo_url(), p.head.as_str()), p.number))
        .collect();
    let mut prs: Vec<Pr> = all
        .into_iter()
        .filter(|p| filter.matches(p))
        .map(|p| Pr {
            parent: heads
                .get(&(p.repo_url(), p.base.as_str()))
                .copied()
                .filter(|n| *n != p.number),
            tree: String::new(),
            ..p.clone()
        })
        .collect();
    match group {
        Group::Repo => {
            prs.sort_by(|a, b| (&a.repo, &a.host, a.number).cmp(&(&b.repo, &b.host, b.number)));
            stack(prs)
        }
        Group::User => {
            prs.sort_by(|a, b| {
                (&a.author, &a.repo, &a.host, a.number)
                    .cmp(&(&b.author, &b.repo, &b.host, b.number))
            });
            prs
        }
    }
}

/// Puts each PR after the one it is stacked on, when that one shows in the same group, and
/// draws the tree. The order is otherwise the same.
fn stack(prs: Vec<Pr>) -> Vec<Pr> {
    let index: HashMap<(&str, &str, u64), usize> = prs
        .iter()
        .enumerate()
        .map(|(i, p)| ((p.repo.as_str(), p.host.as_str(), p.number), i))
        .collect();
    let mut children = vec![Vec::new(); prs.len()];
    let mut joined = vec![false; prs.len()];
    for (i, p) in prs.iter().enumerate() {
        if let Some(&parent) = p
            .parent
            .and_then(|n| index.get(&(p.repo.as_str(), p.host.as_str(), n)))
        {
            children[parent].push(i);
            joined[i] = true;
        }
    }

    let mut order: Vec<(usize, String)> = Vec::with_capacity(prs.len());
    let mut done = vec![false; prs.len()];
    // A loop of PRs stacked on each other has no root, so its PRs start trees at the end
    let roots = (0..prs.len()).filter(|&i| !joined[i]).chain(0..prs.len());
    for root in roots {
        if !done[root] {
            done[root] = true;
            order.push((root, String::new()));
            walk(root, "", &children, &mut done, &mut order);
        }
    }

    let mut prs: Vec<Option<Pr>> = prs.into_iter().map(Some).collect();
    order
        .into_iter()
        .map(|(i, tree)| Pr {
            tree,
            ..prs[i].take().unwrap()
        })
        .collect()
}

fn walk(
    i: usize,
    lead: &str,
    children: &[Vec<usize>],
    done: &mut [bool],
    order: &mut Vec<(usize, String)>,
) {
    let next: Vec<usize> = children[i].iter().copied().filter(|&c| !done[c]).collect();
    for (k, &child) in next.iter().enumerate() {
        let last = k + 1 == next.len();
        done[child] = true;
        order.push((child, format!("{lead}{}", if last { "└─ " } else { "├─ " })));
        let lead = format!("{lead}{}", if last { "   " } else { "│  " });
        walk(child, &lead, children, done, order);
    }
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
        let url = format!("https://{host}.example.com/{repo}/pull/{number}");
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
            new: false,
            draft: false,
            conflict: false,
            comments: 0,
            base: "main".into(),
            head: format!("branch-{number}"),
            parent: None,
            tree: String::new(),
        }
    }

    /// The same PR, stacked on `parent`'s branch.
    pub fn on(mut p: Pr, parent: u64) -> Pr {
        p.base = format!("branch-{parent}");
        p
    }

    /// Each PR's number and tree, in order.
    fn trees(prs: &[Pr], filter: &Filter) -> Vec<(u64, String)> {
        pick(prs, filter, Group::Repo)
            .into_iter()
            .map(|p| (p.number, p.tree))
            .collect()
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
    fn pick_filters_review_requests_new_prs_failing_ci_and_drafts() {
        let mut prs = [1, 2, 3, 4].map(|n| pr("GitHub", "a", n, "x", Status::Needs));
        prs[0].requested = true;
        prs[2].new = true;
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
        let new = Filter {
            new: true,
            ..Filter::default()
        };
        assert_eq!(numbers(&prs, &new), [3]);
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
                new: true,
                failing: true,
                hide_drafts: true,
                ..for_you
            }
            .describe(),
            ["mine", "for you", "new", "failing CI", "no drafts"]
        );
    }

    #[test]
    fn text_needs_every_word_somewhere_in_any_case() {
        let mut prs = [1, 2, 3].map(|n| pr("GitHub", "billing", n, "amy", Status::Needs));
        prs[0].title = "Fix the Login page".into();
        prs[1].title = "Add a page".into();
        prs[2].repo = "acme/web".into();
        let text = |text: &str| Filter {
            text: text.into(),
            ..Filter::default()
        };
        assert_eq!(numbers(&prs, &text("")), [1, 2, 3]);
        assert_eq!(numbers(&prs, &text("  ")), [1, 2, 3]);
        assert_eq!(numbers(&prs, &text("PAGE")), [1, 2]);
        assert_eq!(numbers(&prs, &text("page login")), [1]);
        assert_eq!(numbers(&prs, &text("web")), [3]);
        assert_eq!(numbers(&prs, &text("billing#2")), [2]);
        assert_eq!(numbers(&prs, &text("amy github")), [1, 2, 3]);
        assert_eq!(numbers(&prs, &text("page bob")), Vec::<u64>::new());
        assert_eq!(text(" login ").describe(), ["matching \"login\""]);
    }

    #[test]
    fn stacks_show_as_trees_under_the_pr_they_are_on() {
        let pr = |n| pr("GitHub", "a", n, "x", Status::Needs);
        // 1 ← 2 ← 4, 1 ← 3, and 5 on its own; 6 is in another repo, on a branch of the same name
        let mut other = on(pr(6), 1);
        other.repo = "acme/b".into();
        other.url = "https://GitHub.example.com/acme/b/pull/6".into();
        let prs = [
            on(pr(4), 2),
            pr(5),
            on(pr(3), 1),
            on(pr(2), 1),
            pr(1),
            other,
        ];
        let got = trees(&prs, &Filter::default());
        let want = [
            (1, ""),
            (2, "├─ "),
            (4, "│  └─ "),
            (3, "└─ "),
            (5, ""),
            (6, ""),
        ];
        assert_eq!(got, want.map(|(n, t)| (n, t.to_owned())));
        let picked = pick(&prs, &Filter::default(), Group::Repo);
        let parents: Vec<_> = picked.iter().map(|p| (p.number, p.parent)).collect();
        assert_eq!(
            parents,
            [
                (1, None),
                (2, Some(1)),
                (4, Some(2)),
                (3, Some(1)),
                (5, None),
                (6, None)
            ]
        );
    }

    #[test]
    fn a_hidden_parent_leaves_its_stack_as_a_note() {
        let mut prs = [1, 2, 3].map(|n| pr("GitHub", "a", n, "x", Status::Needs));
        prs[1] = on(prs[1].clone(), 1);
        prs[2] = on(prs[2].clone(), 2);
        prs[0].draft = true;
        let no_drafts = Filter {
            hide_drafts: true,
            ..Filter::default()
        };
        assert_eq!(
            trees(&prs, &no_drafts),
            [(2, String::new()), (3, "└─ ".to_owned())]
        );
        let picked = pick(&prs, &no_drafts, Group::Repo);
        assert_eq!(picked[0].notes(), [Note::On(1)]);
        // Grouped by user there are no trees, so every stacked PR has the note
        let by_user = pick(&prs, &Filter::default(), Group::User);
        let notes: Vec<_> = by_user
            .iter()
            .map(|p| (p.tree.clone(), p.notes()))
            .collect();
        assert_eq!(
            notes,
            [
                (String::new(), vec![Note::Draft]),
                (String::new(), vec![Note::On(1)]),
                (String::new(), vec![Note::On(2)]),
            ]
        );
    }

    #[test]
    fn a_loop_of_stacks_still_shows_every_pr() {
        let prs = [
            on(pr("GitHub", "a", 1, "x", Status::Needs), 2),
            on(pr("GitHub", "a", 2, "x", Status::Needs), 1),
            pr("GitHub", "a", 3, "x", Status::Needs),
        ];
        let got = trees(&prs, &Filter::default());
        assert_eq!(
            got,
            [(3, ""), (1, ""), (2, "└─ ")].map(|(n, t)| (n, t.to_owned()))
        );
    }

    #[test]
    fn a_pr_from_another_repo_has_nothing_stacked_on_it() {
        let mut fork = pr("GitHub", "a", 1, "x", Status::Needs);
        fork.head = String::new();
        let prs = [fork, on(pr("GitHub", "a", 2, "x", Status::Needs), 1)];
        let parents: Vec<_> = pick(&prs, &Filter::default(), Group::Repo)
            .into_iter()
            .map(|p| p.parent)
            .collect();
        assert_eq!(parents, [None, None]);
    }

    #[test]
    fn notes_list_what_is_true_in_order() {
        let mut p = pr("GitHub", "a", 1, "x", Status::Needs);
        assert!(p.notes().is_empty());
        p.conflict = true;
        p.requested = true;
        p.parent = Some(7);
        assert_eq!(p.notes(), [Note::ForYou, Note::Conflict, Note::On(7)]);
        p.new = true;
        assert_eq!(p.notes()[0], Note::New);
        assert_eq!(Note::On(7).label(), "on #7");
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
