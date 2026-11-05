use std::collections::{BTreeSet, HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::config::{Kind, Login, Source};
use crate::http::{Client, Http, Refused};
use crate::pr::{Ci, Pr, Status};
use crate::preview::{self, Check, Comment, FileChange, Files, Preview};
use crate::reviewers::{self as review, Reviewers};
use crate::token;

pub fn fetch(source: &Source) -> Result<Vec<Pr>> {
    let found = match &source.kind {
        Kind::GitHub {
            query,
            hostname,
            token: None,
        } => github(source.name(), query, &Gh::Cli(hostname.as_deref())),
        Kind::GitHub {
            query, hostname, ..
        } => {
            let http = api(source)?;
            github(source.name(), query, &Gh::api(&http, hostname.as_deref()))
        }
        Kind::Forgejo {
            login: Login::Tea(login),
            owner,
        } => forgejo(source.name(), login, owner.as_deref()),
        Kind::Forgejo { owner, .. } => forgejo_api(source.name(), &api(source)?, owner.as_deref()),
    };
    refused(source, found)
}

/// The description, checks, files and last comments of `pr`, which `source` found.
pub fn preview(source: &Source, pr: &Pr) -> Result<Preview> {
    let found = match &source.kind {
        Kind::GitHub {
            hostname,
            token: None,
            ..
        } => github_preview(pr, &Gh::Cli(hostname.as_deref())),
        Kind::GitHub { hostname, .. } => {
            let http = api(source)?;
            github_preview(pr, &Gh::api(&http, hostname.as_deref()))
        }
        Kind::Forgejo {
            login: Login::Tea(login),
            ..
        } => forgejo_preview(login, &pr.repo, pr.number),
        Kind::Forgejo { .. } => forgejo_api_preview(&api(source)?, &pr.repo, pr.number),
    };
    refused(source, found)
}

/// Who reviews `pr`, which `source` found, and who can. `known` is the people in the
/// source's PRs, for a tea login, which can't ask who can review.
pub fn reviewers(source: &Source, pr: &Pr, known: &[String]) -> Result<Reviewers> {
    let found = match &source.kind {
        Kind::GitHub {
            hostname,
            token: None,
            ..
        } => github_reviewers(pr, &Gh::Cli(hostname.as_deref())),
        Kind::GitHub { hostname, .. } => {
            let http = api(source)?;
            github_reviewers(pr, &Gh::api(&http, hostname.as_deref()))
        }
        Kind::Forgejo {
            login: Login::Tea(login),
            ..
        } => forgejo_reviewers(login, pr, known),
        Kind::Forgejo { .. } => forgejo_api_reviewers(&api(source)?, pr),
    };
    refused(source, found)
}

/// Asks for the reviews in `change.add` and stops asking for the ones in `change.remove`.
pub fn request_reviews(source: &Source, pr: &Pr, change: &review::Change) -> Result<()> {
    let done = match &source.kind {
        Kind::GitHub {
            hostname,
            token: None,
            ..
        } => {
            let gh = Gh::Cli(hostname.as_deref());
            github_review_calls(&pr.repo, pr.number, change)
                .iter()
                .try_for_each(|call| gh.review(call))
        }
        Kind::GitHub { hostname, .. } => {
            let http = api(source)?;
            let gh = Gh::api(&http, hostname.as_deref());
            github_review_calls(&pr.repo, pr.number, change)
                .iter()
                .try_for_each(|call| gh.review(call))
        }
        Kind::Forgejo {
            login: Login::Tea(login),
            ..
        } => {
            let args = forgejo_review_args(login, &pr.repo, pr.number, change);
            let mut cmd = vec!["tea"];
            cmd.extend(args.iter().map(String::as_str));
            run(&cmd).map(|_| ())
        }
        Kind::Forgejo { .. } => {
            let http = api(source)?;
            review_calls(&format!("repos/{}/pulls/{}", pr.repo, pr.number), change)
                .iter()
                .try_for_each(|call| {
                    http.send(call.method, &format!("/{}", call.endpoint), &call.json())
                        .map(|_| ())
                })
        }
    };
    refused(source, done)
}

/// The client for a source with a token of its own.
fn api(source: &Source) -> Result<Client> {
    let token = token::get(source)?;
    match &source.kind {
        Kind::GitHub { hostname, .. } => {
            let base = match hostname.as_deref() {
                None | Some("github.com") => "https://api.github.com".to_owned(),
                Some(hostname) => format!("https://{hostname}/api/v3"),
            };
            let auth = format!("Bearer {}", token.expose());
            Client::new(&base, auth, source.name(), false, true)
        }
        Kind::Forgejo {
            login: Login::Token {
                url, allow_http, ..
            },
            ..
        } => {
            let auth = format!("token {}", token.expose());
            Client::new(
                &format!("{url}/api/v1"),
                auth,
                source.name(),
                *allow_http,
                false,
            )
        }
        Kind::Forgejo { .. } => bail!("{} uses tea's login", source.name()),
    }
}

/// When the host refused the token, looks for it again next time, as the user may fix it.
fn refused<T>(source: &Source, result: Result<T>) -> Result<T> {
    if let Err(e) = &result
        && e.downcast_ref::<Refused>().is_some()
    {
        token::forget(source);
    }
    result
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
    let mut command = Command::new(cmd[0]);
    // gh and tea need their own settings, but not the tokens of the other sources
    for name in token::hidden() {
        command.env_remove(name);
    }
    // stdin is null, so gh or tea can't read the keys meant for the TUI
    let out = command
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

/// How a GitHub source reaches GitHub: through gh's login, or the API with its own token.
/// Each gives the same JSON, so only this changes.
enum Gh<'a> {
    /// gh, with the hostname for GitHub Enterprise
    Cli(Option<&'a str>),
    Api {
        http: &'a dyn Http,
        /// Where GraphQL is: /graphql on api.github.com, /api/graphql on GitHub Enterprise
        graphql: String,
    },
}

impl<'a> Gh<'a> {
    fn api(http: &'a dyn Http, hostname: Option<&str>) -> Self {
        let graphql = match hostname {
            None | Some("github.com") => "/graphql".to_owned(),
            Some(hostname) => format!("https://{hostname}/api/graphql"),
        };
        Gh::Api { http, graphql }
    }

    /// `gh api` with `args`.
    fn gh(hostname: Option<&str>, args: &[&str]) -> Result<String> {
        let mut cmd = vec!["gh", "api"];
        if let Some(hostname) = hostname {
            cmd.extend(["--hostname", hostname]);
        }
        cmd.extend(args);
        run(&cmd)
    }

    /// Runs `query` with `vars`. With `paginate`, it reads every page of the connection with
    /// pageInfo, as `gh api graphql --paginate` does, and gives one JSON document per page.
    fn graphql(&self, query: &str, vars: &[(&str, &str)], paginate: bool) -> Result<String> {
        let (http, endpoint) = match self {
            Gh::Cli(hostname) => {
                let fields: Vec<String> = vars
                    .iter()
                    .map(|(name, value)| format!("{name}={value}"))
                    .chain([format!("query={query}")])
                    .collect();
                let mut args = vec!["graphql"];
                if paginate {
                    args.push("--paginate");
                }
                for field in &fields {
                    args.extend(["-f", field]);
                }
                return Self::gh(*hostname, &args);
            }
            Gh::Api { http, graphql } => (http, graphql),
        };
        let mut out = String::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut variables: serde_json::Map<String, Value> = vars
                .iter()
                .map(|(name, value)| ((*name).to_owned(), Value::from(*value)))
                .collect();
            if let Some(cursor) = &cursor {
                variables.insert("endCursor".into(), cursor.as_str().into());
            }
            let body = json!({ "query": query, "variables": variables });
            let reply = http.send("POST", endpoint, &body)?;
            let page: Value =
                serde_json::from_str(&reply).context("could not read the reply from GitHub")?;
            if let Some(errors) = page.get("errors").and_then(Value::as_array)
                && !errors.is_empty()
            {
                // What GitHub leaves out for a fine-grained token, it says in errors, and the
                // rest of the reply still holds
                let fatal: Vec<&Value> = errors.iter().filter(|e| left_out(e).is_none()).collect();
                if page.get("data").is_none_or(Value::is_null) || !fatal.is_empty() {
                    let shown = if fatal.is_empty() {
                        errors.iter().collect()
                    } else {
                        fatal
                    };
                    bail!("GitHub: {}", graphql_errors(&shown));
                }
            }
            out.push_str(&reply);
            out.push('\n');
            match page_info(&page) {
                Some(next) if paginate => cursor = Some(next),
                _ => break,
            }
        }
        Ok(out)
    }

    /// GETs a REST endpoint and its next pages: one JSON document per page, as
    /// `gh api --paginate` prints them.
    fn pages(&self, endpoint: &str) -> Result<String> {
        let http = match self {
            Gh::Cli(hostname) => return Self::gh(*hostname, &[endpoint, "--paginate"]),
            Gh::Api { http, .. } => http,
        };
        let mut out = String::new();
        let mut next = Some(format!("/{endpoint}?per_page=100"));
        while let Some(path) = next {
            let reply = http.get(&path)?;
            out.push_str(&reply.body);
            out.push('\n');
            next = reply.next;
        }
        Ok(out)
    }

    fn review(&self, call: &ReviewCall) -> Result<()> {
        match self {
            Gh::Cli(hostname) => {
                let args = call.gh_args();
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                Self::gh(*hostname, &args).map(|_| ())
            }
            Gh::Api { http, .. } => http
                .send(call.method, &format!("/{}", call.endpoint), &call.json())
                .map(|_| ()),
        }
    }
}

/// A part of the reply that GitHub left out, as the token may not read it.
#[derive(Debug, PartialEq)]
enum LeftOut {
    /// A PR's commits, or their checks. A fine-grained token can't read check runs, and
    /// without Contents: Read, GraphQL doesn't give it a PR's commits either.
    Ci,
    /// A whole PR, in a repo the token can't see
    Pr,
}

/// What `error` says GitHub left out, if the rest of the reply still holds. What SAML SSO
/// keeps out is not left out: the user can authorize the token.
fn left_out(error: &Value) -> Option<LeftOut> {
    if error.get("type").and_then(Value::as_str) != Some("FORBIDDEN")
        || error["extensions"]["saml_failure"] == true
    {
        return None;
    }
    let path: Vec<&Value> = error
        .get("path")
        .and_then(Value::as_array)
        .map(|p| p.iter().collect())
        .unwrap_or_default();
    let named = |name: &str| path.iter().any(|p| p.as_str() == Some(name));
    if named("commits") || named("statusCheckRollup") {
        Some(LeftOut::Ci)
    } else if matches!(path.as_slice(), [search, nodes, n]
        if search.as_str() == Some("search") && nodes.as_str() == Some("nodes") && n.is_u64())
    {
        Some(LeftOut::Pr)
    } else {
        None
    }
}

/// Whether GitHub left out the CI of the PRs in `out`, the replies to GraphQL.
fn ci_left_out(out: &str) -> bool {
    serde_json::Deserializer::from_str(out)
        .into_iter::<Value>()
        .filter_map(Result::ok)
        .flat_map(|page| {
            page.get("errors")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .any(|e| left_out(&e) == Some(LeftOut::Ci))
}

/// Each message of GraphQL's errors once, with how many times it came, and where it came first.
fn graphql_errors(errors: &[&Value]) -> String {
    let mut seen: Vec<(String, usize, String)> = Vec::new();
    for error in errors {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("an error with no message")
            .to_owned();
        let path = error
            .get("path")
            .and_then(Value::as_array)
            .map(|path| {
                path.iter()
                    .map(|part| match part {
                        Value::String(name) => name.clone(),
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(".")
            })
            .unwrap_or_default();
        match seen.iter_mut().find(|(m, ..)| *m == message) {
            Some((_, count, _)) => *count += 1,
            None => seen.push((message, 1, path)),
        }
    }
    seen.into_iter()
        .map(|(message, count, path)| match (count, path.is_empty()) {
            (1, true) => message,
            (1, false) => format!("{message} (at {path})"),
            (count, true) => format!("{message} ({count} times)"),
            (count, false) => format!("{message} ({count} times, first at {path})"),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The cursor of the next page, from the first pageInfo in the reply that has one.
fn page_info(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let Some(info) = map.get("pageInfo") {
                return info
                    .get("hasNextPage")?
                    .as_bool()?
                    .then(|| info.get("endCursor")?.as_str().map(str::to_owned))
                    .flatten();
            }
            map.values().find_map(page_info)
        }
        Value::Array(items) => items.iter().find_map(page_info),
        _ => None,
    }
}

const GITHUB_QUERY: &str = "
query($q: String!, $endCursor: String) {
  viewer { login }
  search(query: $q, type: ISSUE, first: 50, after: $endCursor) {
    pageInfo { hasNextPage endCursor }
    nodes { ... on PullRequest {
      number title url reviewDecision updatedAt isDraft mergeable baseRefName headRefName headRefOid
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
    /// null when the token may not read it
    nodes: Vec<Option<GhCommitNode>>,
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

fn github(name: &str, query: &str, via: &Gh) -> Result<Vec<Pr>> {
    let gh = |search: &str, graphql: &str| {
        let search = format!("is:pr is:open {search}");
        via.graphql(graphql, &[("q", &search)], true)
    };
    // review-requested also finds requests to your teams, which the PR's own list of
    // reviewers can't tell apart from other teams'
    let (prs, requested) = thread::scope(|s| {
        let requested = s.spawn(|| gh(&format!("{query} review-requested:@me"), GITHUB_URLS_QUERY));
        (gh(query, GITHUB_QUERY), requested.join().unwrap())
    });
    let requested = parse_urls(&requested?)?;
    let out = prs?;
    let mut prs = parse_github(name, &out)?;
    for p in &mut prs {
        p.requested = requested.contains(&p.url);
    }
    // GitHub leaves out the CI for a fine-grained token, which can't read check runs; the
    // commit statuses and the Actions runs say most of it
    if let Gh::Api { http, .. } = via
        && ci_left_out(&out)
    {
        let shas = github_shas(&out);
        let found = par_map(&prs, 8, |p| {
            let sha = shas.get(&p.url)?;
            rollup(&kept_checks(name, *http, &p.repo, sha))
        });
        for (p, ci) in prs.iter_mut().zip(found) {
            if p.ci.is_none() {
                p.ci = ci;
            }
        }
    }
    Ok(prs)
}

/// The commit at the head of each PR in `out`, by the PR's URL.
fn github_shas(out: &str) -> HashMap<String, String> {
    let mut shas = HashMap::new();
    for page in serde_json::Deserializer::from_str(out).into_iter::<Value>() {
        let Ok(page) = page else { continue };
        let nodes = page["data"]["search"]["nodes"].as_array().cloned();
        for node in nodes.unwrap_or_default() {
            if let (Some(url), Some(sha)) = (node["url"].as_str(), node["headRefOid"].as_str()) {
                shas.insert(url.to_owned(), sha.to_owned());
            }
        }
    }
    shas
}

#[derive(Deserialize)]
struct GhCombined {
    #[serde(default)]
    statuses: Vec<GhStatus>,
}

#[derive(Deserialize)]
struct GhStatus {
    context: String,
    state: String,
}

#[derive(Deserialize)]
struct GhRuns {
    #[serde(default)]
    workflow_runs: Vec<GhRun>,
}

#[derive(Deserialize)]
struct GhRun {
    name: Option<String>,
    event: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
}

/// How long the checks of a commit stay kept once they are all done; only a new run of
/// one changes them. All of a user's tokens share 5000 REST calls an hour.
const KEEP_CHECKS: Duration = Duration::from_secs(10 * 60);

/// The checks found over REST, by source, repo and commit, with when they were found.
type KeptChecks = HashMap<(String, String, String), (Instant, Vec<Check>)>;

static CHECKS: LazyLock<Mutex<KeptChecks>> = LazyLock::new(Mutex::default);

/// The checks on `sha`, from those kept when they are all done and recent.
fn kept_checks(source: &str, http: &dyn Http, repo: &str, sha: &str) -> Vec<Check> {
    let key = (source.to_owned(), repo.to_owned(), sha.to_owned());
    if let Some((at, checks)) = CHECKS.lock().unwrap().get(&key)
        && at.elapsed() < KEEP_CHECKS
    {
        return checks.clone();
    }
    let checks = rest_checks(http, repo, sha);
    let mut kept = CHECKS.lock().unwrap();
    kept.retain(|_, (at, _)| at.elapsed() < KEEP_CHECKS);
    // CI that has not started yet shows as no checks, so only checks that are done stay
    if !checks.is_empty() && checks.iter().all(|c| c.state != Ci::Pending) {
        kept.insert(key, (Instant::now(), checks.clone()));
    }
    checks
}

/// The checks on `sha` that a fine-grained token can read through REST: the commit statuses,
/// and the runs of GitHub Actions. A token without the permission for one still gets the other.
fn rest_checks(http: &dyn Http, repo: &str, sha: &str) -> Vec<Check> {
    let (statuses, runs) = thread::scope(|s| {
        let runs = s.spawn(|| {
            let path = format!("/repos/{repo}/actions/runs?head_sha={sha}&per_page=100");
            get_json::<GhRuns>(http, &path)
        });
        let statuses = get_json::<GhCombined>(http, &format!("/repos/{repo}/commits/{sha}/status"));
        (statuses, runs.join().unwrap())
    });
    let mut checks: Vec<Check> = statuses
        .map(|s| s.statuses)
        .unwrap_or_default()
        .into_iter()
        .map(|s| Check {
            name: s.context,
            state: github_ci(&s.state.to_ascii_uppercase()).unwrap_or(Ci::Pending),
        })
        .collect();
    // The newest run comes first; an older run of the same workflow, for the same event, is
    // one that a new run replaced
    let mut seen = HashSet::new();
    checks.extend(
        runs.map(|r| r.workflow_runs)
            .unwrap_or_default()
            .into_iter()
            .filter(|r| seen.insert((r.name.clone(), r.event.clone())))
            .map(|r| Check {
                name: r.name.unwrap_or_else(|| "a workflow".into()),
                state: match (r.status.as_deref(), r.conclusion.as_deref()) {
                    (Some("completed"), Some("success" | "neutral" | "skipped")) => Ci::Passed,
                    (Some("completed"), _) => Ci::Failing,
                    _ => Ci::Pending,
                },
            }),
    );
    checks
}

/// The state of `checks` together, as GitHub's rollup gives it: any failing check fails.
fn rollup(checks: &[Check]) -> Option<Ci> {
    if checks.iter().any(|c| c.state == Ci::Failing) {
        Some(Ci::Failing)
    } else if checks.iter().any(|c| c.state == Ci::Pending) {
        Some(Ci::Pending)
    } else if checks.is_empty() {
        None
    } else {
        Some(Ci::Passed)
    }
}

fn parse_urls(out: &str) -> Result<HashSet<String>> {
    let mut urls = HashSet::new();
    for page in serde_json::Deserializer::from_str(out).into_iter::<GhUrlPage>() {
        let page = page.context("could not read the reply from GitHub")?;
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
        let page = page.context("could not read the reply from GitHub")?;
        let me = page.data.viewer.login;
        for node in page.data.search.nodes {
            // A search result that isn't a PR comes back as null or {}
            if node.as_object().is_none_or(|o| o.is_empty()) {
                continue;
            }
            let n: GhPr =
                serde_json::from_value(node).context("could not read a PR from GitHub")?;
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
                .flatten()
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
                new: false,
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

const GITHUB_PREVIEW_QUERY: &str = "
query($url: URI!) {
  resource(url: $url) { ... on PullRequest {
    body additions deletions changedFiles headRefOid
    files(first: 100) { nodes { path additions deletions } }
    comments(last: 10) { nodes { author { login } body createdAt } }
    reviews(last: 10) { nodes { author { login } state body submittedAt } }
    commits(last: 1) { nodes { commit { statusCheckRollup { contexts(first: 100) { nodes {
      __typename
      ... on CheckRun { name status conclusion }
      ... on StatusContext { context state }
    } } } } } }
  } }
}";

#[derive(Deserialize)]
struct GhPreviewPage {
    data: GhPreviewData,
}

#[derive(Deserialize)]
struct GhPreviewData {
    /// null when there is no such PR, or no access to it
    resource: Option<GhPreview>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPreview {
    body: String,
    additions: u64,
    deletions: u64,
    changed_files: u64,
    files: Option<GhNodes<GhFile>>,
    comments: GhNodes<GhComment>,
    reviews: GhNodes<GhReview>,
    /// A node is null when the token may not read it
    commits: GhNodes<Option<GhContextCommitNode>>,
}

#[derive(Deserialize)]
struct GhNodes<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct GhFile {
    path: String,
    additions: u64,
    deletions: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhComment {
    author: Option<GhLogin>,
    body: String,
    created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhReview {
    author: Option<GhLogin>,
    state: String,
    body: String,
    /// null while it is pending
    submitted_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct GhContextCommitNode {
    commit: GhContextCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhContextCommit {
    status_check_rollup: Option<GhRollup>,
}

#[derive(Deserialize)]
struct GhRollup {
    contexts: GhNodes<GhContext>,
}

/// A check run from Actions or an app, or a commit status
#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum GhContext {
    CheckRun {
        name: String,
        status: String,
        conclusion: Option<String>,
    },
    StatusContext {
        context: String,
        state: String,
    },
}

fn github_preview(pr: &Pr, via: &Gh) -> Result<Preview> {
    let out = via.graphql(GITHUB_PREVIEW_QUERY, &[("url", &pr.url)], false)?;
    let mut preview = parse_github_preview(&out)?;
    // As for the list, the checks of a fine-grained token come from REST
    if let Gh::Api { http, .. } = via
        && ci_left_out(&out)
        && let Some(sha) = serde_json::from_str::<Value>(&out).ok().and_then(|page| {
            page["data"]["resource"]["headRefOid"]
                .as_str()
                .map(str::to_owned)
        })
    {
        preview.checks = rest_checks(*http, &pr.repo, &sha);
    }
    Ok(preview)
}

fn parse_github_preview(out: &str) -> Result<Preview> {
    let page: GhPreviewPage =
        serde_json::from_str(out).context("could not read the reply from GitHub")?;
    let pr = page.data.resource.context("GitHub found no such PR")?;
    let login = |a: Option<GhLogin>| a.map_or_else(|| "ghost".into(), |a| a.login);
    let mut comments: Vec<Comment> = pr
        .comments
        .nodes
        .into_iter()
        .map(|c| Comment {
            author: login(c.author),
            when: c.created_at,
            body: c.body,
            verdict: None,
        })
        .collect();
    for r in pr.reviews.nodes {
        let verdict = match r.state.as_str() {
            "APPROVED" => "approved",
            "CHANGES_REQUESTED" => "changes requested",
            // A review with only inline comments has no text of its own
            "COMMENTED" | "DISMISSED" if !r.body.trim().is_empty() => "reviewed",
            _ => continue,
        };
        let Some(when) = r.submitted_at else { continue };
        comments.push(Comment {
            author: login(r.author),
            when,
            body: r.body,
            verdict: Some(verdict),
        });
    }
    let contexts = pr
        .commits
        .nodes
        .into_iter()
        .flatten()
        .next()
        .and_then(|c| c.commit.status_check_rollup)
        .map_or_else(Vec::new, |r| r.contexts.nodes);
    let checks = contexts
        .into_iter()
        .map(|c| match c {
            GhContext::CheckRun {
                name,
                status,
                conclusion,
            } => Check {
                name,
                state: match (status.as_str(), conclusion.as_deref()) {
                    ("COMPLETED", Some("SUCCESS" | "NEUTRAL" | "SKIPPED")) => Ci::Passed,
                    ("COMPLETED", _) => Ci::Failing,
                    _ => Ci::Pending,
                },
            },
            GhContext::StatusContext { context, state } => Check {
                name: context,
                state: github_ci(&state).unwrap_or(Ci::Pending),
            },
        })
        .collect();
    Ok(Preview {
        body: pr.body,
        files: Some(Files {
            additions: pr.additions,
            deletions: pr.deletions,
            count: pr.changed_files,
            changed: pr
                .files
                .map_or_else(Vec::new, |f| f.nodes)
                .into_iter()
                .map(|f| FileChange {
                    path: f.path,
                    additions: f.additions,
                    deletions: f.deletions,
                })
                .collect(),
        }),
        checks,
        comments: preview::last_comments(comments),
    })
}

const GITHUB_REVIEWERS_QUERY: &str = "
query($url: URI!) {
  resource(url: $url) { ... on PullRequest {
    author { login }
    reviewRequests(first: 50) { nodes { requestedReviewer {
      __typename
      ... on User { login }
      ... on Team { slug organization { login } }
    } } }
    latestReviews(first: 50) { nodes { author { login } state } }
    suggestedReviewers { reviewer { login } }
    repository { assignableUsers(first: 100) { nodes { login } } }
  } }
}";

#[derive(Deserialize)]
struct GhReviewersPage {
    data: GhReviewersData,
}

#[derive(Deserialize)]
struct GhReviewersData {
    resource: Option<GhReviewersPr>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhReviewersPr {
    author: Option<GhLogin>,
    review_requests: GhNodes<GhRequest>,
    latest_reviews: GhNodes<GhLatestReview>,
    suggested_reviewers: Vec<GhSuggested>,
    repository: GhAssignable,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhRequest {
    requested_reviewer: Option<GhRequested>,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum GhRequested {
    User {
        login: String,
    },
    Team {
        slug: String,
        organization: GhLogin,
    },
    /// A bot or a mannequin
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct GhLatestReview {
    author: Option<GhLogin>,
    state: String,
}

#[derive(Deserialize)]
struct GhSuggested {
    reviewer: GhLogin,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhAssignable {
    assignable_users: GhNodes<GhLogin>,
}

#[derive(Deserialize)]
struct GhTeam {
    slug: String,
}

fn github_reviewers(pr: &Pr, via: &Gh) -> Result<Reviewers> {
    let teams_endpoint = format!("repos/{}/teams", pr.repo);
    let (out, teams) = thread::scope(|s| {
        let teams = s.spawn(|| via.pages(&teams_endpoint));
        let out = via.graphql(GITHUB_REVIEWERS_QUERY, &[("url", &pr.url)], false);
        (out, teams.join().unwrap())
    });
    // The people still work without the teams, as when no one may see them, or a token
    // without the large permission that lists them
    let teams = teams
        .and_then(|out| github_teams(&out, pr.owner()))
        .unwrap_or_default();
    parse_github_reviewers(&out?, teams)
}

/// The teams with access to the repo, as owner/slug: only they can review its PRs.
fn github_teams(out: &str, owner: &str) -> Result<Vec<String>> {
    let mut teams = Vec::new();
    // --paginate prints one JSON array per page
    for page in serde_json::Deserializer::from_str(out).into_iter::<Vec<GhTeam>>() {
        let page = page.context("could not read the teams from GitHub")?;
        teams.extend(page.into_iter().map(|t| format!("{owner}/{}", t.slug)));
    }
    Ok(teams)
}

/// The people are GitHub's suggestions, then `teams`, then the users who can be assigned.
fn parse_github_reviewers(out: &str, teams: Vec<String>) -> Result<Reviewers> {
    let page: GhReviewersPage =
        serde_json::from_str(out).context("could not read the reply from GitHub")?;
    let pr = page.data.resource.context("GitHub found no such PR")?;
    let author = pr.author.map(|a| a.login);
    let requested = pr
        .review_requests
        .nodes
        .into_iter()
        .filter_map(|r| match r.requested_reviewer? {
            GhRequested::User { login } => Some(login),
            GhRequested::Team { slug, organization } => {
                Some(format!("{}/{slug}", organization.login))
            }
            GhRequested::Other => None,
        })
        .collect();
    let verdicts = pr
        .latest_reviews
        .nodes
        .into_iter()
        .filter_map(|r| {
            let verdict = match r.state.as_str() {
                "APPROVED" => "approved",
                "CHANGES_REQUESTED" => "changes requested",
                "COMMENTED" => "commented",
                "DISMISSED" => "dismissed",
                _ => return None,
            };
            Some((r.author?.login, verdict))
        })
        .collect();
    let mut people: Vec<String> = Vec::new();
    let names = pr
        .suggested_reviewers
        .into_iter()
        .map(|s| s.reviewer.login)
        .chain(teams)
        .chain(
            pr.repository
                .assignable_users
                .nodes
                .into_iter()
                .map(|u| u.login),
        );
    for name in names {
        if Some(&name) != author.as_ref() && !people.contains(&name) {
            people.push(name);
        }
    }
    Ok(Reviewers {
        requested,
        people,
        verdicts,
    })
}

/// A call that asks for reviews, or stops asking for them.
#[derive(Debug, PartialEq, Eq)]
struct ReviewCall {
    /// POST to ask, DELETE to stop asking
    method: &'static str,
    /// From the API's base, without a / at the start
    endpoint: String,
    users: Vec<String>,
    /// The teams' names, without the org
    teams: Vec<String>,
}

impl ReviewCall {
    /// The arguments to `gh api`.
    fn gh_args(&self) -> Vec<String> {
        let mut args = vec![
            "-X".to_owned(),
            self.method.to_owned(),
            self.endpoint.clone(),
        ];
        // GitHub wants reviewers, if only empty
        if self.users.is_empty() {
            args.extend(["-f".into(), "reviewers[]".into()]);
        }
        for user in &self.users {
            args.extend(["-f".into(), format!("reviewers[]={user}")]);
        }
        for team in &self.teams {
            args.extend(["-f".into(), format!("team_reviewers[]={team}")]);
        }
        args
    }

    /// The body for the API, which GitHub and Forgejo take alike.
    fn json(&self) -> Value {
        json!({ "reviewers": self.users, "team_reviewers": self.teams })
    }
}

/// The calls that make `change` to the PR at `pull`, such as repos/acme/app/pulls/7. A team is
/// org/name, and the host wants only the name.
fn review_calls(pull: &str, change: &review::Change) -> Vec<ReviewCall> {
    let call = |method, names: &[String]| {
        let (teams, users): (Vec<&String>, Vec<&String>) =
            names.iter().partition(|n| n.contains('/'));
        ReviewCall {
            method,
            endpoint: format!("{pull}/requested_reviewers"),
            users: users.into_iter().cloned().collect(),
            teams: teams
                .into_iter()
                .map(|t| t.rsplit('/').next().unwrap_or(t).to_owned())
                .collect(),
        }
    };
    let mut calls = Vec::new();
    if !change.add.is_empty() {
        calls.push(call("POST", &change.add));
    }
    if !change.remove.is_empty() {
        calls.push(call("DELETE", &change.remove));
    }
    calls
}

fn github_review_calls(repo: &str, number: u64, change: &review::Change) -> Vec<ReviewCall> {
    review_calls(&format!("repos/{repo}/pulls/{number}"), change)
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
    #[serde(default)]
    body: String,
    #[serde(default)]
    created: Option<DateTime<Utc>>,
}

/// A PR on its own, with --comments.
#[derive(Deserialize, Default)]
struct TeaPullDetail {
    /// Its author
    #[serde(default)]
    user: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    reviews: Option<Vec<TeaReview>>,
    #[serde(default)]
    comments: Option<Vec<TeaComment>>,
}

#[derive(Deserialize)]
struct TeaComment {
    author: String,
    created: DateTime<Utc>,
    #[serde(default)]
    body: String,
}

fn forgejo_reviewers(login: &str, pr: &Pr, known: &[String]) -> Result<Reviewers> {
    let number = pr.number.to_string();
    let pull: TeaPullDetail = tea(login, &["pulls", &number, "--repo", &pr.repo])?;
    Ok(forgejo_reviewers_from(pull, known))
}

/// Who it asks now, from each reviewer's last entry, and what they said last. tea can't ask
/// who can review, so the people are the ones it knows from the PRs.
fn forgejo_reviewers_from(pull: TeaPullDetail, known: &[String]) -> Reviewers {
    let reviews = pull.reviews.unwrap_or_default();
    let mut last: HashMap<&str, &str> = HashMap::new();
    let mut verdicts = HashMap::new();
    for r in by_id(&reviews) {
        last.insert(&r.reviewer, &r.state);
        let verdict = match r.state.as_str() {
            "APPROVED" => "approved",
            "REQUEST_CHANGES" => "changes requested",
            "COMMENT" => "commented",
            _ => continue,
        };
        verdicts.insert(r.reviewer.clone(), verdict);
    }
    let mut requested: Vec<String> = last
        .into_iter()
        .filter(|(_, state)| *state == "REQUEST_REVIEW")
        .map(|(name, _)| name.to_owned())
        .collect();
    requested.sort();
    let mut people: Vec<String> = known
        .iter()
        .cloned()
        .chain(reviews.iter().map(|r| r.reviewer.clone()))
        .filter(|n| *n != pull.user && !n.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    people.retain(|n| !requested.contains(n));
    Reviewers {
        requested,
        people,
        verdicts,
    }
}

fn forgejo_review_args(
    login: &str,
    repo: &str,
    number: u64,
    change: &review::Change,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "pulls",
        "edit",
        &number.to_string(),
        "--repo",
        repo,
        "--login",
        login,
    ]
    .map(String::from)
    .into();
    // Long flags only: tea gives -r to both --repo and --add-reviewers
    if !change.add.is_empty() {
        args.extend(["--add-reviewers".into(), change.add.join(",")]);
    }
    if !change.remove.is_empty() {
        args.extend(["--remove-reviewers".into(), change.remove.join(",")]);
    }
    args
}

fn forgejo_preview(login: &str, repo: &str, number: u64) -> Result<Preview> {
    let number = number.to_string();
    let pull: TeaPullDetail = tea(login, &["pulls", &number, "--repo", repo, "--comments"])?;
    Ok(forgejo_preview_from(pull))
}

fn forgejo_preview_from(pull: TeaPullDetail) -> Preview {
    let mut comments: Vec<Comment> = pull
        .comments
        .unwrap_or_default()
        .into_iter()
        .map(|c| Comment {
            author: c.author,
            when: c.created,
            body: c.body,
            verdict: None,
        })
        .collect();
    for r in pull.reviews.unwrap_or_default() {
        let verdict = match r.state.as_str() {
            "APPROVED" => "approved",
            "REQUEST_CHANGES" => "changes requested",
            "COMMENT" if !r.body.trim().is_empty() => "reviewed",
            // REQUEST_REVIEW names who was asked, not who asked
            _ => continue,
        };
        let Some(when) = r.created else { continue };
        comments.push(Comment {
            author: r.reviewer,
            when,
            body: r.body,
            verdict: Some(verdict),
        });
    }
    Preview {
        body: pull.body,
        files: None,
        checks: Vec::new(),
        comments: preview::last_comments(comments),
    }
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
            new: false,
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

#[derive(Deserialize)]
struct FjUser {
    login: String,
}

/// A PR in the search of every repo.
#[derive(Deserialize)]
struct FjIssue {
    number: u64,
    title: String,
    html_url: String,
    user: FjUser,
    updated_at: DateTime<Utc>,
    repository: FjRepo,
}

#[derive(Deserialize)]
struct FjRepo {
    full_name: String,
}

#[derive(Deserialize)]
struct FjPull {
    #[serde(default)]
    body: Option<String>,
    user: Option<FjUser>,
    /// False while it has a conflict, but also while it is a draft or Forgejo checks it
    #[serde(default)]
    mergeable: bool,
    base: FjBranch,
    head: FjBranch,
    /// Forgejo's count: each comment in the conversation and each review counts once
    #[serde(default)]
    comments: u64,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
    #[serde(default)]
    changed_files: u64,
}

#[derive(Deserialize)]
struct FjBranch {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
    /// A head in another repo than the base is from a fork
    #[serde(default)]
    repo_id: i64,
}

#[derive(Deserialize)]
struct FjReview {
    id: i64,
    /// None when it asks a team
    user: Option<FjUser>,
    team: Option<FjTeam>,
    state: String,
    #[serde(default)]
    body: String,
    submitted_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct FjTeam {
    name: String,
}

/// The combined state of a commit's checks, and each check.
#[derive(Deserialize)]
struct FjStatus {
    #[serde(default)]
    state: String,
    #[serde(default)]
    total_count: u64,
    #[serde(default)]
    statuses: Option<Vec<FjCheck>>,
}

#[derive(Deserialize)]
struct FjCheck {
    context: String,
    status: String,
}

#[derive(Deserialize)]
struct FjFile {
    filename: String,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
}

#[derive(Deserialize)]
struct FjComment {
    user: Option<FjUser>,
    #[serde(default)]
    body: String,
    created_at: DateTime<Utc>,
}

/// GETs `path` and reads the reply as JSON.
fn get_json<T: DeserializeOwned>(http: &dyn Http, path: &str) -> Result<T> {
    let reply = http.get(path)?;
    serde_json::from_str(&reply.body).with_context(|| format!("could not read the reply to {path}"))
}

/// GETs `path` and the pages after it, as the Link header gives them, until there are `most`
/// items or no more pages.
fn get_all<T: DeserializeOwned>(http: &dyn Http, path: &str, most: usize) -> Result<Vec<T>> {
    let mut items = Vec::new();
    let mut next = Some(path.to_owned());
    while let Some(path) = next
        && items.len() < most
    {
        let reply = http.get(&path)?;
        let page: Vec<T> = serde_json::from_str(&reply.body)
            .with_context(|| format!("could not read the reply to {path}"))?;
        if page.is_empty() {
            break;
        }
        items.extend(page);
        next = reply.next;
    }
    items.truncate(most);
    Ok(items)
}

/// `text` for a URL's query: letters, digits and -._~ as they are, the rest as %XX.
pub(crate) fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            b => format!("%{b:02X}"),
        })
        .collect()
}

/// The reviews as tea gives them, so the rules for tea apply to them too. A request to a team
/// is from `owner/team`, as the reviewer panel names teams.
fn fj_reviews(reviews: Vec<FjReview>, owner: &str) -> Vec<TeaReview> {
    reviews
        .into_iter()
        .filter_map(|r| {
            let reviewer = match (r.user, r.team) {
                (Some(user), _) => user.login,
                (None, Some(team)) => format!("{owner}/{}", team.name),
                (None, None) => return None,
            };
            Some(TeaReview {
                id: r.id,
                reviewer,
                state: r.state,
                body: r.body,
                created: r.submitted_at,
            })
        })
        .collect()
}

/// The open PRs that a token can see on a Forgejo or Gitea, through its API.
fn forgejo_api(name: &str, http: &dyn Http, owner: Option<&str>) -> Result<Vec<Pr>> {
    let me: FjUser = get_json(http, "/user")?;
    let mut search = "/repos/issues/search?type=pulls&state=open&limit=50".to_owned();
    // Without an owner, a public instance returns the PRs of every public repo
    if let Some(owner) = owner {
        search.push_str(&format!("&owner={}", encode(owner)));
    }
    let listed: Vec<FjIssue> = get_all(http, &search, usize::MAX)?;

    // A few calls for each PR, side by side
    let status = |issue: &FjIssue| -> Result<Pr> {
        let repo = &issue.repository.full_name;
        let pull_path = format!("/repos/{repo}/pulls/{}", issue.number);
        let pull: FjPull = get_json(http, &pull_path)?;
        let reviews: Vec<FjReview> = get_all(http, &format!("{pull_path}/reviews?limit=50"), 500)?;
        let ci: FjStatus = get_json(
            http,
            &format!("/repos/{repo}/commits/{}/status", pull.head.sha),
        )?;
        let owner = repo.split('/').next().unwrap_or_default();
        let reviews = fj_reviews(reviews, owner);
        let draft = forgejo_draft(&issue.title);
        Ok(Pr {
            host: name.into(),
            number: issue.number,
            repo: repo.clone(),
            title: issue.title.clone(),
            url: issue.html_url.clone(),
            checks_url: forgejo_checks_url(&issue.html_url),
            author: issue.user.login.clone(),
            status: forgejo_status(&reviews),
            ci: if ci.total_count == 0 {
                None
            } else {
                forgejo_ci_state(&ci.state)
            },
            updated: issue.updated_at,
            mine: issue.user.login == me.login,
            requested: forgejo_requested(&reviews, &me.login),
            new: false,
            draft,
            conflict: !pull.mergeable && !draft,
            comments: pull.comments,
            base: pull.base.name,
            head: if pull.head.repo_id == pull.base.repo_id {
                pull.head.name
            } else {
                String::new()
            },
            parent: None,
            tree: String::new(),
        })
    };
    par_map(&listed, 8, status).into_iter().collect()
}

/// The PR, its reviews and its comments, as tea gives them with --comments.
fn forgejo_api_pull(http: &dyn Http, repo: &str, number: u64) -> Result<(FjPull, TeaPullDetail)> {
    let pull_path = format!("/repos/{repo}/pulls/{number}");
    let (pull, reviews, comments) = thread::scope(|s| {
        let reviews =
            s.spawn(|| get_all::<FjReview>(http, &format!("{pull_path}/reviews?limit=50"), 500));
        let comments = s.spawn(|| {
            get_json::<Vec<FjComment>>(http, &format!("/repos/{repo}/issues/{number}/comments"))
        });
        (
            get_json::<FjPull>(http, &pull_path),
            reviews.join().unwrap(),
            comments.join().unwrap(),
        )
    });
    let mut pull = pull?;
    let owner = repo.split('/').next().unwrap_or_default();
    let detail = TeaPullDetail {
        user: pull
            .user
            .as_ref()
            .map(|u| u.login.clone())
            .unwrap_or_default(),
        body: pull.body.take().unwrap_or_default(),
        reviews: Some(fj_reviews(reviews?, owner)),
        comments: Some(
            comments?
                .into_iter()
                .map(|c| TeaComment {
                    author: c.user.map_or_else(|| "ghost".into(), |u| u.login),
                    created: c.created_at,
                    body: c.body,
                })
                .collect(),
        ),
    };
    Ok((pull, detail))
}

fn forgejo_api_preview(http: &dyn Http, repo: &str, number: u64) -> Result<Preview> {
    let (pull, detail) = forgejo_api_pull(http, repo, number)?;
    let (files, ci) = thread::scope(|s| {
        let files = s.spawn(|| {
            get_all::<FjFile>(
                http,
                &format!("/repos/{repo}/pulls/{number}/files?limit=50"),
                100,
            )
        });
        let ci = get_json::<FjStatus>(
            http,
            &format!("/repos/{repo}/commits/{}/status", pull.head.sha),
        );
        (files.join().unwrap(), ci)
    });
    let mut preview = forgejo_preview_from(detail);
    preview.files = Some(Files {
        additions: pull.additions,
        deletions: pull.deletions,
        count: pull.changed_files,
        changed: files?
            .into_iter()
            .map(|f| FileChange {
                path: f.filename,
                additions: f.additions,
                deletions: f.deletions,
            })
            .collect(),
    });
    preview.checks = ci?
        .statuses
        .unwrap_or_default()
        .into_iter()
        .map(|c| Check {
            name: c.context,
            state: match c.status.as_str() {
                // As on GitHub, a skipped check doesn't need a look
                "skipped" => Ci::Passed,
                status => forgejo_ci_state(status).unwrap_or(Ci::Pending),
            },
        })
        .collect();
    Ok(preview)
}

/// Who reviews the PR, and who can: the people the host suggests, then the repo's teams.
fn forgejo_api_reviewers(http: &dyn Http, pr: &Pr) -> Result<Reviewers> {
    let repo = &pr.repo;
    let ((_, detail), can, teams) = thread::scope(|s| {
        let can = s.spawn(|| get_json::<Vec<FjUser>>(http, &format!("/repos/{repo}/reviewers")));
        let teams = s.spawn(|| get_json::<Vec<FjTeam>>(http, &format!("/repos/{repo}/teams")));
        let pull = forgejo_api_pull(http, repo, pr.number);
        Ok::<_, anyhow::Error>((pull?, can.join().unwrap()?, teams.join().unwrap()))
    })?;
    let author = detail.user.clone();
    let mut reviewers = forgejo_reviewers_from(detail, &[]);
    // A repo of a user, not an org, has no teams
    let teams = teams.unwrap_or_default();
    let names = can.into_iter().map(|u| u.login).chain(
        teams
            .into_iter()
            .map(|t| format!("{}/{}", pr.owner(), t.name)),
    );
    let mut people = Vec::new();
    for name in names {
        if name != author && !people.contains(&name) {
            people.push(name);
        }
    }
    reviewers.people = people;
    Ok(reviewers)
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
    use crate::http::Reply;

    /// A PR on GitHub in acme/r, as the tests of the preview need one.
    fn gh_pr() -> Pr {
        crate::pr::tests::pr("GitHub", "r", 1, "me", Status::Needs)
    }

    fn review(id: i64, reviewer: &str, state: &str) -> TeaReview {
        TeaReview {
            id,
            reviewer: reviewer.into(),
            state: state.into(),
            body: String::new(),
            created: None,
        }
    }

    #[test]
    fn github_preview_reads_the_checks_files_and_comments() {
        let out = r#"{"data":{"resource":{
          "body":"Adds limits","additions":12,"deletions":3,"changedFiles":2,
          "files":{"nodes":[{"path":"src/a.rs","additions":10,"deletions":3},
                            {"path":"b.md","additions":2,"deletions":0}]},
          "comments":{"nodes":[
            {"author":{"login":"bao"},"body":"Why 429?","createdAt":"2026-11-04T09:00:00Z"},
            {"author":null,"body":"old","createdAt":"2026-11-04T08:00:00Z"}]},
          "reviews":{"nodes":[
            {"author":{"login":"amy"},"state":"APPROVED","body":"","submittedAt":"2026-11-04T09:30:00Z"},
            {"author":{"login":"chidi"},"state":"COMMENTED","body":"","submittedAt":"2026-11-04T09:10:00Z"},
            {"author":{"login":"me"},"state":"PENDING","body":"draft","submittedAt":null}]},
          "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[
            {"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"FAILURE"},
            {"__typename":"CheckRun","name":"lint","status":"COMPLETED","conclusion":"SUCCESS"},
            {"__typename":"CheckRun","name":"e2e","status":"IN_PROGRESS","conclusion":null},
            {"__typename":"StatusContext","context":"ci/legacy","state":"ERROR"}]}}}}]}
        }}}"#;
        let p = parse_github_preview(out).unwrap();
        assert_eq!(p.body, "Adds limits");
        let checks: Vec<(&str, Ci)> = p
            .checks
            .iter()
            .map(|c| (c.name.as_str(), c.state))
            .collect();
        assert_eq!(
            checks,
            [
                ("test", Ci::Failing),
                ("lint", Ci::Passed),
                ("e2e", Ci::Pending),
                ("ci/legacy", Ci::Failing)
            ]
        );
        let files = p.files.unwrap();
        assert_eq!((files.additions, files.deletions, files.count), (12, 3, 2));
        assert_eq!(files.changed[1].path, "b.md");
        // Oldest first; a review with only inline comments, or a pending one, has nothing to show
        let comments: Vec<(&str, Option<&str>)> = p
            .comments
            .iter()
            .map(|c| (c.author.as_str(), c.verdict))
            .collect();
        assert_eq!(
            comments,
            [("ghost", None), ("bao", None), ("amy", Some("approved"))]
        );
    }

    #[test]
    fn github_reviewers_lists_requests_verdicts_and_people() {
        let out = r#"{"data":{"resource":{
          "author":{"login":"amy"},
          "reviewRequests":{"nodes":[
            {"requestedReviewer":{"__typename":"User","login":"bao"}},
            {"requestedReviewer":{"__typename":"Team","slug":"api","organization":{"login":"acme"}}},
            {"requestedReviewer":{"__typename":"Mannequin"}},
            {"requestedReviewer":null}]},
          "latestReviews":{"nodes":[
            {"author":{"login":"chidi"},"state":"CHANGES_REQUESTED"},
            {"author":{"login":"dana"},"state":"PENDING"}]},
          "suggestedReviewers":[{"reviewer":{"login":"chidi"}}],
          "repository":{"assignableUsers":{"nodes":[
            {"login":"amy"},{"login":"bao"},{"login":"chidi"},{"login":"erin"}]}}
        }}}"#;
        let teams = github_teams(r#"[{"slug":"api"}][{"slug":"web"}]"#, "acme").unwrap();
        assert_eq!(teams, ["acme/api", "acme/web"]);
        let r = parse_github_reviewers(out, teams).unwrap();
        assert_eq!(r.requested, ["bao", "acme/api"]);
        // The suggestions first, then the teams, and not the author
        assert_eq!(r.people, ["chidi", "acme/api", "acme/web", "bao", "erin"]);
        assert_eq!(
            r.verdicts,
            HashMap::from([("chidi".into(), "changes requested")])
        );
    }

    #[test]
    fn github_review_calls_post_and_delete_with_teams_as_slugs() {
        let change = review::Change {
            add: vec!["bao".into(), "acme/api".into()],
            remove: vec!["acme/web".into()],
        };
        let calls = github_review_calls("acme/api", 418, &change);
        let endpoint = "repos/acme/api/pulls/418/requested_reviewers";
        let args: Vec<Vec<String>> = calls.iter().map(ReviewCall::gh_args).collect();
        assert_eq!(
            args,
            [
                vec![
                    "-X",
                    "POST",
                    endpoint,
                    "-f",
                    "reviewers[]=bao",
                    "-f",
                    "team_reviewers[]=api"
                ],
                vec![
                    "-X",
                    "DELETE",
                    endpoint,
                    "-f",
                    "reviewers[]",
                    "-f",
                    "team_reviewers[]=web"
                ],
            ]
        );
        assert!(github_review_calls("acme/api", 418, &review::Change::default()).is_empty());
        // The API takes the same as a JSON body
        assert_eq!(
            calls[0].json(),
            json!({ "reviewers": ["bao"], "team_reviewers": ["api"] })
        );
        assert_eq!(
            calls[1].json(),
            json!({ "reviewers": [], "team_reviewers": ["web"] })
        );
    }

    #[test]
    fn forgejo_reviewers_come_from_each_reviewers_last_entry() {
        let pull: TeaPullDetail = serde_json::from_str(
            r#"{"user":"dana","reviews":[
              {"id":1,"reviewer":"erin","state":"REQUEST_REVIEW"},
              {"id":2,"reviewer":"erin","state":"APPROVED"},
              {"id":3,"reviewer":"bao","state":"COMMENT"},
              {"id":4,"reviewer":"bao","state":"REQUEST_REVIEW"}]}"#,
        )
        .unwrap();
        let known = ["dana".to_owned(), "sam".to_owned(), "bao".to_owned()];
        let r = forgejo_reviewers_from(pull, &known);
        assert_eq!(r.requested, ["bao"]);
        assert_eq!(r.people, ["erin", "sam"]);
        assert_eq!(
            r.verdicts,
            HashMap::from([("erin".into(), "approved"), ("bao".into(), "commented")])
        );
        let change = review::Change {
            add: vec!["sam".into(), "erin".into()],
            remove: vec!["bao".into()],
        };
        assert_eq!(
            forgejo_review_args("work", "acme/docs", 23, &change),
            [
                "pulls",
                "edit",
                "23",
                "--repo",
                "acme/docs",
                "--login",
                "work",
                "--add-reviewers",
                "sam,erin",
                "--remove-reviewers",
                "bao"
            ]
        );
    }

    #[test]
    fn github_preview_without_the_pr_says_so() {
        let error = parse_github_preview(r#"{"data":{"resource":null}}"#).unwrap_err();
        assert!(format!("{error:#}").contains("no such PR"));
    }

    #[test]
    fn forgejo_preview_merges_comments_and_reviews() {
        let pull: TeaPullDetail = serde_json::from_str(
            r#"{"body":"Fix links","reviews":[
              {"id":1,"reviewer":"erin","state":"REQUEST_REVIEW","body":"","created":"2026-11-04T08:00:00Z"},
              {"id":2,"reviewer":"dana","state":"APPROVED","body":"","created":"2026-11-04T10:00:00Z"},
              {"id":3,"reviewer":"bao","state":"COMMENT","body":"Nit","created":"2026-11-04T09:30:00Z"}],
              "comments":[{"id":5,"author":"erin","created":"2026-11-04T09:00:00Z","body":"Thanks"}]}"#,
        )
        .unwrap();
        let p = forgejo_preview_from(pull);
        assert_eq!(p.body, "Fix links");
        assert!(p.files.is_none() && p.checks.is_empty());
        let comments: Vec<(&str, Option<&str>)> = p
            .comments
            .iter()
            .map(|c| (c.author.as_str(), c.verdict))
            .collect();
        assert_eq!(
            comments,
            [
                ("erin", None),
                ("bao", Some("reviewed")),
                ("dana", Some("approved"))
            ]
        );
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

    /// Canned replies from an API, and the calls made to it, in order.
    struct Canned {
        answer: fn(&str, &str, Option<&Value>) -> Option<Reply>,
        calls: Mutex<Vec<String>>,
    }

    impl Canned {
        fn new(answer: fn(&str, &str, Option<&Value>) -> Option<Reply>) -> Self {
            Canned {
                answer,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Http for Canned {
        fn get(&self, path: &str) -> Result<Reply> {
            self.calls.lock().unwrap().push(format!("GET {path}"));
            (self.answer)("GET", path, None).with_context(|| format!("no reply to GET {path}"))
        }

        fn send(&self, method: &str, path: &str, body: &Value) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{method} {path} {body}"));
            (self.answer)(method, path, Some(body))
                .map(|r| r.body)
                .with_context(|| format!("no reply to {method} {path}"))
        }
    }

    fn body(text: &str) -> Option<Reply> {
        Some(Reply {
            body: text.into(),
            next: None,
        })
    }

    /// A PR in GitHub's search, with only what parse_github needs.
    fn gh_node(number: u64) -> String {
        format!(
            r#"{{"number":{number},"title":"t","url":"u{number}","reviewDecision":null,"updatedAt":"2026-11-04T09:07:15Z","isDraft":false,"mergeable":"MERGEABLE","baseRefName":"main","headRefName":"b{number}","headRefOid":"sha{number}","author":{{"login":"me"}},"repository":{{"nameWithOwner":"acme/r"}},"headRepository":{{"nameWithOwner":"acme/r"}},"latestOpinionatedReviews":{{"nodes":[]}},"commits":{{"nodes":[]}},"comments":{{"totalCount":0}},"reviewThreads":{{"nodes":[]}}}}"#
        )
    }

    #[test]
    fn github_through_the_api_reads_every_page_and_the_requests() {
        let canned = Canned::new(|method, path, sent| {
            assert_eq!((method, path), ("POST", "/graphql"));
            let sent = sent?;
            let variables = &sent["variables"];
            let search = |more: bool, cursor: &str, nodes: &str| {
                format!(
                    r#"{{"data":{{"viewer":{{"login":"me"}},"search":{{"pageInfo":{{"hasNextPage":{more},"endCursor":{cursor}}},"nodes":[{nodes}]}}}}}}"#
                )
            };
            if variables["q"] == "is:pr is:open org:acme review-requested:@me" {
                return body(&search(false, "null", r#"{"url":"u2"}"#));
            }
            assert_eq!(variables["q"], "is:pr is:open org:acme");
            match variables.get("endCursor").and_then(Value::as_str) {
                None => body(&search(true, r#""c1""#, &gh_node(1))),
                Some("c1") => body(&search(false, "null", &gh_node(2))),
                Some(_) => None,
            }
        });
        let prs = github("GitHub", "org:acme", &Gh::api(&canned, None)).unwrap();
        let got: Vec<_> = prs.iter().map(|p| (p.number, p.requested)).collect();
        assert_eq!(got, [(1, false), (2, true)]);
        assert_eq!(canned.calls().len(), 3);
    }

    #[test]
    fn github_through_the_api_fails_with_graphqls_errors() {
        let canned = Canned::new(|_, _, _| {
            body(
                r#"{"data":null,"errors":[{"message":"Resource not accessible by personal access token"}]}"#,
            )
        });
        let error = github_preview(&gh_pr(), &Gh::api(&canned, None)).unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "GitHub: Resource not accessible by personal access token"
        );
    }

    #[test]
    fn graphqls_errors_show_each_message_once_with_a_count_and_a_place() {
        let canned = Canned::new(|_, _, _| {
            let refused = r#"{"type":"FORBIDDEN","path":["resource","files"],"message":"Resource not accessible by personal access token"}"#;
            body(&format!(
                r#"{{"data":null,"errors":[{refused},{refused},{refused},{{"message":"Something else"}}]}}"#
            ))
        });
        let error = github_preview(&gh_pr(), &Gh::api(&canned, None)).unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "GitHub: Resource not accessible by personal access token (3 times, first at \
             resource.files); Something else"
        );
    }

    /// A reply to the search for the PRs, as GitHub gives it to a fine-grained token: the
    /// first PR without its commit, the second PR not there, and the third PR without its CI.
    fn search_without_ci(_: &str, path: &str, sent: Option<&Value>) -> Option<Reply> {
        let refused = |path: &str| {
            format!(
                r#"{{"type":"FORBIDDEN","path":{path},"message":"Resource not accessible by personal access token"}}"#
            )
        };
        match path {
            "/graphql" if sent?["variables"]["q"] == "is:pr is:open org:acme" => body(&format!(
                r#"{{"data":{{"viewer":{{"login":"me"}},"search":{{"pageInfo":{{"hasNextPage":false,"endCursor":null}},"nodes":[{},null,{}]}}}},"errors":[{},{},{}]}}"#,
                gh_node(1).replace(r#""commits":{"nodes":[]}"#, r#""commits":{"nodes":[null]}"#),
                gh_node(3).replace(
                    r#""commits":{"nodes":[]}"#,
                    r#""commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}"#
                ),
                refused(r#"["search","nodes",0,"commits","nodes",0]"#),
                refused(r#"["search","nodes",1]"#),
                refused(r#"["search","nodes",2,"commits","nodes",0,"commit","statusCheckRollup"]"#),
            )),
            "/graphql" => body(
                r#"{"data":{"search":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}"#,
            ),
            "/repos/acme/r/commits/sha1/status" => {
                body(r#"{"state":"success","statuses":[{"context":"lint","state":"success"}]}"#)
            }
            "/repos/acme/r/actions/runs?head_sha=sha1&per_page=100" => body(
                r#"{"workflow_runs":[{"name":"test","status":"completed","conclusion":"failure"}]}"#,
            ),
            "/repos/acme/r/commits/sha3/status" => {
                body(r#"{"state":"pending","statuses":[{"context":"deploy","state":"pending"}]}"#)
            }
            // Without the Actions permission, GitHub doesn't show the runs
            _ => None,
        }
    }

    #[test]
    fn a_fine_grained_token_gets_the_prs_and_reads_their_ci_over_rest() {
        let canned = Canned::new(search_without_ci);
        let via = Gh::api(&canned, None);
        let prs = github("Fine-grained", "org:acme", &via).unwrap();
        let got: Vec<_> = prs.iter().map(|p| (p.number, p.ci)).collect();
        assert_eq!(got, [(1, Some(Ci::Failing)), (3, Some(Ci::Pending))]);

        // The next load keeps the checks that are done, and reads again those that are not
        let before = canned.calls().len();
        let prs = github("Fine-grained", "org:acme", &via).unwrap();
        let got: Vec<_> = prs.iter().map(|p| (p.number, p.ci)).collect();
        assert_eq!(got, [(1, Some(Ci::Failing)), (3, Some(Ci::Pending))]);
        let again = canned.calls().split_off(before);
        assert!(!again.iter().any(|c| c.contains("sha1")), "{again:?}");
        assert!(again.iter().any(|c| c.contains("sha3")), "{again:?}");
    }

    #[test]
    fn a_pr_that_saml_sso_keeps_out_fails() {
        let canned = Canned::new(|_, _, _| {
            body(&format!(
                r#"{{"data":{{"viewer":{{"login":"me"}},"search":{{"pageInfo":{{"hasNextPage":false,"endCursor":null}},"nodes":[{},null]}}}},"errors":[{{"type":"FORBIDDEN","path":["search","nodes",1],"extensions":{{"saml_failure":true}},"message":"Resource protected by organization SAML enforcement"}}]}}"#,
                gh_node(1)
            ))
        });
        let error = github("GitHub", "org:acme", &Gh::api(&canned, None)).unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "GitHub: Resource protected by organization SAML enforcement (at search.nodes.1)"
        );
    }

    #[test]
    fn a_refusal_of_another_field_still_fails() {
        let canned = Canned::new(|_, _, _| {
            body(&format!(
                r#"{{"data":{{"viewer":{{"login":"me"}},"search":{{"pageInfo":{{"hasNextPage":false,"endCursor":null}},"nodes":[{}]}}}},"errors":[{{"type":"FORBIDDEN","path":["search","nodes",0,"author"],"message":"Resource not accessible by personal access token"}}]}}"#,
                gh_node(1)
            ))
        });
        let error = github("GitHub", "org:acme", &Gh::api(&canned, None)).unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "GitHub: Resource not accessible by personal access token (at search.nodes.0.author)"
        );
    }

    #[test]
    fn the_preview_of_a_fine_grained_token_reads_the_checks_over_rest() {
        let canned = Canned::new(|_, path, _| match path {
            "/graphql" => body(
                r#"{"data":{"resource":{"body":"","additions":0,"deletions":0,"changedFiles":0,
                    "headRefOid":"sha1","files":{"nodes":[]},"comments":{"nodes":[]},
                    "reviews":{"nodes":[]},"commits":{"nodes":[null]}}},
                  "errors":[{"type":"FORBIDDEN","message":"Resource not accessible by personal access token",
                    "path":["resource","commits","nodes",0]}]}"#,
            ),
            "/repos/acme/r/commits/sha1/status" => {
                body(r#"{"statuses":[{"context":"lint","state":"error"}]}"#)
            }
            "/repos/acme/r/actions/runs?head_sha=sha1&per_page=100" => body(
                r#"{"workflow_runs":[
                    {"name":"test","event":"pull_request","status":"completed","conclusion":"skipped"},
                    {"name":"e2e","event":"pull_request","status":"queued","conclusion":null},
                    {"name":"e2e","event":"pull_request","status":"completed","conclusion":"cancelled"}]}"#,
            ),
            _ => None,
        });
        let preview = github_preview(&gh_pr(), &Gh::api(&canned, None)).unwrap();
        let checks: Vec<(&str, Ci)> = preview
            .checks
            .iter()
            .map(|c| (c.name.as_str(), c.state))
            .collect();
        assert_eq!(
            checks,
            [
                ("lint", Ci::Failing),
                ("test", Ci::Passed),
                ("e2e", Ci::Pending)
            ]
        );
    }

    #[test]
    fn github_enterprise_has_graphql_on_its_own_host() {
        let canned = Canned::new(|_, _, _| body(r#"{"data":{"resource":null}}"#));
        let _ = github_preview(&gh_pr(), &Gh::api(&canned, Some("ghe.acme.dev")));
        assert!(canned.calls()[0].starts_with("POST https://ghe.acme.dev/api/graphql "));
    }

    #[test]
    fn github_rest_pages_follow_the_link_header() {
        let canned = Canned::new(|_, path, _| match path {
            "/repos/acme/r/teams?per_page=100" => Some(Reply {
                body: r#"[{"slug":"web"}]"#.into(),
                next: Some("https://api.github.com/repositories/1/teams?page=2".into()),
            }),
            "https://api.github.com/repositories/1/teams?page=2" => body(r#"[{"slug":"api"}]"#),
            _ => None,
        });
        let out = Gh::api(&canned, None).pages("repos/acme/r/teams").unwrap();
        assert_eq!(
            github_teams(&out, "acme").unwrap(),
            ["acme/web", "acme/api"]
        );
    }

    #[test]
    fn review_requests_through_the_api_send_json() {
        let canned = Canned::new(|_, _, _| body("{}"));
        let change = review::Change {
            add: vec!["bao".into()],
            remove: vec!["acme/web".into()],
        };
        let gh = Gh::api(&canned, None);
        for call in github_review_calls("acme/r", 7, &change) {
            gh.review(&call).unwrap();
        }
        let endpoint = "/repos/acme/r/pulls/7/requested_reviewers";
        assert_eq!(
            canned.calls(),
            [
                format!(r#"POST {endpoint} {{"reviewers":["bao"],"team_reviewers":[]}}"#),
                format!(r#"DELETE {endpoint} {{"reviewers":[],"team_reviewers":["web"]}}"#),
            ]
        );
    }

    /// A Forgejo with two PRs in acme/app: 1 by bao, which asks me for a review, and 2 by me,
    /// a draft from a fork, on the search's second page.
    fn forgejo_answer(method: &str, path: &str, _: Option<&Value>) -> Option<Reply> {
        assert_eq!(method, "GET");
        let issue = |number: u64, title: &str, author: &str| {
            format!(
                r#"{{"number":{number},"title":"{title}","html_url":"https://git.example.com/acme/app/pulls/{number}","user":{{"login":"{author}"}},"updated_at":"2026-11-04T09:07:15Z","repository":{{"full_name":"acme/app","owner":"acme","name":"app"}},"comments":1}}"#
            )
        };
        let search = "/repos/issues/search?type=pulls&state=open&limit=50&owner=acme";
        match path {
            "/user" => body(r#"{"login":"me"}"#),
            _ if path == search => Some(Reply {
                body: format!("[{}]", issue(1, "Add a cache", "bao")),
                next: Some(format!("{search}&page=2")),
            }),
            _ if path == format!("{search}&page=2") => {
                body(&format!("[{}]", issue(2, "WIP: try a thing", "me")))
            }
            "/repos/acme/app/pulls/1" => body(
                r#"{"body":"It caches.","user":{"login":"bao"},"mergeable":false,"comments":3,"additions":10,"deletions":2,"changed_files":2,
                    "base":{"ref":"main","sha":"b0","repo_id":1},"head":{"ref":"cache","sha":"s1","repo_id":1}}"#,
            ),
            "/repos/acme/app/pulls/2" => body(
                r#"{"mergeable":false,"base":{"ref":"main","sha":"b0","repo_id":1},"head":{"ref":"main","sha":"s2","repo_id":9}}"#,
            ),
            "/repos/acme/app/pulls/1/reviews?limit=50" => body(
                r#"[{"id":1,"user":{"login":"me"},"state":"REQUEST_REVIEW","body":"","submitted_at":"2026-11-04T09:00:00Z"},
                    {"id":2,"user":{"login":"chidi"},"state":"APPROVED","body":"Good","submitted_at":"2026-11-04T10:00:00Z"},
                    {"id":3,"user":null,"team":{"name":"web"},"state":"REQUEST_REVIEW","body":""}]"#,
            ),
            "/repos/acme/app/pulls/2/reviews?limit=50" => body("[]"),
            "/repos/acme/app/commits/s1/status" => body(
                r#"{"state":"warning","total_count":2,"statuses":[{"context":"build","status":"warning"},{"context":"lint","status":"skipped"}]}"#,
            ),
            "/repos/acme/app/commits/s2/status" => {
                body(r#"{"state":"","total_count":0,"statuses":null}"#)
            }
            "/repos/acme/app/issues/1/comments" => body(
                r#"[{"user":{"login":"dana"},"body":"Why?","created_at":"2026-11-04T09:30:00Z"}]"#,
            ),
            "/repos/acme/app/pulls/1/files?limit=50" => body(
                r#"[{"filename":"src/cache.rs","additions":9,"deletions":0},{"filename":"README.md","additions":1,"deletions":2}]"#,
            ),
            "/repos/acme/app/reviewers" => {
                body(r#"[{"login":"bao"},{"login":"chidi"},{"login":"dana"}]"#)
            }
            "/repos/acme/app/teams" => body(r#"[{"name":"web"},{"name":"ops"}]"#),
            _ => None,
        }
    }

    #[test]
    fn forgejo_through_the_api_reads_the_prs_with_their_reviews_and_ci() {
        let canned = Canned::new(forgejo_answer);
        let prs = forgejo_api("Forgejo", &canned, Some("acme")).unwrap();
        let got: Vec<_> = prs
            .iter()
            .map(|p| (p.number, p.status, p.ci, p.mine, p.requested))
            .collect();
        assert_eq!(
            got,
            [
                (1, Status::Approved, Some(Ci::Failing), false, true),
                (2, Status::Needs, None, true, false),
            ]
        );
        let notes: Vec<_> = prs
            .iter()
            .map(|p| (p.draft, p.conflict, p.comments, p.head.as_str()))
            .collect();
        // 2 is a draft, so mergeable false is no conflict, and its head is in a fork
        assert_eq!(notes, [(false, true, 3, "cache"), (true, false, 0, "")]);
        assert_eq!(
            prs[0].checks_url,
            "https://git.example.com/acme/app/actions"
        );
    }

    #[test]
    fn forgejo_preview_through_the_api_has_files_checks_and_comments() {
        let canned = Canned::new(forgejo_answer);
        let preview = forgejo_api_preview(&canned, "acme/app", 1).unwrap();
        assert_eq!(preview.body, "It caches.");
        let files = preview.files.unwrap();
        assert_eq!((files.count, files.additions, files.deletions), (2, 10, 2));
        let paths: Vec<_> = files.changed.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["src/cache.rs", "README.md"]);
        let checks: Vec<_> = preview
            .checks
            .iter()
            .map(|c| (c.name.as_str(), c.state))
            .collect();
        assert_eq!(checks, [("build", Ci::Failing), ("lint", Ci::Passed)]);
        let comments: Vec<_> = preview
            .comments
            .iter()
            .map(|c| (c.author.as_str(), c.verdict))
            .collect();
        assert_eq!(comments, [("dana", None), ("chidi", Some("approved"))]);
    }

    #[test]
    fn forgejo_reviewers_through_the_api_have_people_and_teams() {
        let canned = Canned::new(forgejo_answer);
        let pr = crate::pr::tests::pr("Forgejo", "app", 1, "bao", Status::Needs);
        let reviewers = forgejo_api_reviewers(&canned, &pr).unwrap();
        assert_eq!(reviewers.requested, ["acme/web", "me"]);
        assert_eq!(reviewers.people, ["chidi", "dana", "acme/web", "acme/ops"]);
        assert_eq!(reviewers.verdicts.get("chidi"), Some(&"approved"));
    }

    #[test]
    fn gh_and_tea_dont_get_the_variables_that_hold_tokens() {
        // cargo sets CARGO_PKG_NAME for the tests, so it stands for a token's variable
        let source = Source::github("org:acme", None, None).with_token(crate::config::Token {
            command: None,
            env: Some("CARGO_PKG_NAME".into()),
        });
        token::note_vars([&source]);
        let out = run(&["env"]).unwrap();
        assert!(!out.contains("CARGO_PKG_NAME="), "{out}");
        assert!(out.contains("PATH="));
    }
}
