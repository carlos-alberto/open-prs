# open_prs

Lists open PRs on GitHub, Forgejo and Gitea, with their review status and CI status, in a TUI.

```
open_prs [-m/--mine] [-a/--approved | -n/--needs-review] [-w/--for-you] [-f/--failing]
         [-d/--hide-drafts] [--text WORDS] [-g/--group repo|user] [-i/--interval SECONDS]
         [--github QUERY]... [--forgejo LOGIN]...
```

The flags set how the TUI starts. The keys `m`, `a`, `n`, `w`, `f`, `d` and `g` toggle them
while it runs, and `/` changes the words. It reloads every SECONDS (default 60) and keeps the cursor on the same PR.

| Key | Action |
|-----|--------|
| `enter`, `o`, click the cursor row | Open the PR in the browser |
| `c` | Open the PR's CI runs |
| `/` | Filter by words in the repo, number, title, author or host; `enter` keeps them, `esc` clears them |
| `esc` | Clear the words |
| `y` | Copy the PR's URL |
| `r` | Reload now |
| `m` | Only PRs you opened |
| `a` / `n` | Only approved PRs / only PRs that need review |
| `w` | Only PRs that ask for your review |
| `f` | Only PRs with failing CI |
| `d` | Hide drafts |
| `g` | Group by repo or by author |
| `s` | Add, edit and delete sources |
| `↑` `↓` `PgUp` `PgDn` `Home` `End`, scroll | Move the cursor |
| `←` `→`, sideways scroll | Scroll the columns; Repo and # stay |
| `?` | Show every key |
| `q`, `ctrl+c` | Quit |

The footer shows as many keys as fit, and `?` shows them all.

The Host column shows only when the PRs come from more than one host. When the title has
less than 25 columns, the table puts one space between columns and shortens
`changes requested` to `changes`. Nothing else hides: `→` scrolls the columns after `#` to
the left, so the title gets their room, and `‹` shows that some are hidden. `←` brings them
back.

The CI column is `passed`, `failing` or `pending`, and blank when the last commit has no
checks. The Notes column says when a PR asks for your review (`for you`, which on GitHub
includes requests to your teams), is a `draft`, or has a `conflict` with its base branch.
Drafts are dim.

The Comments column has the number that the host shows next to the PR in its own list, and
is blank for none. GitHub counts the comments in the conversation and the inline comments
on the code. Forgejo counts each comment in the conversation and each review, also an
approval with no text, but not the inline comments.

A PR whose base is another open PR's branch is stacked on it, and that PR merges first.
Grouped by repo, a stacked PR shows under the PR it is on, joined to it by lines:

```
api        GitHub  #412          approved
                   └─ #418       approved
                      └─ #421    needs review
web        GitHub  #2            approved
                   ├─ #3         changes requested
                   └─ #4         approved
```

When the tree can't show it, because the PRs are grouped by user or a filter hides the PR
it is on, the Notes column says `on #N`. A branch from a fork can't have PRs stacked on it. On Forgejo, `c` opens the repo's list of action runs, as there is no
checks page per PR.

When stdout isn't a terminal, it prints one tab-separated line per PR instead:
host, repo, number, status, author, title, url, ci, notes, comments. The notes have
commas between them. It exits 1 if a source failed.

## Sources

Each source is a GitHub search or a tea login. They come from, in order:

1. `--github QUERY` and `--forgejo LOGIN`, each as many times as you like. Changes made
   with `s` last until you quit.
2. `$XDG_CONFIG_HOME/open_prs/config.toml`, else `~/.config/open_prs/config.toml`. Changes
   made with `s` save to this file, and keep its comments.
3. Neither: one GitHub search for `involves:@me`.

```toml
[[source]]
kind = "github"
query = "org:rust-lang"        # GitHub search qualifiers; default involves:@me
# hostname = "ghe.example.com" # for GitHub Enterprise
# name = "Rust"                # the Host column; default GitHub, or the hostname

[[source]]
kind = "forgejo"               # or "gitea"
login = "codeberg.org"         # a name from `tea logins list`
owner = "forgejo"              # only this user's or org's repos; without it, a public
                               # instance gives the PRs of every public repo
# name = "Codeberg"            # default: the login
```

When two sources find the same PR, it shows once, from the first source. The table leaves
out the owner when every PR has the same one.

## Needs

- `gh`, logged in, for GitHub sources
- `tea`, with a login, for Forgejo and Gitea sources

## Build

```
cargo build --release
cargo install --path .   # puts open_prs in ~/.cargo/bin
```
