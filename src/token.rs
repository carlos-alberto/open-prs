//! A source's own token: from its token_command, else the OS keychain, else its token_env.
//! open_prs keeps a token only in memory. It never writes one to a file, a log or the screen.

use std::collections::{BTreeSet, HashMap};
use std::env;
use std::fmt;
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};

use anyhow::{Context, Result, bail};

use crate::config::{Source, Token};

/// The keychain's service for every token of open_prs.
pub const SERVICE: &str = "open_prs";

/// A token. Its Debug output hides it, so an error or a log can't show it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(token: &str) -> Self {
        Secret(token.trim().to_owned())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(••••)")
    }
}

/// Where tokens are kept: the OS keychain, or a fake one in the tests.
pub trait Keychain: Sync {
    fn get(&self, account: &str) -> Result<Option<String>>;
    fn set(&self, account: &str, token: &str) -> Result<()>;
    /// False when there was nothing to delete.
    fn delete(&self, account: &str) -> Result<bool>;
}

/// macOS Keychain, or the Secret Service on Linux.
pub struct Os;

static STORE: LazyLock<Result<(), String>> = LazyLock::new(|| {
    #[cfg(target_os = "macos")]
    let store = apple_native_keyring_store::keychain::Store::new();
    #[cfg(all(unix, not(target_os = "macos")))]
    let store = zbus_secret_service_keyring_store::Store::new();
    #[cfg(any(target_os = "macos", all(unix, not(target_os = "macos"))))]
    return match store {
        Ok(store) => {
            keyring_core::set_default_store(store);
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    };
    #[allow(unreachable_code)]
    Err("open_prs has no keychain on this system".into())
});

fn entry(account: &str) -> Result<keyring_core::Entry> {
    if let Err(e) = &*STORE {
        bail!("could not open the keychain: {e}");
    }
    keyring_core::Entry::new(SERVICE, account).context("could not open the keychain")
}

impl Keychain for Os {
    fn get(&self, account: &str) -> Result<Option<String>> {
        match entry(account)?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(e).context("could not read the keychain"),
        }
    }

    fn set(&self, account: &str, token: &str) -> Result<()> {
        entry(account)?
            .set_password(token)
            .context("could not save the token in the keychain")
    }

    fn delete(&self, account: &str) -> Result<bool> {
        match entry(account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring_core::Error::NoEntry) => Ok(false),
            Err(e) => Err(e).context("could not delete the token from the keychain"),
        }
    }
}

/// The token for `source`, from the first place that has one: its `token.command`, the
/// keychain, or the variable `token.env`. `var` reads a variable.
pub fn find(
    source: &Source,
    token: &Token,
    keychain: &dyn Keychain,
    var: &dyn Fn(&str) -> Option<String>,
) -> Result<Secret> {
    if let Some(command) = &token.command {
        return run(command).with_context(|| format!("the token_command of {}", source.name()));
    }
    let account = source.account();
    let mut looked = vec![format!(
        "the keychain, as service {SERVICE} and account {account}"
    )];
    match keychain.get(&account) {
        Ok(Some(token)) if !token.trim().is_empty() => return Ok(Secret::new(&token)),
        Ok(_) => {}
        Err(e) => looked[0] = format!("{}, which failed: {e:#}", looked[0]),
    }
    if let Some(name) = &token.env {
        match var(name).filter(|t| !t.trim().is_empty()) {
            Some(token) => return Ok(Secret::new(&token)),
            None => looked.push(format!("${name}")),
        }
    }
    bail!(
        "no token for {}; open_prs looked in {}. To add one, press s, then e on the source",
        source.name(),
        looked.join(", and in ")
    )
}

/// Runs `command`, split into words as a shell does but with no shell, and reads the token
/// from what it prints.
fn run(command: &str) -> Result<Secret> {
    let words = shlex::split(command).context("its quotes don't match")?;
    let Some((program, args)) = words.split_first() else {
        bail!("it is empty");
    };
    let mut command = Command::new(program);
    // It needs the user's environment, but not the tokens of the other sources
    for name in hidden() {
        command.env_remove(name);
    }
    let out = command
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("could not run {program}"))?;
    if !out.status.success() {
        // Only stderr: stdout can hold the token
        let stderr = String::from_utf8_lossy(&out.stderr);
        let first = stderr.lines().next().unwrap_or_default().trim();
        bail!("{program} exited {}: {first}", out.status);
    }
    let token = String::from_utf8(out.stdout).context("it printed invalid UTF-8")?;
    if token.trim().is_empty() {
        bail!("{program} printed nothing");
    }
    Ok(Secret::new(&token))
}

/// The tokens found so far, so the keychain and a token_command are asked once per run.
static FOUND: LazyLock<Mutex<HashMap<String, Secret>>> = LazyLock::new(Mutex::default);

/// What the tokens found so far are kept by: the source's account, and where to look.
fn found_key(source: &Source, token: &Token) -> String {
    format!("{}\n{:?}\n{:?}", source.account(), token.command, token.env)
}

/// The token of `source`, which has a token of its own.
pub fn get(source: &Source) -> Result<Secret> {
    let Some(token) = source.token() else {
        bail!(
            "{} uses the login of {}",
            source.name(),
            source.cli().unwrap_or("its program")
        );
    };
    let key = found_key(source, token);
    if let Some(found) = FOUND.lock().unwrap().get(&key) {
        return Ok(found.clone());
    }
    let found = find(source, token, &Os, &|name| env::var(name).ok())?;
    FOUND.lock().unwrap().insert(key, found.clone());
    Ok(found)
}

/// Looks for the token of `source` again next time, as after the host refused it.
pub fn forget(source: &Source) {
    if let Some(token) = source.token() {
        FOUND.lock().unwrap().remove(&found_key(source, token));
    }
}

/// Puts `token` in `keychain` for `source`.
pub fn save(keychain: &dyn Keychain, source: &Source, token: &str) -> Result<()> {
    keychain.set(&source.account(), token.trim())?;
    forget(source);
    Ok(())
}

/// Whether `keychain` has a token for `source`.
pub fn saved(keychain: &dyn Keychain, source: &Source) -> bool {
    matches!(keychain.get(&source.account()), Ok(Some(_)))
}

/// Deletes the token of `source` from `keychain`; false when it had none.
pub fn delete(keychain: &dyn Keychain, source: &Source) -> Result<bool> {
    forget(source);
    keychain.delete(&source.account())
}

/// Moves the token of `old` to `new`, whose account differs, as when the source's name changes.
pub fn rename(keychain: &dyn Keychain, old: &Source, new: &Source) -> Result<()> {
    if old.account() == new.account() {
        return Ok(());
    }
    if let Some(token) = keychain.get(&old.account())? {
        keychain.set(&new.account(), &token)?;
        keychain.delete(&old.account())?;
    }
    forget(old);
    forget(new);
    Ok(())
}

/// The variables that hold tokens, which gh and tea must not get.
static VARS: LazyLock<Mutex<BTreeSet<String>>> = LazyLock::new(Mutex::default);

/// Notes the token_env of each source, so `hidden` gives it.
pub fn note_vars<'a>(sources: impl IntoIterator<Item = &'a Source>) {
    let mut vars = VARS.lock().unwrap();
    vars.extend(sources.into_iter().filter_map(|s| s.token()?.env.clone()));
}

/// The variables to take out of the environment of gh and tea.
pub fn hidden() -> Vec<String> {
    VARS.lock().unwrap().iter().cloned().collect()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A keychain in memory.
    #[derive(Default)]
    pub struct Fake(pub Mutex<HashMap<String, String>>);

    impl Keychain for Fake {
        fn get(&self, account: &str) -> Result<Option<String>> {
            Ok(self.0.lock().unwrap().get(account).cloned())
        }

        fn set(&self, account: &str, token: &str) -> Result<()> {
            self.0.lock().unwrap().insert(account.into(), token.into());
            Ok(())
        }

        fn delete(&self, account: &str) -> Result<bool> {
            Ok(self.0.lock().unwrap().remove(account).is_some())
        }
    }

    struct Locked;

    impl Keychain for Locked {
        fn get(&self, _: &str) -> Result<Option<String>> {
            bail!("the keychain is locked")
        }
        fn set(&self, _: &str, _: &str) -> Result<()> {
            bail!("the keychain is locked")
        }
        fn delete(&self, _: &str) -> Result<bool> {
            bail!("the keychain is locked")
        }
    }

    fn source(token: Token) -> Source {
        Source::github("org:acme", None, Some("Work".into())).with_token(token)
    }

    #[test]
    fn the_command_comes_first_then_the_keychain_then_the_variable() {
        let keychain = Fake::default();
        keychain
            .set("github:github.com:Work", "from-keychain")
            .unwrap();
        let var = |name: &str| (name == "WORK_TOKEN").then(|| "from-env".to_owned());
        let find = |token: Token| {
            let source = source(token.clone());
            find(&source, &token, &keychain, &var).map(|s| s.expose().to_owned())
        };

        let command = Some("printf ' tok\\n'".into());
        let env = Some("WORK_TOKEN".into());
        let all = Token {
            command: command.clone(),
            env: env.clone(),
        };
        assert_eq!(find(all).unwrap(), "tok");
        let no_command = Token { command: None, env };
        assert_eq!(find(no_command.clone()).unwrap(), "from-keychain");
        keychain.delete("github:github.com:Work").unwrap();
        assert_eq!(find(no_command).unwrap(), "from-env");
    }

    #[test]
    fn the_command_has_no_shell() {
        let keychain = Fake::default();
        let token = Token {
            command: Some("echo 'a b' $HOME; rm -rf /".into()),
            env: None,
        };
        let found = find(&source(token.clone()), &token, &keychain, &|_| None).unwrap();
        assert_eq!(found.expose(), "a b $HOME; rm -rf /");
    }

    #[test]
    fn a_failed_command_says_why_and_shows_no_token() {
        let keychain = Fake::default();
        let token = Token {
            command: Some("sh -c 'echo secret; echo locked >&2; exit 3'".into()),
            env: None,
        };
        let error = find(&source(token.clone()), &token, &keychain, &|_| None).unwrap_err();
        let error = format!("{error:#}");
        assert!(
            error.starts_with("the token_command of Work: sh exited"),
            "{error}"
        );
        assert!(error.ends_with(": locked"), "{error}");
        assert!(!error.contains("secret"));

        let token = Token {
            command: Some("'open".into()),
            env: None,
        };
        let error = find(&source(token.clone()), &token, &keychain, &|_| None).unwrap_err();
        assert!(format!("{error:#}").contains("quotes don't match"));
    }

    #[test]
    fn without_a_token_it_says_where_it_looked() {
        let token = Token {
            command: None,
            env: Some("WORK_TOKEN".into()),
        };
        let error = find(&source(token.clone()), &token, &Fake::default(), &|_| None);
        assert_eq!(
            format!("{:#}", error.unwrap_err()),
            "no token for Work; open_prs looked in the keychain, as service open_prs and \
             account github:github.com:Work, and in $WORK_TOKEN. To add one, press s, then e \
             on the source"
        );

        // A keychain that fails doesn't stop the variable, and the error says why it failed
        let var = |_: &str| Some("from-env".to_owned());
        assert_eq!(
            find(&source(token.clone()), &token, &Locked, &var)
                .unwrap()
                .expose(),
            "from-env"
        );
        let error = find(&source(token.clone()), &token, &Locked, &|_| None).unwrap_err();
        assert!(format!("{error:#}").contains("which failed: the keychain is locked"));
    }

    #[test]
    fn debug_hides_the_token() {
        assert_eq!(format!("{:?}", Secret::new("tok")), "Secret(••••)");
    }
}
