//! The HTTP client for the sources with a token of their own. It sends the token only to the
//! source's own host, and it follows no redirect, so the token can't go anywhere else.

use std::fmt;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use serde_json::Value;
use ureq::http::Response;
use ureq::tls::{RootCerts, TlsConfig};
use ureq::{Agent, Body};

use crate::app::clean;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// A reply to a GET.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reply {
    pub body: String,
    /// The next page, from the Link header
    pub next: Option<String>,
}

/// What the sources call, so the tests can give canned replies.
pub trait Http: Sync {
    /// GETs `path`, which is from the API's base, or a full URL on the same host.
    fn get(&self, path: &str) -> Result<Reply>;
    /// Sends `body` as JSON with `method`, such as POST, and gives the reply's body.
    fn send(&self, method: &str, path: &str, body: &Value) -> Result<String>;
}

/// The host refused the token, so the token is wrong or expired.
#[derive(Debug)]
pub struct Refused(pub String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the token for {} is wrong or expired", self.0)
    }
}

impl std::error::Error for Refused {}

pub struct Client {
    agent: Agent,
    /// The API's base, such as https://api.github.com, without a / at the end
    base: String,
    /// The scheme and host of `base`, in lower case: the only place the token goes
    origin: String,
    /// The value of the Authorization header
    auth: String,
    /// The source's name, for the errors
    source: String,
    /// GitHub's headers and errors
    github: bool,
}

/// The scheme and host of `url`, such as https://git.example.com:3000, in lower case.
fn origin(url: &str) -> Result<String> {
    let (scheme, rest) = url
        .split_once("://")
        .with_context(|| format!("{url} isn't a web address"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() || host.contains(['@', '\\', ' ']) {
        bail!("{url} has no host that open_prs can use");
    }
    Ok(format!("{scheme}://{host}").to_ascii_lowercase())
}

impl Client {
    /// A client for the API at `base` that sends `auth` as the Authorization header, such as
    /// `Bearer <token>`. `base` must be https, unless `allow_http`.
    pub fn new(
        base: &str,
        auth: String,
        source: &str,
        allow_http: bool,
        github: bool,
    ) -> Result<Self> {
        let base = base.trim_end_matches('/').to_owned();
        let origin = origin(&base)?;
        let http = allow_http && origin.starts_with("http://");
        if !origin.starts_with("https://") && !http {
            bail!("{base} isn't https, so open_prs won't send it a token");
        }
        let config = Agent::config_builder()
            .https_only(!allow_http)
            // A redirect comes back as it is, and `check` turns it into an error
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(REQUEST_TIMEOUT))
            .user_agent(concat!("open_prs/", env!("CARGO_PKG_VERSION")))
            // The system's certificates, as a company's own GitHub or Forgejo can use its own
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build();
        Ok(Client {
            agent: config.into(),
            base,
            origin,
            auth,
            source: source.to_owned(),
            github,
        })
    }

    /// The URL for `path`: from the base, or a full URL that must be on the same host.
    fn url(&self, path: &str) -> Result<String> {
        if !path.contains("://") {
            return Ok(format!("{}{path}", self.base));
        }
        if origin(path)? != self.origin {
            bail!(
                "{} sent open_prs to {path}; it sends the token only to {}",
                self.source,
                self.origin
            );
        }
        Ok(path.to_owned())
    }

    fn accept(&self) -> &'static str {
        if self.github {
            "application/vnd.github+json"
        } else {
            "application/json"
        }
    }

    /// The reply's body, or an error that says what went wrong. `write` says the request was
    /// to change something.
    fn check(
        &self,
        url: &str,
        write: bool,
        mut reply: Response<Body>,
    ) -> Result<(String, Option<String>)> {
        let status = reply.status().as_u16();
        let header = |name: &str| {
            reply
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let next = header("link").and_then(|link| next_link(&link));
        let location = header("location");
        let sso = header("x-github-sso");
        let remaining = header("x-ratelimit-remaining");
        let reset = header("x-ratelimit-reset");
        let retry = header("retry-after");
        let body = reply
            .body_mut()
            .read_to_string()
            .with_context(|| format!("could not read the reply from {url}"))?;
        if (200..300).contains(&status) {
            return Ok((body, next));
        }
        if (300..400).contains(&status) {
            let to = location.unwrap_or_else(|| "nowhere".into());
            bail!(
                "{url} sent open_prs to {}; open_prs follows no redirect, so the token stays \
                 on {}",
                clean(&to),
                self.origin
            );
        }
        if status == 401 {
            return Err(Refused(self.source.clone()).into());
        }
        let host = if self.github { "GitHub" } else { &self.source };
        if matches!(status, 403 | 429) {
            if let Some(sso) = sso {
                match sso.split_once("url=") {
                    Some((_, link)) => bail!("authorize the token for SSO: {}", clean(link.trim())),
                    None => bail!("authorize the token for SSO, in the token's settings"),
                }
            }
            if remaining.as_deref() == Some("0")
                && let Some(at) = reset
                    .and_then(|r| r.parse().ok())
                    .and_then(|r| DateTime::from_timestamp(r, 0))
            {
                let at = at.with_timezone(&Local).format("%H:%M");
                bail!("{host}'s rate limit; it resets at {at}");
            }
            if let Some(seconds) = retry {
                bail!("{host}'s rate limit; try again in {}s", clean(&seconds));
            }
        }
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("message")?.as_str().map(str::to_owned))
            .unwrap_or_else(|| body.chars().take(200).collect());
        if let Some(repo) = repo(url) {
            match status {
                403 if write => bail!(
                    "the token may not change {repo}; it needs write access: {}",
                    clean(message.trim())
                ),
                403 | 404 => bail!("the token can't see {repo}"),
                _ => {}
            }
        }
        bail!("HTTP {status} from {url}: {}", clean(message.trim()))
    }
}

impl Http for Client {
    fn get(&self, path: &str) -> Result<Reply> {
        let url = self.url(path)?;
        let reply = self
            .agent
            .get(&url)
            .header("Authorization", &self.auth)
            .header("Accept", self.accept())
            .header("X-GitHub-Api-Version", "2022-11-28")
            .call()
            .with_context(|| format!("could not reach {url}"))?;
        let (body, next) = self.check(&url, false, reply)?;
        Ok(Reply { body, next })
    }

    fn send(&self, method: &str, path: &str, body: &Value) -> Result<String> {
        let url = self.url(path)?;
        let request = match method {
            "POST" => self.agent.post(&url),
            "DELETE" => self.agent.delete(&url).force_send_body(),
            _ => bail!("open_prs doesn't send {method}"),
        };
        let reply = request
            .header("Authorization", &self.auth)
            .header("Accept", self.accept())
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send_json(body)
            .with_context(|| format!("could not reach {url}"))?;
        // A GraphQL query is a POST but it only reads; its path names no repo
        Ok(self.check(&url, true, reply)?.0)
    }
}

/// The URL with rel="next" in a Link header.
fn next_link(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        params
            .split(';')
            .any(|p| p.trim() == "rel=\"next\"")
            .then(|| {
                url.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_owned()
            })
    })
}

/// The repo, owner/name, that a REST URL is about.
fn repo(url: &str) -> Option<String> {
    let path = url.split_once("://")?.1.split(['?', '#']).next()?;
    let mut parts = path.split('/').skip_while(|p| *p != "repos").skip(1);
    let (owner, name) = (parts.next()?, parts.next()?);
    // Forgejo's search of every repo
    if owner.is_empty() || name.is_empty() || (owner == "issues" && name == "search") {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    /// A server on 127.0.0.1 that gives each connection the next reply, and then the requests
    /// it got.
    fn serve(replies: Vec<String>) -> (u16, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let mut got = Vec::new();
            for reply in replies {
                let (mut stream, _) = listener.accept().unwrap();
                got.push(read_request(&mut stream));
                stream.write_all(reply.as_bytes()).unwrap();
            }
            got
        });
        (port, handle)
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buf = [0; 4096];
        loop {
            let n = stream.read(&mut buf).unwrap();
            request.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&request).to_string();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().to_owned())
                    })
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                if body.len() >= length {
                    return text;
                }
            }
            if n == 0 {
                return String::from_utf8_lossy(&request).to_string();
            }
        }
    }

    fn reply(status: &str, headers: &[&str], body: &str) -> String {
        let mut text = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
        for header in headers {
            text.push_str(header);
            text.push_str("\r\n");
        }
        text.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        text
    }

    fn client(port: u16, github: bool) -> Client {
        let base = format!("http://127.0.0.1:{port}/api/v1");
        Client::new(&base, "token tok".into(), "Work", true, github).unwrap()
    }

    #[test]
    fn it_sends_the_token_and_reads_the_next_page() {
        let link = "Link: <http://127.0.0.1:1/api/v1/x?page=2>; rel=\"next\", <http://127.0.0.1:1/api/v1/x?page=9>; rel=\"last\"";
        let (port, server) = serve(vec![
            reply("200 OK", &[link], "[1]"),
            reply("201 Created", &[], "{}"),
        ]);
        let client = client(port, false);
        let got = client.get("/user").unwrap();
        assert_eq!(got.body, "[1]");
        assert_eq!(
            got.next.as_deref(),
            Some("http://127.0.0.1:1/api/v1/x?page=2")
        );
        let body = serde_json::json!({"reviewers": ["amy"]});
        client
            .send("POST", "/repos/acme/app/pulls/1/requested_reviewers", &body)
            .unwrap();

        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /api/v1/user HTTP/1.1\r\n"),
            "{}",
            requests[0]
        );
        let lower = requests[0].to_ascii_lowercase();
        assert!(
            lower.contains("\r\nauthorization: token tok\r\n"),
            "{lower}"
        );
        assert!(lower.contains("\r\nuser-agent: open_prs/"), "{lower}");
        assert!(
            requests[1].starts_with("POST /api/v1/repos/acme/app/pulls/1/requested_reviewers ")
        );
        let sent = requests[1].split_once("\r\n\r\n").unwrap().1;
        assert_eq!(serde_json::from_str::<Value>(sent).unwrap(), body);
    }

    #[test]
    fn a_redirect_fails_and_the_token_stays_on_its_host() {
        // The other host is localhost, which the client must not call
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        other.set_nonblocking(true).unwrap();
        let to = format!(
            "http://localhost:{}/steal",
            other.local_addr().unwrap().port()
        );
        let (port, server) = serve(vec![reply("302 Found", &[&format!("Location: {to}")], "")]);
        let error = client(port, true).get("/user").unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            format!(
                "http://127.0.0.1:{port}/api/v1/user sent open_prs to {to}; open_prs follows no \
                 redirect, so the token stays on http://127.0.0.1:{port}"
            )
        );
        server.join().unwrap();
        thread::sleep(Duration::from_millis(100));
        assert!(other.accept().is_err(), "the other host got a request");
    }

    #[test]
    fn a_next_page_on_another_host_is_not_called() {
        let (port, server) = serve(vec![]);
        let error = client(port, false)
            .get("http://localhost:1/api/v1/user?page=2")
            .unwrap_err();
        assert!(format!("{error:#}").starts_with("Work sent open_prs to http://localhost:1/"));
        server.join().unwrap();
        let same = format!("HTTP://127.0.0.1:{port}/api/v1/x");
        assert_eq!(client(port, false).url(&same).unwrap(), same);
    }

    #[test]
    fn errors_say_what_to_do() {
        let reset = 4_102_444_800; // 2100-01-01
        let at = DateTime::from_timestamp(reset, 0)
            .unwrap()
            .with_timezone(&Local)
            .format("%H:%M");
        let cases = [
            (reply("401 Unauthorized", &[], r#"{"message":"Bad credentials"}"#), "/user", "the token for Work is wrong or expired".to_owned()),
            (
                reply("403 Forbidden", &["X-GitHub-SSO: required; url=https://github.com/orgs/acme/sso?authorization_request=x"], "{}"),
                "/repos/acme/app/teams",
                "authorize the token for SSO: https://github.com/orgs/acme/sso?authorization_request=x".to_owned(),
            ),
            (reply("404 Not Found", &[], r#"{"message":"Not Found"}"#), "/repos/acme/app/pulls/1", "the token can't see acme/app".to_owned()),
            (
                reply("403 Forbidden", &["x-ratelimit-remaining: 0", &format!("x-ratelimit-reset: {reset}")], "{}"),
                "/graphql",
                format!("GitHub's rate limit; it resets at {at}"),
            ),
            (reply("429 Too Many Requests", &["Retry-After: 30"], "{}"), "/graphql", "GitHub's rate limit; try again in 30s".to_owned()),
            (reply("500 Internal Server Error", &[], "oops"), "/repos/issues/search", String::new()),
        ];
        for (answer, path, want) in cases {
            let (port, server) = serve(vec![answer]);
            let error = client(port, true).get(path).unwrap_err();
            let want = if want.is_empty() {
                format!("HTTP 500 from http://127.0.0.1:{port}/api/v1/repos/issues/search: oops")
            } else {
                want
            };
            assert_eq!(format!("{error:#}"), want);
            server.join().unwrap();
        }
    }

    #[test]
    fn a_403_on_a_change_says_the_token_needs_write_access() {
        let message = r#"{"message":"Resource not accessible by personal access token"}"#;
        let (port, server) = serve(vec![reply("403 Forbidden", &[], message)]);
        let body = serde_json::json!({"reviewers": ["amy"]});
        let path = "/repos/acme/app/pulls/1/requested_reviewers";
        let error = client(port, true).send("POST", path, &body).unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "the token may not change acme/app; it needs write access: Resource not accessible \
             by personal access token"
        );
        server.join().unwrap();
    }

    #[test]
    fn a_401_is_refused_so_the_token_is_read_again() {
        let (port, server) = serve(vec![reply("401 Unauthorized", &[], "{}")]);
        let error = client(port, false).get("/user").unwrap_err();
        assert!(error.downcast_ref::<Refused>().is_some());
        server.join().unwrap();
    }

    #[test]
    fn only_https_gets_a_token_unless_http_is_allowed() {
        let new = |base: &str, allow_http| Client::new(base, "x".into(), "Work", allow_http, false);
        assert!(new("https://git.example.com", false).is_ok());
        let error = new("http://git.example.com", false).err().unwrap();
        assert_eq!(
            format!("{error:#}"),
            "http://git.example.com isn't https, so open_prs won't send it a token"
        );
        assert!(new("http://git.example.com", true).is_ok());
        assert!(new("file:///etc", true).is_err());
        assert!(new("https://me@git.example.com", false).is_err());
    }

    #[test]
    fn repo_and_next_link_read_their_parts() {
        assert_eq!(
            repo("https://h/api/v3/repos/acme/app/pulls/1").as_deref(),
            Some("acme/app")
        );
        assert_eq!(
            repo("https://h/api/v1/repos/issues/search?type=pulls"),
            None
        );
        assert_eq!(repo("https://h/graphql"), None);
        assert_eq!(next_link(r#"<https://h/x?page=3>; rel="last""#), None);
    }
}
