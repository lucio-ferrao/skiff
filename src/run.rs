//! A terminal shared by the user and an agent: the agent types into a session the user watches
//! and reads its screen, and the user types there too, say a password the agent never sees.
//!
//! `skiff run <session> <command>` types a command and waits for it to end, printing its output
//! and ending with its exit code; the end is found by a marker printed after the command, read on
//! the session's screen. `skiff send` types keys as they are, `skiff read` prints the screen,
//! `skiff show` only shows the session, starting it if it ended. Typing shows the session to the user: in the panel beside (`skiff panel`), if one runs, and
//! floating in Termarchy, which follows the `shown` label through `skiff watch`.

use std::io;
use std::time::{Duration, Instant};

use crate::{panel, session, zmx};

/// The shells at whose prompt a command may be typed.
const SHELLS: [&str; 5] = ["bash", "zsh", "sh", "dash", "fish"];

const POLL: Duration = Duration::from_millis(250);

/// Lines read back from the session, enough for the output of the commands meant for it.
const SCROLLBACK: &str = "2000";

pub fn run(name: &str, command: &[&str], timeout: Duration) -> io::Result<i32> {
    let command = command.join(" ");
    let shell = zmx::list()?
        .into_iter()
        .find(|session| session.name == name)
        .ok_or_else(|| io::Error::other(format!("no session {name}: start it with skiff new {name}")))?
        .pid
        .ok_or_else(|| io::Error::other(format!("session {name} has no shell")))?;
    let front = foreground(shell)
        .ok_or_else(|| io::Error::other(format!("can't read the terminal of {name}")))?;
    if !SHELLS.contains(&front.name.as_str()) {
        return Err(io::Error::other(format!(
            "{name} is busy with {} ({}): wait for it or ask the user",
            front.name, front.user
        )));
    }
    eprintln!("[{name}] {} at {}", front.user, front.name);
    show(name)?;

    let token = format!("skiff-{:x}", session::now_ms() ^ u64::from(std::process::id()) << 20);
    // the line typed shows "<token> $?", the line printed "<token>:<code>": only the second ends it
    zmx::send(name, &format!("{command}; printf '\\n%s:%s\\n' {token} $?\r"))?;

    let started = Instant::now();
    let mut asked = false;
    loop {
        std::thread::sleep(POLL);
        let screen = zmx::history(name, SCROLLBACK)?;
        if let Some((output, code)) = finished(&screen, &token) {
            print!("{output}");
            return Ok(code);
        }
        if !asked && session::input_mode(Some(shell)) == Some("password") {
            asked = true;
            eprintln!("[{name}] waiting for the password");
        }
        if started.elapsed() > timeout {
            return Err(io::Error::other(format!(
                "{name}: no end after {} s; the command may still run there",
                timeout.as_secs()
            )));
        }
    }
}

/// Types [text] into the session as it is (`$'\r'` is Enter, `$'\x03'` Ctrl+C) and shows it.
pub fn send(name: &str, text: &str) -> io::Result<()> {
    if !zmx::list()?.iter().any(|session| session.name == name) {
        return Err(io::Error::other(format!("no session {name}: start it with skiff new {name}")));
    }
    zmx::send(name, text)?;
    show(name)
}

/// Prints the session's screen, with up to [lines] rows above it.
pub fn read(name: &str, lines: u32) -> io::Result<()> {
    print!("{}", zmx::history(name, &lines.to_string())?);
    Ok(())
}

/// Shows the session, starting it first if it ended (an `exit` there ends it).
pub fn show_or_start(name: &str) -> io::Result<()> {
    if !zmx::list()?.iter().any(|session| session.name == name) {
        zmx::start(name)?;
    }
    show(name)
}

/// Puts the session before the user: in the panel, and in the clients that follow `shown`.
pub fn show(name: &str) -> io::Result<()> {
    let _ = panel::show(name);
    zmx::set(name, &[("shown", &session::now_ms().to_string())])?;
    crate::store::mark_changed();
    Ok(())
}

/// The output of the command and its exit code, once the marker [token] is on [screen]: the lines
/// between the one typed (the last holding "<token> $?") and the one printed ("<token>:<code>").
fn finished(screen: &str, token: &str) -> Option<(String, i32)> {
    let lines = screen.lines().collect::<Vec<_>>();
    let end = lines.iter().rposition(|line| {
        line.trim_end()
            .strip_prefix(&format!("{token}:"))
            .is_some_and(|code| code.parse::<i32>().is_ok())
    })?;
    let code = lines[end].trim_end()[token.len() + 1..].parse().ok()?;
    let typed = format!("{token} $?");
    let start = lines[..end].iter().rposition(|line| line.contains(&typed)).map_or(0, |i| i + 1);
    // printf starts with a newline, so output without one still ends its line
    let body = &lines[start..end];
    let body = body.strip_suffix(&[""]).unwrap_or(body);
    let mut output = body.join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    Some((output, code))
}

struct Foreground {
    name: String,
    user: String,
}

/// The process in front on the terminal of [shell] (its foreground process group), and its user.
fn foreground(shell: u32) -> Option<Foreground> {
    let stat = std::fs::read_to_string(format!("/proc/{shell}/stat")).ok()?;
    // "pid (comm) state ppid pgrp session tty_nr tpgid ..."
    let tpgid: u32 = stat[stat.rfind(')')? + 1..].split_whitespace().nth(5)?.parse().ok()?;
    let status = std::fs::read_to_string(format!("/proc/{tpgid}/status")).ok()?;
    let field = |key: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .map(|value| value.trim().to_owned())
    };
    let uid: u32 = field("Uid:")?.split_whitespace().next()?.parse().ok()?;
    Some(Foreground {
        name: field("Name:")?,
        user: user_name(uid).unwrap_or_else(|| uid.to_string()),
    })
}

fn user_name(uid: u32) -> Option<String> {
    std::fs::read_to_string("/etc/passwd").ok()?.lines().find_map(|line| {
        let fields = line.split(':').collect::<Vec<_>>();
        (fields.get(2)?.parse::<u32>().ok()? == uid).then(|| fields[0].to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_output_between_the_typed_line_and_the_marker() {
        let screen = "old\n$ ls; printf '\\n%s:%s\\n' skiff-1 $?\na\nb\n\nskiff-1:0\n$ \n";
        assert_eq!(finished(screen, "skiff-1"), Some(("a\nb\n".to_owned(), 0)));
    }

    #[test]
    fn keeps_the_exit_code_and_ignores_the_typed_marker() {
        let typed = "$ false; printf '\\n%s:%s\\n' skiff-2 $?\n";
        assert_eq!(finished(typed, "skiff-2"), None);
        let screen = format!("{typed}\nskiff-2:1\n");
        assert_eq!(finished(&screen, "skiff-2"), Some((String::new(), 1)));
    }
}
