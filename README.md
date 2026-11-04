# open_prs

Lists open PRs on GitHub, Forgejo and Gitea, with their review status, in a TUI.

```
open_prs [-m/--mine] [-a/--approved | -n/--needs-review] [-g/--group repo|user] [-i/--interval SECONDS]
         [--github QUERY]... [--forgejo LOGIN]...
```

The flags set how the TUI starts. The keys `m`, `a`, `n` and `g` toggle them while it runs.
It reloads every SECONDS (default 60) and keeps the cursor on the same PR.

| Key | Action |
|-----|--------|
| `enter`, `o`, click the cursor row | Open the PR in the browser |
| `y` | Copy the PR's URL |
| `r` | Reload now |
| `m` | Only PRs you opened |
| `a` / `n` | Only approved PRs / only PRs that need review |
| `g` | Group by repo or by author |
| `s` | Add, edit and delete sources |
| `↑` `↓` `PgUp` `PgDn` `Home` `End`, scroll | Move the cursor |
| `q`, `ctrl+c` | Quit |

When stdout isn't a terminal, it prints one tab-separated line per PR instead:
host, repo, number, status, author, title, url. It exits 1 if a source failed.

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
