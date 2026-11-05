//! The desktop programs that open_prs starts: open or xdg-open, pbcopy, and osascript or
//! notify-send. They get only the environment that they need, so no token or other secret of
//! open_prs gets to them.

use std::env;
use std::process::{Command, Stdio};
use std::thread;

/// The variables that a desktop program can need. The first eight find the program, the
/// user's files, the language and the display. xdg-open uses the last three to find the
/// browser.
const KEEP: [&str; 11] = [
    "PATH",
    "HOME",
    "LANG",
    "TMPDIR",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_CURRENT_DESKTOP",
    "DESKTOP_SESSION",
    "BROWSER",
];

/// A command for `program`, with an empty environment but for the variables in `KEEP` that
/// are set.
pub fn command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.env_clear();
    for name in KEEP {
        if let Some(value) = env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

/// Opens `url` in the browser. It doesn't wait for the browser to start.
pub fn browse(url: &str) -> Result<(), String> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = command(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{opener}: {e}"))?;
    thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_desktop_program_gets_only_the_variables_it_needs() {
        // cargo test sets CARGO_PKG_NAME and more, so there is something to leave out
        assert!(env::vars_os().any(|(name, _)| !KEEP.iter().any(|k| name == *k)));
        let out = command("env").output().unwrap();
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        let names: Vec<&str> = text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(n, _)| n)
            .collect();
        for name in &names {
            assert!(KEEP.contains(name), "{name} got through");
        }
        if env::var_os("PATH").is_some() {
            assert!(names.contains(&"PATH"), "{text}");
        }
    }
}
