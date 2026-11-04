//! List open PRs on GitHub and Forgejo with their review status, in a TUI.
//!
//! The flags set how the TUI starts, and m, a, n and g toggle them while it runs. It
//! reloads every SECONDS (default 60) and keeps the cursor on the same PR. Keys:
//! enter/o open the PR in the browser, y copy its URL, r reload now, q quit.
//!
//! When stdout isn't a terminal it prints one tab-separated line per PR instead:
//! host, repo, number, status, author, title, url.

mod app;
mod pr;
mod sources;

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use clap::Parser;

use crate::pr::{Group, HOSTS, Status, pick};

#[derive(Parser)]
#[command(
    name = "open_prs",
    about = "List open PRs on GitHub and Forgejo with their review status.",
    after_help = "Keys: enter/o open the PR in the browser, y copy its URL, r reload now, \
                  m/a/n/g toggle the flags, q quit.\n\
                  When stdout isn't a terminal, it prints one tab-separated line per PR: \
                  host, repo, number, status, author, title, url."
)]
struct Args {
    /// Only PRs you opened
    #[arg(short, long)]
    mine: bool,
    /// Only approved PRs
    #[arg(short, long, conflicts_with = "needs_review")]
    approved: bool,
    /// Only PRs that need review
    #[arg(short, long)]
    needs_review: bool,
    /// Group by repo or author
    #[arg(short, long, value_enum, default_value_t = Group::Repo)]
    group: Group,
    /// Seconds between reloads
    #[arg(short, long, value_name = "SECONDS", default_value_t = 60,
          value_parser = clap::value_parser!(u64).range(1..))]
    interval: u64,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let status = if args.approved {
        Some(Status::Approved)
    } else if args.needs_review {
        Some(Status::Needs)
    } else {
        None
    };

    if !io::stdout().is_terminal() {
        return plain(args.mine, status, args.group);
    }
    match app::run(
        args.mine,
        status,
        args.group,
        Duration::from_secs(args.interval),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("open_prs: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn plain(mine: bool, status: Option<Status>, group: Group) -> ExitCode {
    let results: Vec<_> = thread::scope(|s| {
        let handles = HOSTS.map(|host| (host, s.spawn(move || sources::fetch(host))));
        handles
            .map(|(host, h)| (host, h.join().unwrap()))
            .into_iter()
            .collect()
    });

    let mut prs = Vec::new();
    let mut code = ExitCode::SUCCESS;
    for (host, result) in results {
        match result {
            Ok(found) => prs.extend(found),
            Err(e) => {
                eprintln!("open_prs: {}: {e:#}", host.name());
                code = ExitCode::FAILURE;
            }
        }
    }

    let mut out = io::stdout().lock();
    for p in pick(&prs, mine, status, group) {
        let number = p.number.to_string();
        let fields = [
            p.host.name(),
            &p.repo,
            &number,
            p.status.label(),
            &p.author,
            &p.title,
            &p.url,
        ];
        // Stop quietly when the reader goes away, as with `| head`
        if writeln!(out, "{}", fields.join("\t")).is_err() {
            break;
        }
    }
    code
}
