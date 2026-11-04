//! When you last saw each PR, so the table can mark the ones with new activity since then.
//!
//! A PR is new when it changed since you last opened or previewed it, or when it showed up
//! after the first load of its source. The first load of a source puts its PRs down as seen,
//! so a first run, or a new source, doesn't mark everything.

use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeDelta, Utc};

use crate::pr::Pr;

/// How long it keeps a PR that no source found.
const KEEP: TimeDelta = TimeDelta::days(30);

pub struct Seen {
    /// None keeps it in memory, as without a home folder
    path: Option<PathBuf>,
    /// Each PR's updated time when you last saw it, by URL
    times: BTreeMap<String, DateTime<Utc>>,
    /// The file is there, so it knows every PR it saw; a PR it doesn't know is new
    exists: bool,
}

/// `$XDG_STATE_HOME/open_prs/seen.json`, else `~/.local/state/open_prs/seen.json`.
pub fn path() -> Option<PathBuf> {
    let base = env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::home_dir().map(|home| home.join(".local").join("state")))?;
    Some(base.join("open_prs").join("seen.json"))
}

impl Seen {
    /// Reads the file at `path`; without one, nothing is seen yet.
    pub fn load(path: Option<PathBuf>) -> Result<Seen> {
        let Some(file) = &path else {
            return Ok(Seen::memory());
        };
        let text = match fs::read_to_string(file) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Ok(Seen {
                    path,
                    ..Seen::memory()
                });
            }
            Err(e) => return Err(e).with_context(|| format!("could not read {}", file.display())),
        };
        let times = serde_json::from_str(&text).with_context(|| {
            format!(
                "could not read {}; delete it to start again",
                file.display()
            )
        })?;
        Ok(Seen {
            path,
            times,
            exists: true,
        })
    }

    /// Nothing seen, and nothing saved.
    pub fn memory() -> Seen {
        Seen {
            path: None,
            times: BTreeMap::new(),
            exists: false,
        }
    }

    /// The file is there, or it saved.
    pub fn exists(&self) -> bool {
        self.exists
    }

    pub fn is_new(&self, p: &Pr) -> bool {
        match self.times.get(&p.url) {
            Some(seen) => p.updated > *seen,
            None => self.exists,
        }
    }

    pub fn mark(&mut self, p: &Pr) {
        self.times.insert(p.url.clone(), p.updated);
    }

    /// Makes `p` new again, as if you last saw it before its last change.
    pub fn unmark(&mut self, p: &Pr) {
        self.times
            .insert(p.url.clone(), p.updated - TimeDelta::seconds(1));
    }

    /// Puts down the PRs it doesn't know as seen, for the first load of a source.
    pub fn start(&mut self, prs: &[Pr]) {
        for p in prs {
            self.times.entry(p.url.clone()).or_insert(p.updated);
        }
    }

    /// Writes the file. With `open`, the URLs of every open PR, it forgets the PRs that
    /// closed; leave it out when a source failed, as then its PRs are missing. It keeps a
    /// PR that changed in the last `KEEP`, which other sources, or other flags, can find.
    pub fn save(&mut self, open: Option<&HashSet<&str>>, now: DateTime<Utc>) -> Result<()> {
        if let Some(open) = open {
            self.times
                .retain(|url, seen| open.contains(url.as_str()) || now - *seen < KEEP);
        }
        // From here on, it knows every PR it saw
        self.exists = true;
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("could not make {}", dir.display()))?;
        }
        // Write a new file and move it over the old one, so a crash can't leave half a file
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&self.times)?;
        fs::write(&tmp, text + "\n")
            .with_context(|| format!("could not write {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("could not replace {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::Status;
    use crate::pr::tests::pr;

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::UNIX_EPOCH + TimeDelta::minutes(minutes)
    }

    fn pr_at(number: u64, minutes: i64) -> Pr {
        let mut p = pr("GitHub", "api", number, "amy", Status::Needs);
        p.updated = at(minutes);
        p
    }

    /// A folder for the state file, removed when the test ends.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = env::temp_dir().join(format!("open_prs-seen-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            TempDir(dir)
        }

        fn file(&self) -> PathBuf {
            self.0.join("open_prs").join("seen.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_pr_is_new_when_it_changed_since_you_saw_it() {
        let mut seen = Seen::memory();
        let mut p = pr_at(1, 10);
        seen.mark(&p);
        assert!(!seen.is_new(&p));
        p.updated = at(11);
        assert!(seen.is_new(&p));
        seen.mark(&p);
        assert!(!seen.is_new(&p));
        seen.unmark(&p);
        assert!(seen.is_new(&p));
    }

    #[test]
    fn the_first_load_of_a_source_is_seen_and_later_prs_are_new() {
        let dir = TempDir::new("first");
        let mut seen = Seen::load(Some(dir.file())).unwrap();
        let (old, later) = (pr_at(1, 10), pr_at(2, 20));
        // Without the file, it can't tell a new PR from one it never saw
        assert!(!seen.is_new(&later));
        seen.start(std::slice::from_ref(&old));
        seen.save(None, at(30)).unwrap();
        assert!(!seen.is_new(&old));
        assert!(seen.is_new(&later));
        // start doesn't forget that a PR changed
        let mut changed = old.clone();
        changed.updated = at(15);
        seen.start(std::slice::from_ref(&changed));
        assert!(seen.is_new(&changed));
    }

    #[test]
    fn save_keeps_the_open_prs_and_load_reads_them_back() {
        let dir = TempDir::new("save");
        let mut seen = Seen::load(Some(dir.file())).unwrap();
        let (open, closed, recent) = (pr_at(1, 10), pr_at(2, 20), pr_at(3, 60 * 24 * 20));
        for p in [&open, &closed, &recent] {
            seen.mark(p);
        }
        let now = at(60 * 24 * 40);
        seen.save(Some(&HashSet::from([open.url.as_str()])), now)
            .unwrap();

        let seen = Seen::load(Some(dir.file())).unwrap();
        assert!(!seen.is_new(&open));
        assert!(
            seen.is_new(&closed),
            "a PR that no source found is forgotten"
        );
        assert!(!seen.is_new(&recent), "but not in its first 30 days");
        assert!(!dir.file().with_extension("json.tmp").exists());
    }

    #[test]
    fn load_says_which_file_it_could_not_read() {
        let dir = TempDir::new("bad");
        fs::create_dir_all(dir.file().parent().unwrap()).unwrap();
        fs::write(dir.file(), "{").unwrap();
        let error = format!("{:#}", Seen::load(Some(dir.file())).err().unwrap());
        assert!(
            error.contains("seen.json; delete it to start again"),
            "{error}"
        );
    }
}
