# open_prs

[![CI](https://github.com/carlos-alberto/open-prs/actions/workflows/ci.yml/badge.svg)](https://github.com/carlos-alberto/open-prs/actions/workflows/ci.yml)

A TUI that lists the open PRs on GitHub, Forgejo and Gitea, with their review status, CI
status and comments, and shows which PRs are stacked on which.

![open_prs with 10 PRs from GitHub and Forgejo, grouped by repo](docs/table.svg)

- One list for many GitHub searches and Forgejo or Gitea logins
- Review status, CI, review requests for you, drafts and merge conflicts
- PRs that changed since you last saw them are bold, and marked `new`
- `p` previews a PR: its description, the checks that didn't pass, the files and the last
  comments
- `v` shows who reviews a PR. When you turn writes on, it asks people or teams to review
  it, or stops asking them
- Desktop notifications, if you turn them on with `b`: when a PR asks for your review, and
  when one of yours gets approved, gets changes requested or starts to fail CI
- Stacked PRs show as a tree under the PR they are on
- Filters for your PRs, approved PRs, PRs that need review, review requests, new PRs,
  failing CI and drafts, and a text filter
- `enter` opens the PR, `c` opens its CI runs and `y` copies its URL
- Add, edit and delete sources in the TUI; changes save to the config file and keep its
  comments
- It reloads every minute and keeps the cursor on the same PR
- A tab-separated list when stdout isn't a terminal, for scripts

The screenshots show made-up PRs.

## Install

```
cargo install --git https://github.com/carlos-alberto/open-prs
```

This puts `open_prs` in `~/.cargo/bin`. Each source needs one of these:

- A token of its own, with only the rights that open_prs needs; see [Tokens](#tokens)
- [`gh`](https://cli.github.com), logged in, for GitHub sources, or
  [`tea`](https://gitea.com/gitea/tea), with a login, for Forgejo and Gitea sources. These
  logins have all your rights.

## Usage

```
open_prs [-m/--mine] [-a/--approved | -n/--needs-review] [-w/--for-you] [-u/--new]
         [-f/--failing] [-d/--hide-drafts] [--text WORDS] [-g/--group repo|user]
         [-i/--interval SECONDS] [--notify | --no-notify] [--writes | --no-writes]
         [--github QUERY]... [--forgejo LOGIN]...
```

With no sources, it shows the PRs on github.com that involve you. The flags set how the TUI
starts. The keys `m`, `a`, `n`, `w`, `u`, `f`, `d` and `g` toggle them while it runs, and `/`
changes the words. It reloads every SECONDS (default 60).

| Key | Action |
|-----|--------|
| `enter`, `o`, click the cursor row | Open the PR in the browser, and mark it as seen |
| `p` | Preview the PR, and mark it as seen |
| `c` | Open the PR's CI runs |
| `v` | Show who reviews the PR; with writes on, ask people to review it or stop asking them |
| `/` | Filter by words in the repo, number, title, author or host; `enter` keeps them, `esc` clears them |
| `esc` | Clear the words |
| `y` | Copy the PR's URL |
| `r` | Reload now |
| `m` | Only PRs you opened |
| `a` / `n` | Only approved PRs / only PRs that need review |
| `w` | Only PRs that ask for your review |
| `u` | Only PRs that changed since you last saw them |
| `f` | Only PRs with failing CI |
| `d` | Hide drafts |
| `g` | Group by repo or by author |
| `s` | Add, edit and delete sources |
| `space` | Mark the PR as seen, or as new again |
| `b` | Turn desktop notifications on or off |
| `↑` `↓` `PgUp` `PgDn` `Home` `End`, scroll | Move the cursor |
| `←` `→`, sideways scroll | Scroll the columns; Repo and # stay |
| `?` | Show every key |
| `q`, `ctrl+c` | Quit |

The footer shows as many keys as fit, and `?` shows them all.

## The table

The CI column is `passed`, `failing` or `pending`, and blank when the last commit has no
checks. The Notes column says when a PR changed since you last saw it (`new`), asks for your
review (`for you`, which on GitHub includes requests to your teams), is a `draft`, or has a
`conflict` with its base branch. Drafts are dim.

The Comments column has the number that the host shows next to the PR in its own list, and
is blank for none. GitHub counts the comments in the conversation and the inline comments on
the code. Forgejo counts each comment in the conversation and each review, also an approval
with no text, but not the inline comments.

The Host column shows only when the PRs come from more than one host. The table leaves out
the owner when every PR has the same one. When two sources find the same PR, it shows once,
from the first source.

On Forgejo, `c` opens the repo's list of action runs, as there is no checks page per PR.

### New activity

A PR is `new` when it changed since you last opened it or previewed it: a new commit,
comment or review, or a new title. Its title is bold, and the header counts them. `u` shows
only these, and `space` marks the PR under the cursor as seen, or as new again. A PR that
opens is new too.

The first time a source loads, its PRs count as seen, so a first run or a new source
doesn't mark everything. `$XDG_STATE_HOME/open_prs/seen.json`, else
`~/.local/state/open_prs/seen.json`, keeps when you last saw each PR. The host decides
what counts as a change, so your own comments also make a PR new.

### Stacks

A PR whose base is another open PR's branch is stacked on it, and that PR merges first.
Grouped by repo, a stacked PR shows under the PR it is on, joined to it by lines. In the
screenshot, api #421 is on #418, which is on #412.

When the tree can't show it, because the PRs are grouped by user or a filter hides the PR it
is on, the Notes column says `on #N`. A branch from a fork can't have PRs stacked on it.

### Narrow terminals

When the title has less than 25 columns, the table puts one space between the columns and
shortens `changes requested` to `changes`. Nothing else hides: `→` scrolls the columns after
`#` to the left, so the title gets their room, and `‹` shows that some are hidden. `←`
brings them back.

![The same PRs in 92 columns, scrolled past Host, Status and CI](docs/narrow.svg)

## Notifications

Notifications are off until you turn them on. `b` turns them on or off, and the header says
`notifications on` while they are. When the sources come from the config file, `b` saves
`notify = true` or `notify = false` in it, so the next run does the same. `--notify` and
`--no-notify` turn them on or off for one run, whatever the file says. When `b` turns them
on, it sends one, so you can see that they work.

After the first load of each source, each reload sends a desktop notification when a PR
starts to ask for your review, and when one of your PRs gets approved, gets changes
requested, or starts to fail CI. One reload sends three at most: when more PRs changed, the
third says how many. A PR that two sources find sends one.

On macOS it uses `osascript`, so the notifications come from Script Editor; if none show,
allow notifications for Script Editor in System Settings. On Linux it uses `notify-send`.
Without either, it asks the terminal to show one (OSC 9), which some terminals do.

## Preview

`p` shows the PR under the cursor: its description, the checks on its last commit that
didn't pass, the files it changes, and its last 10 comments and reviews. `↑` `↓` `PgUp`
`PgDn` scroll, `←` `→` go to the PR above or below in the table, and `esc` goes back. `o`,
`c` and `y` work as in the table.

![The preview of a PR with a pending check and three comments](docs/preview.svg)

It loads each time it opens, and shows the last preview of that PR while it loads. For a
Forgejo or Gitea source that uses tea, tea doesn't give the files or each check, so the
preview shows the CI state of them all. A source with a token gets the files and each check.

## Reviewers

`v` shows who the PR asks for a review, with a tick, and the people who can review it. Type
to filter the names, `space` ticks or unticks the name under the cursor, and `enter` saves.
With nothing ticked yet, `enter` ticks the name under the cursor first, so `v`, a name and
`enter` asks one person or team. A team is `org/team`, and the words match its name after
the `/`, so `eng` finds `acme/engineering`. A name that isn't in the list shows as typed,
for someone it doesn't know.

![The reviewers of a PR: it asks bao, and the acme/backend team is ticked](docs/reviewers.svg)

To ask for reviews, open_prs must change the PR on the host. It makes no change on a host
until you turn writes on: put `writes = true` in the config file, or start it with
`--writes`. `--no-writes` turns writes off for one run, whatever the file says. No key in the
TUI turns writes on, so a key that you press by mistake cannot change a PR. While writes are
off, `v` shows who reviews the PR, and says how to turn writes on. While writes are on, the
header says `writes on`.

The host tells each person it asks, so the panel says what `enter` does before you press
it. On GitHub, the list has GitHub's suggestions, then the teams with access to the repo,
then the users who can be assigned in it. With a token, Forgejo and Gitea give the people
who can review, then the repo's teams. tea can't list who can review, so for a source that
uses tea, the list has the authors of the source's PRs and the PR's reviewers, and no teams.

## Sources

Each source is a GitHub search, or the PRs on a Forgejo or Gitea. They come from, in order:

1. `--github QUERY` and `--forgejo LOGIN`, each as many times as you like. Changes made with
   `s` last until you quit.
2. `$XDG_CONFIG_HOME/open_prs/config.toml`, else `~/.config/open_prs/config.toml`. Changes
   made with `s` save to this file, and keep its comments.
3. Neither: one GitHub search for `involves:@me`.

```toml
notify = true                  # desktop notifications; b sets it; default false
writes = true                  # let v change the reviewers on the host; default false

[[source]]
kind = "github"
query = "org:rust-lang"        # GitHub search qualifiers; default involves:@me
auth = "token"                 # a token of its own; "gh", or no auth, uses gh's login
# hostname = "ghe.example.com" # for GitHub Enterprise
# name = "Rust"                # the Host column; default GitHub, or the hostname

[[source]]
kind = "forgejo"               # or "gitea"
url = "https://codeberg.org"   # with auth = "token"
auth = "token"
owner = "forgejo"              # only this user's or org's repos; without it, a public
                               # instance gives the PRs of every public repo
# name = "Codeberg"            # default: the host

[[source]]
kind = "forgejo"
login = "home"                 # a name from `tea logins list`, without auth = "token"
```

`s` shows each source, how many PRs it found, its Auth (`token`, `gh` or `tea`), and the
error of a source that failed. For a source that uses gh or tea, it says that the source has
all the rights of that login. `a` adds a source with a token of its own, unless you pick gh
or tea for Auth.

![The Sources panel, with two token sources and a GitHub Enterprise source that failed](docs/sources.svg)

## Tokens

The login of gh or tea can do all that you can do: a gh token usually has the `repo` scope,
which can write to every repo that you can get to, and tea signs with your SSH key. A bug in
open_prs, a bad dependency, or a different `gh` earlier in `PATH` would get all those rights.
A source with `auth = "token"` uses a token of its own in place of that, with only the
rights that open_prs needs. open_prs then calls the host's API itself.

In the TUI, `s`, then `a` or `e`. In the form, `ctrl+o` opens the host's page that makes a
token. Make the token there, copy it, and paste it in the Token field. The field shows
`••••`, never the token, and `enter` puts the token in the keychain. When you delete a source,
open_prs asks if it must delete the source's token from the keychain too.

open_prs looks for a source's token in this order:

1. `token_command = "op read op://Work/GitHub/token"` on the source: open_prs runs this
   program without a shell, and reads the token from what it prints.
2. The keychain: macOS Keychain, or the Secret Service on Linux. The service is `open_prs`,
   and the account is `<kind>:<host>:<name>`, such as `github:github.com:GitHub`.
3. `token_env = "NAME"` on the source: the environment variable `NAME`, for scripts and CI.

If it finds no token, the source fails, and the error says where open_prs looked. Plain
output finds tokens in the same way.

What open_prs does with a token:

- It sends the token only to the source's own host, over https. It follows no redirect, so
  a redirect to another host fails and the token stays where it was.
- It reads each token once per run, and again after the host refuses it.
- It never writes a token to the config file, to `seen.json` or to the screen.
- gh, tea and a `token_command` don't get the variables in `token_env`. The programs for the
  browser, the clipboard and notifications get almost no environment at all.
- A Forgejo or Gitea on `http`, such as one on your own network, needs `allow_http = true`
  on the source. The token then goes as plain text.

### A GitHub token

`ctrl+o` in the form opens GitHub's page for a new fine-grained token, with its name, the
owner from the query (`org:`, `user:` or `repo:`), and these permissions filled in. Choose
the repositories, and click Generate token. GitHub Enterprise Server 3.19 and older don't
fill in the page, so set these yourself there. The page is in GitHub's Settings, Developer
settings, Personal access tokens, Fine-grained tokens:

- Resource owner: the org, or you.
- Repository access: the repos that open_prs must see.
- Repository permissions:
  - Pull requests: Read. Read and write only if `v` must ask for reviews.
  - Commit statuses: Read, for CI.
  - Metadata: Read. GitHub adds it.
  - Administration: Read, only if `v` must list the repo's teams. This also lets the token
    read the repo's settings. Without it, the reviewer panel has no teams.

Limits of fine-grained tokens:

- A token works for one owner only: one org, or your own repos. Thus each org needs its own
  source, with a query such as `org:acme`, and its own token. A search across orgs, such as
  `involves:@me`, needs a classic token. A classic token needs the `repo` scope to read
  private repos, and that scope can write too.
- GitHub's docs say that fine-grained tokens can't use the Checks API. CI from check runs,
  such as GitHub Actions, may not show with such a token.
- An org can make its owners approve each fine-grained token, or block them.
- A fine-grained token expires: by default after 30 days.
- If the org uses SAML SSO, authorize the token for it. When GitHub asks for this, the error
  gives the link.

For GitHub Enterprise, set `hostname`, and make the token on that host.

### A Forgejo or Gitea token

`ctrl+o` in the form opens the host's page for tokens: Settings, Applications, Access tokens.
Give the token these scopes:

- `read:repository`: the PRs, reviews, files, CI, who can review, and the repo's teams.
  `write:repository` only if `v` must ask for reviews.
- `read:issue`: the search for PRs, and their comments.
- `read:user`: who you are, for `mine` and `for you`.

## Plain output

When stdout isn't a terminal, it prints one tab-separated line per PR instead: host, repo,
number, status, author, title, url, ci, notes, comments. The notes have commas between them.
It exits 1 if a source failed. It reads what you saw, for `new`, but doesn't change it.

```
$ open_prs --github org:acme --failing | cut -f 1-5,8,9
GitHub	acme/billing	88	changes requested	chidi	failing	conflict
GitHub	acme/web	1291	needs review	sam	failing	on #1290
```

## Development

```
cargo test
cargo clippy --all-targets
cargo test screenshots -- --ignored   # draws docs/*.svg again
cargo deny check                      # security holes, licenses and sources of the crates
```

The screenshots are the real TUI, drawn with the made-up PRs in `src/screenshots.rs`. CI
runs these on each push and PR, and fails when the screenshots don't match the code.
`deny.toml` sets what `cargo deny check` allows. Each week, Dependabot makes PRs for new
versions of the crates and the actions.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.
