use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::pr::{Host, ORG, Pr, Status};

/// tea login for https://git.example.com, signed with the ssh-agent key
const TEA_LOGIN: &str = "work";

pub fn fetch(host: Host) -> Result<Vec<Pr>> {
    match host {
        Host::GitHub => github(),
        Host::Forgejo => forgejo(),
    }
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
      number title url reviewDecision updatedAt
      author { login }
      repository { nameWithOwner }
      latestOpinionatedReviews(first: 20) { nodes { state } }
    } }
  }
}";

#[derive(Deserialize)]
struct GhPage {
    data: GhData,
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
    author: Option<GhLogin>,
    repository: GhRepo,
    latest_opinionated_reviews: GhReviews,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhRepo {
    name_with_owner: String,
}

#[derive(Deserialize)]
struct GhReviews {
    nodes: Vec<GhReview>,
}

#[derive(Deserialize)]
struct GhReview {
    state: String,
}

fn github() -> Result<Vec<Pr>> {
    let out = run(&[
        "gh",
        "api",
        "graphql",
        "--paginate",
        "-f",
        &format!("q=is:pr is:open org:{ORG}"),
        "-f",
        &format!("query={GITHUB_QUERY}"),
    ])?;
    parse_github(&out)
}

fn parse_github(out: &str) -> Result<Vec<Pr>> {
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
            prs.push(Pr {
                host: Host::GitHub,
                repo: n.repository.name_with_owner,
                number: n.number,
                title: n.title,
                url: n.url,
                mine: author == me,
                author,
                status: match decision.as_deref() {
                    Some("APPROVED") => Status::Approved,
                    Some("CHANGES_REQUESTED") => Status::Changes,
                    _ => Status::Needs,
                },
                updated: n.updated_at,
            });
        }
    }
    Ok(prs)
}

fn tea<T: DeserializeOwned + Default>(args: &[&str]) -> Result<T> {
    let mut cmd = vec!["tea"];
    cmd.extend_from_slice(args);
    cmd.extend_from_slice(&["--login", TEA_LOGIN, "-o", "json"]);
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
}

#[derive(Deserialize)]
struct TeaReview {
    id: i64,
    reviewer: String,
    state: String,
}

fn forgejo() -> Result<Vec<Pr>> {
    let logins: Vec<TeaLogin> =
        serde_json::from_str(&run(&["tea", "logins", "list", "-o", "json"])?)
            .context("could not read the logins from tea")?;
    let me = logins
        .into_iter()
        .find(|l| l.name == TEA_LOGIN)
        .map(|l| l.user);

    let mut listed: Vec<TeaIssue> = Vec::new();
    for page in 1.. {
        // --repo / stops tea from limiting the search to the repo in $PWD
        let batch: Vec<TeaIssue> = tea(&[
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
            &page.to_string(),
            "-f",
            "index,title,url,author-id,owner,repo,updated",
        ])?;
        if batch.is_empty() {
            break;
        }
        listed.extend(batch);
    }

    let status = |pr: &TeaIssue| -> Result<Pr> {
        let repo = format!("{}/{}", pr.owner, pr.repo);
        let pull: TeaPull = tea(&["pulls", &pr.index, "--repo", &repo])?;
        Ok(Pr {
            host: Host::Forgejo,
            number: pr
                .index
                .parse()
                .with_context(|| format!("{repo} has a PR numbered {:?}", pr.index))?,
            repo,
            title: pr.title.clone(),
            url: pr.url.clone(),
            author: pr.author_id.clone(),
            status: forgejo_status(pull.reviews.unwrap_or_default()),
            updated: pr.updated,
            mine: me.as_deref() == Some(pr.author_id.as_str()),
        })
    };

    // One tea call per PR, so run them side by side
    par_map(&listed, 8, status).into_iter().collect()
}

/// The latest verdict per reviewer: any change request wins, then any approval.
fn forgejo_status(mut reviews: Vec<TeaReview>) -> Status {
    reviews.sort_by_key(|r| r.id);
    let mut latest = HashMap::new();
    for r in &reviews {
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
        assert_eq!(forgejo_status(vec![]), Status::Needs);
        assert_eq!(
            forgejo_status(vec![review(1, "a", "COMMENT")]),
            Status::Needs
        );
        assert_eq!(
            forgejo_status(vec![review(1, "a", "APPROVED")]),
            Status::Approved
        );
        // a approved after asking for changes; the ids are out of order on purpose
        assert_eq!(
            forgejo_status(vec![
                review(2, "a", "APPROVED"),
                review(1, "a", "REQUEST_CHANGES")
            ]),
            Status::Approved
        );
        // A comment after a verdict doesn't undo it
        assert_eq!(
            forgejo_status(vec![
                review(1, "a", "REQUEST_CHANGES"),
                review(2, "a", "COMMENT")
            ]),
            Status::Changes
        );
        // Any open change request wins over another reviewer's approval
        assert_eq!(
            forgejo_status(vec![
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
            format!(
                r#"{{"number":{number},"title":"t","url":"u{number}","reviewDecision":{decision},"updatedAt":"2026-11-04T09:07:15Z","author":{author},"repository":{{"nameWithOwner":"acme/r"}},"latestOpinionatedReviews":{{"nodes":[{reviews}]}}}}"#
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
        let prs = parse_github(&out).unwrap();
        let got: Vec<_> = prs
            .iter()
            .map(|p| (p.number, p.status, p.author.as_str(), p.mine))
            .collect();
        assert_eq!(
            got,
            [
                (1, Status::Approved, "me", true),
                (2, Status::Changes, "ghost", false),
                (3, Status::Needs, "you", false),
            ]
        );
    }
}
