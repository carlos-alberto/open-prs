use std::collections::{BTreeSet, HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::config::{Kind, Source};
use crate::pr::{Ci, Pr, Status};

pub fn fetch(source: &Source) -> Result<Vec<Pr>> {
    match &source.kind {
        Kind::GitHub { query, hostname } => github(source.name(), query, hostname.as_deref()),
        Kind::Forgejo { login, owner } => forgejo(source.name(), login, owner.as_deref()),
    }
}

/// The names of the logins tea has, to help pick one.
pub fn tea_logins() -> Result<Vec<String>> {
    Ok(logins()?.into_iter().map(|l| l.name).collect())
}

fn logins() -> Result<Vec<TeaLogin>> {
    serde_json::from_str(&run(&["tea", "logins", "list", "-o", "json"])?)
        .context("could not read the logins from tea")
}

fn run(cmd: &[&str]) -> Result<String> {
    // stdin is null, so gh or tea can't read the keys meant for the TUI
    let out = Command::new(cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("could not run {}", cmd[0]))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let message = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        match message.trim() {
            "" => bail!("{} exited {}", cmd[0], out.status),
            message => bail!("{message}"),
        }
    }
    String::from_utf8(out.stdout).with_context(|| format!("{} printed invalid UTF-8", cmd[0]))
}

const GITHUB_QUERY: &str = "
query($q: String!, $endCursor: String) {
  viewer { login }
  search(query: $q, type: ISSUE, first: 50, after: $endCursor) {
    pageInfo { hasNextPage endCursor }
    nodes { ... on PullRequest {
      number title url reviewDecision updatedAt isDraft mergeable baseRefName headRefName
      author { login }
      repository { nameWithOwner }
      headRepository { nameWithOwner }
      latestOpinionatedReviews(first: 20) { nodes { state } }
      comments { totalCount }
      reviewThreads(first: 100) { nodes { comments { totalCount } } }
      commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
    } }
  }
}";

/// Only the URLs, for the search for PRs that ask for your review.
const GITHUB_URLS_QUERY: &str = "
query($q: String!, $endCursor: String) {
  search(query: $q, type: ISSUE, first: 100, after: $endCursor) {
    pageInfo { hasNextPage endCursor }
    nodes { ... on PullRequest { url } }
  }
}";

#[derive(Deserialize)]
struct GhPage {
    data: GhData,
}

#[derive(Deserialize)]
struct GhUrlPage {
    data: GhUrlData,
}

#[derive(Deserialize)]
struct GhUrlData {
    search: GhSearch,
}

#[derive(Deserialize)]
struct GhData {
    viewer: GhLogin,
    search: GhSearch,
}

#[derive(Deserialize)]
struct GhLogin {
    login: String,
}

#[derive(Deserialize)]
struct GhSearch {
    nodes: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPr {
    number: u64,
    title: String,
    url: String,
    review_decision: Option<String>,
    updated_at: DateTime<Utc>,
    is_draft: bool,
    /// MERGEABLE, CONFLICTING, or UNKNOWN while GitHub works it out
    mergeable: String,
    base_ref_name: String,
    head_ref_name: String,
    author: Option<GhLogin>,
    repository: GhRepo,
    /// null when the fork it came from is gone
    head_repository: Option<GhRepo>,
    latest_opinionated_reviews: GhReviews,
    commits: GhCommits,
    /// In the conversation
    comments: GhCount,
    /// Each holds inline comments on the code
    review_threads: GhThreads,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhCount {
    total_count: u64,
}

#[derive(Deserialize)]
struct GhThreads {
    nodes: Vec<GhThread>,
}

#[derive(Deserialize)]
struct GhThread {
    comments: GhCount,
}

#[derive(Deserialize)]
struct GhCommits {
    nodes: Vec<GhCommitNode>,
}

#[derive(Deserialize)]
struct GhCommitNode {
    commit: GhCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhCommit {
    /// null when the commit has no checks or statuses
    status_check_rollup: Option<GhState>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(PartialEq)]
struct GhRepo {
    name_with_owner: String,
}

#[derive(Deserialize)]
struct GhReviews {
    nodes: Vec<GhState>,
}

#[derive(Deserialize)]
struct GhState {
    state: String,
}

fn github(name: &str, query: &str, hostname: Option<&str>) -> Result<Vec<Pr>> {
    let gh = |search: &str, graphql: &str| {
        let search = format!("q=is:pr is:open {search}");
        let graphql = format!("query={graphql}");
        let mut cmd = vec!["gh", "api"];
        if let Some(hostname) = hostname {
            cmd.extend(["--hostname", hostname]);
        }
        cmd.extend(["graphql", "--paginate", "-f", &search, "-f", &graphql]);
        run(&cmd)
    };
    // review-requested also finds requests to your teams, which the PR's own list of
    // reviewers can't tell apart from other teams'
    let (prs, requested) = thread::scope(|s| {
        let requested = s.spawn(|| gh(&format!("{query} review-requested:@me"), GITHUB_URLS_QUERY));
        (gh(query, GITHUB_QUERY), requested.join().unwrap())
    });
    let requested = parse_urls(&requested?)?;
    let mut prs = parse_github(name, &prs?)?;
    for p in &mut prs {
        p.requested = requested.contains(&p.url);
    }
    Ok(prs)
}

fn parse_urls(out: &str) -> Result<HashSet<String>> {
    let mut urls = HashSet::new();
    for page in serde_json::Deserializer::from_str(out).into_iter::<GhUrlPage>() {
        let page = page.context("could not read the reply from gh")?;
        urls.extend(
            page.data
                .search
                .nodes
                .iter()
                .filter_map(|n| n.get("url")?.as_str().map(String::from)),
        );
    }
    Ok(urls)
}

fn parse_github(name: &str, out: &str) -> Result<Vec<Pr>> {
    let mut prs = Vec::new();
    // --paginate prints one JSON document per page
    for page in serde_json::Deserializer::from_str(out).into_iter::<GhPage>() {
        let page = page.context("could not read the reply from gh")?;
        let me = page.data.viewer.login;
        for node in page.data.search.nodes {
            // A search result that isn't a PR comes back as null or {}
            if node.as_object().is_none_or(|o| o.is_empty()) {
                continue;
            }
            let n: GhPr = serde_json::from_value(node).context("could not read a PR from gh")?;
            // reviewDecision is null when the repo does not require reviews, so fall back to the latest verdicts
            let decision = n.review_decision.or_else(|| {
                let states = n.latest_opinionated_reviews.nodes;
                ["CHANGES_REQUESTED", "APPROVED"]
                    .into_iter()
                    .find(|s| states.iter().any(|r| r.state == *s))
                    .map(String::from)
            });
            let author = n.author.map_or_else(|| "ghost".into(), |a| a.login);
            // A branch in a fork can have the name of one here, so nothing stacks on it
            let head = if n.head_repository.as_ref() == Some(&n.repository) {
                n.head_ref_name
            } else {
                String::new()
            };
            let rollup = n
                .commits
                .nodes
                .into_iter()
                .next()
                .and_then(|c| c.commit.status_check_rollup);
            prs.push(Pr {
                host: name.into(),
                repo: n.repository.name_with_owner,
                number: n.number,
                title: n.title,
                mine: author == me,
                author,
                status: match decision.as_deref() {
                    Some("APPROVED") => Status::Approved,
                    Some("CHANGES_REQUESTED") => Status::Changes,
                    _ => Status::Needs,
                },
                ci: rollup.and_then(|r| github_ci(&r.state)),
                checks_url: format!("{}/checks", n.url),
                url: n.url,
                updated: n.updated_at,
                requested: false,
                draft: n.is_draft,
                conflict: n.mergeable == "CONFLICTING",
                // As GitHub counts them in its list of PRs
                comments: n.comments.total_count
                    + n.review_threads
                        .nodes
                        .iter()
                        .map(|t| t.comments.total_count)
                        .sum::<u64>(),
                base: n.base_ref_name,
                head,
                parent: None,
                tree: String::new(),
            });
        }
    }
    Ok(prs)
}

fn github_ci(state: &str) -> Option<Ci> {
    match state {
        "SUCCESS" => Some(Ci::Passed),
        "FAILURE" | "ERROR" => Some(Ci::Failing),
        "PENDING" | "EXPECTED" => Some(Ci::Pending),
        _ => None,
    }
}

fn tea<T: DeserializeOwned + Default>(login: &str, args: &[&str]) -> Result<T> {
    let mut cmd = vec!["tea"];
    cmd.extend_from_slice(args);
    cmd.extend_from_slice(&["--login", login, "-o", "json"]);
    let out = run(&cmd)?;
    if out.trim().is_empty() {
        return Ok(T::default());
    }
    serde_json::from_str(&out).context("could not read the reply from tea")
}

#[derive(Deserialize)]
struct TeaLogin {
    name: String,
    user: String,
}

#[derive(Deserialize)]
struct TeaIssue {
    index: String,
    title: String,
    url: String,
    #[serde(rename = "author-id")]
    author_id: String,
    owner: String,
    repo: String,
    updated: DateTime<Utc>,
}

#[derive(Deserialize, Default)]
struct TeaPull {
    #[serde(default)]
    reviews: Option<Vec<TeaReview>>,
    /// False while it has a conflict, but also while it is a draft or Forgejo checks it
    #[serde(default)]
    mergeable: Option<bool>,
}

/// A PR in a repo's list, which has what the issue search and the PR on its own don't.
#[derive(Deserialize)]
struct TeaRepoPull {
    index: String,
    /// The combined commit status, or empty without one
    #[serde(default)]
    ci: String,
    #[serde(default)]
    base: String,
    /// Its branch; the PR on its own gives refs/pull/N/head in its place
    #[serde(default)]
    head: String,
    /// Forgejo's count: each comment in the conversation and each review counts once
    #[serde(default)]
    comments: String,
}

#[derive(Deserialize)]
struct TeaReview {
    id: i64,
    reviewer: String,
    state: String,
}

fn forgejo(name: &str, login: &str, owner: Option<&str>) -> Result<Vec<Pr>> {
    let Some(me) = logins()?
        .into_iter()
        .find(|l| l.name == login)
        .map(|l| l.user)
    else {
        bail!("tea has no login named {login:?}; `tea logins list` shows them");
    };

    let mut listed: Vec<TeaIssue> = Vec::new();
    for page in 1.. {
        let page = page.to_string();
        // --repo / stops tea from limiting the search to the repo in $PWD
        let mut args = vec![
            "issues",
            "list",
            "--repo",
            "/",
            "--kind",
            "pulls",
            "--state",
            "open",
            "--limit",
            "50",
            "--page",
            &page,
            "-f",
            "index,title,url,author-id,owner,repo,updated",
        ];
        // Without an owner, a public instance returns the PRs of every public repo
        if let Some(owner) = owner {
            args.extend(["--owner", owner]);
        }
        let batch: Vec<TeaIssue> = tea(login, &args)?;
        if batch.is_empty() {
            break;
        }
        listed.extend(batch);
    }

    let status = |pr: &TeaIssue| -> Result<Pr> {
        let repo = format!("{}/{}", pr.owner, pr.repo);
        let pull: TeaPull = tea(login, &["pulls", &pr.index, "--repo", &repo])?;
        let reviews = pull.reviews.unwrap_or_default();
        let draft = forgejo_draft(&pr.title);
        Ok(Pr {
            host: name.into(),
            number: pr
                .index
                .parse()
                .with_context(|| format!("{repo} has a PR numbered {:?}", pr.index))?,
            repo,
            title: pr.title.clone(),
            url: pr.url.clone(),
            checks_url: forgejo_checks_url(&pr.url),
            author: pr.author_id.clone(),
            status: forgejo_status(&reviews),
            ci: None,
            updated: pr.updated,
            mine: pr.author_id == me,
            requested: forgejo_requested(&reviews, &me),
            draft,
            conflict: pull.mergeable == Some(false) && !draft,
            comments: 0,
            base: String::new(),
            head: String::new(),
            parent: None,
            tree: String::new(),
        })
    };

    // One tea call per PR for its reviews, and one per repo for CI and branches, all side by side
    let repos: Vec<String> = listed
        .iter()
        .map(|pr| format!("{}/{}", pr.owner, pr.repo))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let (by_repo, prs) = thread::scope(|s| {
        let by_repo = s.spawn(|| par_map(&repos, 4, |repo| forgejo_repo_pulls(login, repo)));
        let prs = par_map(&listed, 8, status);
        (by_repo.join().unwrap(), prs)
    });
    let by_repo: HashMap<&str, HashMap<String, TeaRepoPull>> = repos
        .iter()
        .map(String::as_str)
        .zip(by_repo.into_iter().collect::<Result<Vec<_>>>()?)
        .collect();
    let mut prs = prs.into_iter().collect::<Result<Vec<_>>>()?;
    for pr in &mut prs {
        let Some(listed) = by_repo
            .get(pr.repo.as_str())
            .and_then(|pulls| pulls.get(&pr.number.to_string()))
        else {
            continue;
        };
        pr.ci = forgejo_ci_state(&listed.ci);
        pr.base = listed.base.clone();
        pr.head = forgejo_head(&listed.head);
        pr.comments = listed.comments.parse().unwrap_or(0);
    }
    Ok(prs)
}

/// The open PRs in `repo`, by number. tea gives CI and branches only in a repo's list of
/// PRs, so this is one call per repo, not per PR.
fn forgejo_repo_pulls(login: &str, repo: &str) -> Result<HashMap<String, TeaRepoPull>> {
    let mut pulls = HashMap::new();
    for page in 1.. {
        let page = page.to_string();
        let batch: Vec<TeaRepoPull> = tea(
            login,
            &[
                "pulls",
                "list",
                "--repo",
                repo,
                "--state",
                "open",
                "--limit",
                "50",
                "--page",
                &page,
                "--fields",
                "index,ci,base,head,comments",
            ],
        )?;
        let last = batch.len() < 50;
        pulls.extend(batch.into_iter().map(|p| (p.index.clone(), p)));
        if last {
            break;
        }
    }
    Ok(pulls)
}

/// A branch name can't have a colon in it, so a head with one is owner:branch from another
/// repo, and nothing here stacks on it.
fn forgejo_head(head: &str) -> String {
    if head.contains(':') {
        String::new()
    } else {
        head.into()
    }
}

fn forgejo_ci_state(state: &str) -> Option<Ci> {
    match state {
        "success" => Some(Ci::Passed),
        // Gitea ranks a warning between a failure and pending, so it needs a look too
        "failure" | "error" | "warning" => Some(Ci::Failing),
        "pending" => Some(Ci::Pending),
        _ => None,
    }
}

/// The latest verdict per reviewer: any change request wins, then any approval.
fn forgejo_status(reviews: &[TeaReview]) -> Status {
    let mut latest = HashMap::new();
    for r in by_id(reviews) {
        if r.state == "APPROVED" || r.state == "REQUEST_CHANGES" {
            latest.insert(&r.reviewer, &r.state);
        }
    }
    if latest.values().any(|s| *s == "REQUEST_CHANGES") {
        Status::Changes
    } else if latest.values().any(|s| *s == "APPROVED") {
        Status::Approved
    } else {
        Status::Needs
    }
}

/// Whether the last entry from `me` asks for a review, so a review since then answers it.
fn forgejo_requested(reviews: &[TeaReview], me: &str) -> bool {
    by_id(reviews)
        .into_iter()
        .rfind(|r| r.reviewer == me)
        .is_some_and(|r| r.state == "REQUEST_REVIEW")
}

fn by_id(reviews: &[TeaReview]) -> Vec<&TeaReview> {
    let mut sorted: Vec<_> = reviews.iter().collect();
    sorted.sort_by_key(|r| r.id);
    sorted
}

/// Forgejo and Gitea mark a draft with a prefix on the title, in any case.
fn forgejo_draft(title: &str) -> bool {
    let title = title.to_uppercase();
    ["WIP:", "[WIP]"]
        .iter()
        .any(|prefix| title.starts_with(prefix))
}

/// The repo's list of action runs, as Forgejo has no checks page per PR.
fn forgejo_checks_url(pr_url: &str) -> String {
    match pr_url.rsplit_once("/pulls/") {
        Some((repo, _)) => format!("{repo}/actions"),
        None => pr_url.into(),
    }
}

/// `f` over `items` on up to `threads` threads, with the results in the order of `items`.
fn par_map<T: Sync, R: Send>(items: &[T], threads: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let results = Mutex::new((0..items.len()).map(|_| None).collect::<Vec<Option<R>>>());
    thread::scope(|s| {
        for _ in 0..threads.min(items.len()) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    let result = f(item);
                    results.lock().unwrap()[i] = Some(result);
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap()
        .into_iter()
        .map(Option::unwrap)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(id: i64, reviewer: &str, state: &str) -> TeaReview {
        TeaReview {
            id,
            reviewer: reviewer.into(),
            state: state.into(),
        }
    }

    #[test]
    fn forgejo_uses_the_latest_verdict_per_reviewer() {
        assert_eq!(forgejo_status(&[]), Status::Needs);
        assert_eq!(forgejo_status(&[review(1, "a", "COMMENT")]), Status::Needs);
        assert_eq!(
            forgejo_status(&[review(1, "a", "APPROVED")]),
            Status::Approved
        );
        // a approved after asking for changes; the ids are out of order on purpose
        assert_eq!(
            forgejo_status(&[
                review(2, "a", "APPROVED"),
                review(1, "a", "REQUEST_CHANGES")
            ]),
            Status::Approved
        );
        // A comment after a verdict doesn't undo it
        assert_eq!(
            forgejo_status(&[review(1, "a", "REQUEST_CHANGES"), review(2, "a", "COMMENT")]),
            Status::Changes
        );
        // Any open change request wins over another reviewer's approval
        assert_eq!(
            forgejo_status(&[
                review(1, "a", "APPROVED"),
                review(2, "b", "REQUEST_CHANGES")
            ]),
            Status::Changes
        );
    }

    #[test]
    fn par_map_keeps_the_order() {
        let items: Vec<u32> = (0..100).collect();
        assert_eq!(
            par_map(&items, 8, |i| i * 2),
            items.iter().map(|i| i * 2).collect::<Vec<_>>()
        );
        assert!(par_map(&[] as &[u32], 8, |i| *i).is_empty());
    }

    #[test]
    fn github_reads_every_page_and_falls_back_to_reviews() {
        let page = |me: &str, nodes: &str| {
            format!(
                r#"{{"data":{{"viewer":{{"login":"{me}"}},"search":{{"pageInfo":{{"hasNextPage":false,"endCursor":null}},"nodes":[{nodes}]}}}}}}"#
            )
        };
        let node = |number: u64, decision: &str, author: &str, reviews: &str| {
            let (draft, mergeable) = match number {
                2 => (true, "CONFLICTING"),
                _ => (false, "UNKNOWN"),
            };
            // Two conversation comments on 1, and threads of 2 and 1 inline comments on 3
            let (talk, threads) = match number {
                1 => (2, ""),
                3 => (
                    0,
                    r#"{"comments":{"totalCount":2}},{"comments":{"totalCount":1}}"#,
                ),
                _ => (0, ""),
            };
            // 2 is from a fork, and 3's fork is gone
            let head_repo = match number {
                1 => r#"{"nameWithOwner":"acme/r"}"#,
                2 => r#"{"nameWithOwner":"fork/r"}"#,
                _ => "null",
            };
            let commits = match number {
                1 => r#"{"commit":{"statusCheckRollup":{"state":"FAILURE"}}}"#,
                2 => r#"{"commit":{"statusCheckRollup":null}}"#,
                _ => "",
            };
            format!(
                r#"{{"number":{number},"title":"t","url":"u{number}","reviewDecision":{decision},"updatedAt":"2026-11-04T09:07:15Z","isDraft":{draft},"mergeable":"{mergeable}","baseRefName":"main","headRefName":"b{number}","author":{author},"repository":{{"nameWithOwner":"acme/r"}},"headRepository":{head_repo},"latestOpinionatedReviews":{{"nodes":[{reviews}]}},"commits":{{"nodes":[{commits}]}},"comments":{{"totalCount":{talk}}},"reviewThreads":{{"nodes":[{threads}]}}}}"#
            )
        };
        let out = format!(
            "{}\n{}\n",
            page(
                "me",
                &[
                    node(1, r#""APPROVED""#, r#"{"login":"me"}"#, ""),
                    "{}".into(),
                    "null".into(),
                    node(
                        2,
                        "null",
                        "null",
                        r#"{"state":"APPROVED"},{"state":"CHANGES_REQUESTED"}"#
                    ),
                ]
                .join(","),
            ),
            page(
                "me",
                &[node(
                    3,
                    r#""REVIEW_REQUIRED""#,
                    r#"{"login":"you"}"#,
                    r#"{"state":"APPROVED"}"#
                )]
                .join(",")
            ),
        );
        let prs = parse_github("GitHub", &out).unwrap();
        let got: Vec<_> = prs
            .iter()
            .map(|p| (p.number, p.status, p.ci, p.author.as_str(), p.mine))
            .collect();
        let notes: Vec<_> = prs
            .iter()
            .map(|p| (p.draft, p.conflict, p.requested))
            .collect();
        // Requests come from a second search, so none yet
        assert_eq!(
            notes,
            [
                (false, false, false),
                (true, true, false),
                (false, false, false)
            ]
        );
        assert_eq!(prs[0].checks_url, "u1/checks");
        let comments: Vec<_> = prs.iter().map(|p| p.comments).collect();
        assert_eq!(comments, [2, 0, 3]);
        let branches: Vec<_> = prs
            .iter()
            .map(|p| (p.base.as_str(), p.head.as_str()))
            .collect();
        assert_eq!(branches, [("main", "b1"), ("main", ""), ("main", "")]);
        assert_eq!(
            got,
            [
                (1, Status::Approved, Some(Ci::Failing), "me", true),
                (2, Status::Changes, None, "ghost", false),
                (3, Status::Needs, None, "you", false),
            ]
        );
    }

    #[test]
    fn the_review_request_search_reads_urls_from_every_page() {
        let out = r#"{"data":{"search":{"nodes":[{"url":"a"},{}]}}}
{"data":{"search":{"nodes":[{"url":"b"},null]}}}"#;
        let mut urls: Vec<_> = parse_urls(out).unwrap().into_iter().collect();
        urls.sort();
        assert_eq!(urls, ["a", "b"]);
    }

    #[test]
    fn forgejo_asks_for_your_review_until_you_give_one() {
        assert!(!forgejo_requested(&[], "me"));
        assert!(forgejo_requested(
            &[review(1, "me", "REQUEST_REVIEW")],
            "me"
        ));
        assert!(!forgejo_requested(
            &[review(1, "you", "REQUEST_REVIEW")],
            "me"
        ));
        // The ids are out of order on purpose
        assert!(!forgejo_requested(
            &[
                review(2, "me", "APPROVED"),
                review(1, "me", "REQUEST_REVIEW")
            ],
            "me"
        ));
        assert!(forgejo_requested(
            &[
                review(1, "me", "APPROVED"),
                review(2, "me", "REQUEST_REVIEW")
            ],
            "me"
        ));
    }

    #[test]
    fn a_forgejo_head_from_another_repo_is_left_out() {
        assert_eq!(forgejo_head("feature"), "feature");
        assert_eq!(forgejo_head("someone:feature"), "");
    }

    #[test]
    fn forgejo_drafts_and_checks_pages() {
        assert!(forgejo_draft("WIP: new thing"));
        assert!(forgejo_draft("[wip] new thing"));
        assert!(!forgejo_draft("Fix the WIP: label"));
        assert_eq!(
            forgejo_checks_url("https://git.example.com/acme/app/pulls/12"),
            "https://git.example.com/acme/app/actions"
        );
    }

    #[test]
    fn ci_states_map_to_passed_failing_and_pending() {
        let github: Vec<_> = ["SUCCESS", "FAILURE", "ERROR", "PENDING", "EXPECTED", "NEW"]
            .map(github_ci)
            .into();
        let forgejo: Vec<_> = ["success", "failure", "error", "warning", "pending", ""]
            .map(forgejo_ci_state)
            .into();
        use Ci::*;
        assert_eq!(
            github,
            [
                Some(Passed),
                Some(Failing),
                Some(Failing),
                Some(Pending),
                Some(Pending),
                None
            ]
        );
        assert_eq!(
            forgejo,
            [
                Some(Passed),
                Some(Failing),
                Some(Failing),
                Some(Failing),
                Some(Pending),
                None
            ]
        );
    }
}
