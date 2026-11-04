# open_prs

Lists the open PRs in the `acme` org on GitHub and on Forgejo
(git.example.com), with their review status, in a TUI.

```
open_prs [-m/--mine] [-a/--approved | -n/--needs-review] [-g/--group repo|user] [-i/--interval SECONDS]
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
| `↑` `↓` `PgUp` `PgDn` `Home` `End`, scroll | Move the cursor |
| `q`, `ctrl+c` | Quit |

When stdout isn't a terminal, it prints one tab-separated line per PR instead:
host, repo, number, status, author, title, url. It exits 1 if a host failed.

## Needs

- `gh`, logged in to GitHub
- `tea`, with a login named `work` for https://git.example.com

## Build

```
cargo build --release
cargo install --path .   # puts open_prs in ~/.cargo/bin
```
