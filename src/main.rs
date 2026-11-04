//! List open PRs on GitHub, Forgejo and Gitea with their review and CI status, in a TUI.
//!
//! The sources come from --github and --forgejo, else from the config file, else it searches
//! github.com for the PRs that involve you. s in the TUI adds, edits and deletes them.
//!
//! The flags set how the TUI starts, and m, a, n, w, f, d and g toggle them while it runs.
//! It reloads every SECONDS (default 60) and keeps the cursor on the same PR. Keys:
//! enter/o open the PR in the browser, y copy its URL, r reload now, q quit.
//!
//! When stdout isn't a terminal it prints one tab-separated line per PR instead:
//! host, repo, number, status, author, title, url, ci, notes, comments.

mod app;
mod config;
mod manage;
mod pr;
mod sources;

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use clap::Parser;

use crate::config::Source;
use crate::pr::{Ci, Filter, Group, Note, Status, pick};

#[derive(Parser)]
#[command(
    name = "open_prs",
    about = "List open PRs on GitHub, Forgejo and Gitea with their review and CI status.",
    after_help = "Sources: --github and --forgejo replace the ones in the config file \
                  ($XDG_CONFIG_HOME/open_prs/config.toml, else ~/.config/open_prs/config.toml). \
                  With neither, it searches github.com for involves:@me.\n\
                  Keys: enter/o open the PR in the browser, y copy its URL, r reload now, \
                  m/a/n/w/f/d/g toggle the flags, / changes the words, s add, edit and delete \
                  sources, ? every key, q quit.\n\
                  When stdout isn't a terminal, it prints one tab-separated line per PR: \
                  host, repo, number, status, author, title, url, ci (passed, failing, pending \
                  or empty), notes (for you, draft, conflict and on #N, with commas between), \
                  comments."
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
    /// Only PRs that ask for your review
    #[arg(short = 'w', long)]
    for_you: bool,
    /// Only PRs with failing CI
    #[arg(short, long)]
    failing: bool,
    /// Leave out draft PRs
    #[arg(short = 'd', long)]
    hide_drafts: bool,
    /// Only PRs with all these words in the repo, number, title, author or host
    #[arg(long, value_name = "WORDS", default_value = "")]
    text: String,
    /// Group by repo or author
    #[arg(short, long, value_enum, default_value_t = Group::Repo)]
    group: Group,
    /// Seconds between reloads
    #[arg(short, long, value_name = "SECONDS", default_value_t = 60,
          value_parser = clap::value_parser!(u64).range(1..))]
    interval: u64,
    /// Search GitHub with these qualifiers, such as "org:rust-lang" (more than once for more searches)
    #[arg(long, value_name = "QUERY")]
    github: Vec<String>,
    /// Show the PRs this tea login can see on Forgejo or Gitea (more than once for more logins)
    #[arg(long, value_name = "LOGIN")]
    forgejo: Vec<String>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let filter = Filter {
        mine: args.mine,
        status: if args.approved {
            Some(Status::Approved)
        } else if args.needs_review {
            Some(Status::Needs)
        } else {
            None
        },
        for_you: args.for_you,
        failing: args.failing,
        hide_drafts: args.hide_drafts,
        text: args.text,
    };
    let (sources, origin) = match config::load(&args.github, &args.forgejo) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("open_prs: {e:#}");
            return ExitCode::from(2);
        }
    };

    if !io::stdout().is_terminal() {
        return plain(&sources, &filter, args.group);
    }
    let options = app::Options {
        filter,
        group: args.group,
        interval: Duration::from_secs(args.interval),
    };
    match app::run(options, sources, origin) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("open_prs: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn plain(sources: &[Source], filter: &Filter, group: Group) -> ExitCode {
    let results: Vec<_> = thread::scope(|s| {
        let handles: Vec<_> = sources
            .iter()
            .map(|source| s.spawn(move || sources::fetch(source)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut prs = Vec::new();
    let mut code = ExitCode::SUCCESS;
    for (source, result) in sources.iter().zip(results) {
        match result {
            Ok(found) => prs.extend(found),
            Err(e) => {
                eprintln!("open_prs: {}: {e:#}", source.name());
                code = ExitCode::FAILURE;
            }
        }
    }

    let mut out = io::stdout().lock();
    for p in pick(&prs, filter, group) {
        let number = p.number.to_string();
        let notes: Vec<_> = p.notes().into_iter().map(Note::label).collect();
        let notes = notes.join(",");
        // New columns go last, so the ones before them stay where they were
        let comments = p.comments.to_string();
        let fields: [&str; 10] = [
            &p.host,
            &p.repo,
            &number,
            p.status.label(),
            &p.author,
            &p.title,
            &p.url,
            p.ci.map_or("", Ci::label),
            &notes,
            &comments,
        ];
        // Stop quietly when the reader goes away, as with `| head`
        if writeln!(out, "{}", fields.join("\t")).is_err() {
            break;
        }
    }
    code
}
