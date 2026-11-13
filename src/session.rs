//! A session as skiff shows it: the zmx session, its space and the state of the agent in it.

use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::store::Spaces;
use crate::zmx;

pub const STATES: [&str; 4] = ["idle", "working", "asking", "ready"];

#[derive(Clone, Serialize, PartialEq)]
pub struct Session {
    pub name: String,
    pub space: Option<String>,
    /// The name it was given since it was created, if any.
    pub title: Option<String>,
    /// idle, working, asking or ready.
    pub state: String,
    /// When the state last changed, in Unix milliseconds; unknown until the agent reports one.
    pub since: Option<u64>,
    pub cwd: Option<String>,
    /// "password" while the program in it reads a line without showing it ([input_mode]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<&'static str>,
    /// The session's shell, whose terminal [input_mode] reads.
    #[serde(skip)]
    pub pid: Option<u32>,
}

pub fn sessions() -> std::io::Result<Vec<Session>> {
    let spaces = Spaces::load();
    let mut sessions = zmx::list()?
        .into_iter()
        .map(|session| Session {
            space: spaces.sessions.get(&session.name).cloned(),
            title: spaces.titles.get(&session.name).cloned(),
            state: session
                .label("state")
                .filter(|state| STATES.contains(state))
                .unwrap_or("idle")
                .to_owned(),
            since: session.label("since").and_then(|since| since.parse().ok()),
            cwd: session.cwd.clone(),
            input: input_mode(session.pid),
            pid: session.pid,
            name: session.name,
        })
        .collect::<Vec<_>>();
    let position = |name: &str| {
        spaces
            .order
            .iter()
            .position(|n| n == name)
            .unwrap_or(usize::MAX)
    };
    let space_position = |space: &Option<String>| {
        space.as_ref().map_or(0, |space| {
            spaces
                .space_order
                .iter()
                .position(|s| s == space)
                .unwrap_or(usize::MAX)
        })
    };
    // those without a space first, then each space in its arranged order
    sessions.sort_by_cached_key(|s| {
        (
            s.space.is_some(),
            space_position(&s.space),
            s.space.clone(),
            position(&s.name),
            s.name.clone(),
        )
    });
    Ok(sessions)
}

/// "password" while the program in the terminal of the shell [pid] reads a line without echoing
/// it, as a password prompt does: echo off with line mode on (a shell at its prompt or a
/// full-screen program turns both off). None otherwise, or when the terminal cannot be read.
pub fn input_mode(pid: Option<u32>) -> Option<&'static str> {
    let tty = std::fs::read_link(format!("/proc/{}/fd/0", pid?)).ok()?;
    if !tty.starts_with("/dev/pts/") {
        return None;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(&tty)
        .ok()?;
    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr only writes into the buffer it is given
    if unsafe { libc::tcgetattr(file.as_raw_fd(), termios.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: tcgetattr succeeded, so it filled the buffer
    let flags = unsafe { termios.assume_init() }.c_lflag;
    (flags & libc::ECHO == 0 && flags & libc::ICANON != 0).then_some("password")
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Records a session's state, unless it already has it or, given `from`, is in none of those.
pub fn set_state(session: &str, state: &str, from: Option<&[&str]>) -> std::io::Result<()> {
    let current = zmx::get(session, "state");
    let current = current.as_deref().unwrap_or("idle");
    if current == state || from.is_some_and(|from| !from.contains(&current)) {
        return Ok(());
    }
    zmx::set(
        session,
        &[("state", state), ("since", &now_ms().to_string())],
    )?;
    crate::store::mark_changed();
    Ok(())
}

/// A finished agent stays ready until someone looks at its session.
pub fn mark_seen(session: &str) -> std::io::Result<()> {
    if zmx::get(session, "state").as_deref() == Some("ready") {
        set_state(session, "idle", None)?;
    }
    Ok(())
}

impl Session {
    /// The name given to it, or else its zmx name.
    pub fn shown_name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.name)
    }
}

/// The zmx name for a session called [title]: zmx names become socket files, so accents go, and
/// anything but letters, digits, dot, dash and underscore becomes a dash. A name already in [taken]
/// gets a number. Termarchy derives its names the same way.
pub fn session_id(title: &str, taken: &[&str]) -> String {
    use unicode_normalization::UnicodeNormalization;
    use unicode_normalization::char::is_combining_mark;
    let mut base = String::new();
    for c in title.trim().nfd().filter(|c| !is_combining_mark(*c)) {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            base.push(c);
        } else if !base.ends_with('-') {
            base.push('-');
        }
    }
    let base = base.trim_matches(['-', '.']);
    let base = if base.is_empty() { "session" } else { base };
    if !taken.contains(&base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken.contains(&candidate.as_str()))
        .unwrap_or_default()
}

/// Names the session, and the Claude conversation running in it, which shows the name on its
/// prompt bar. Claude queues the command while it is answering and runs it after.
pub fn rename(name: &str, title: &str) -> std::io::Result<()> {
    let mut spaces = crate::store::Spaces::load();
    if title == "-" || title == name {
        spaces.titles.remove(name);
    } else {
        spaces.titles.insert(name.to_owned(), title.to_owned());
    }
    spaces.save()?;
    let shell = crate::zmx::list()?
        .into_iter()
        .find(|session| session.name == name)
        .and_then(|session| session.pid);
    if shell.is_some_and(claude_runs_under) {
        let shown = if title == "-" { name } else { title };
        crate::zmx::send(name, &format!("/rename {shown}\r"))?;
    }
    Ok(())
}

/// Whether a `claude` process descends from [shell].
fn claude_runs_under(shell: u32) -> bool {
    let processes = crate::procs::Processes::read();
    processes
        .iter()
        .filter(|(_, process)| process.name == "claude")
        .any(|(pid, _)| {
            processes
                .ancestors(pid)
                .any(|(ancestor, _)| ancestor == shell)
        })
}

/// Puts [name] in [space] (None: no space) and sets the order of the sessions to [order], as a
/// drag in the list leaves them.
pub fn arrange(name: &str, space: Option<&str>, order: Vec<String>) -> std::io::Result<()> {
    let mut spaces = crate::store::Spaces::load();
    match space {
        Some(space) => spaces.sessions.insert(name.to_owned(), space.to_owned()),
        None => spaces.sessions.remove(name),
    };
    spaces.order = order;
    spaces.save()
}

/// Gives space [old] the name [new]: its sessions, and its place among the spaces, go with it.
pub fn rename_space(old: &str, new: &str) -> std::io::Result<()> {
    let mut spaces = crate::store::Spaces::load();
    for space in spaces.sessions.values_mut() {
        if space == old {
            *space = new.to_owned();
        }
    }
    for space in spaces.space_order.iter_mut().chain(&mut spaces.collapsed) {
        if space == old {
            *space = new.to_owned();
        }
    }
    spaces.save()
}

/// Folds [space] when it is open, and opens it when folded; returns the spaces folded now.
pub fn toggle_collapsed(space: &str) -> std::io::Result<Vec<String>> {
    let mut spaces = crate::store::Spaces::load();
    if let Some(at) = spaces.collapsed.iter().position(|s| s == space) {
        spaces.collapsed.remove(at);
    } else {
        spaces.collapsed.push(space.to_owned());
    }
    spaces.save()?;
    Ok(spaces.collapsed)
}

/// Folds [space], or opens it.
pub fn set_collapsed(space: &str, collapsed: bool) -> std::io::Result<()> {
    let mut spaces = crate::store::Spaces::load();
    spaces.collapsed.retain(|s| s != space);
    if collapsed {
        spaces.collapsed.push(space.to_owned());
    }
    spaces.save()
}

/// Sets the order of the spaces, as a drag in the list leaves them.
pub fn arrange_spaces(order: Vec<String>) -> std::io::Result<()> {
    let mut spaces = crate::store::Spaces::load();
    spaces.space_order = order;
    spaces.save()
}

pub fn move_to(name: &str, space: &str) -> std::io::Result<()> {
    let mut spaces = crate::store::Spaces::load();
    if space == "-" {
        spaces.sessions.remove(name);
    } else {
        spaces.sessions.insert(name.to_owned(), space.to_owned());
    }
    spaces.save()
}

pub fn kill(name: &str) -> std::io::Result<()> {
    if !crate::zmx::kill(name)? {
        return Err(std::io::Error::other(format!("could not kill {name}")));
    }
    let mut spaces = crate::store::Spaces::load();
    let had_space = spaces.sessions.remove(name).is_some();
    let had_title = spaces.titles.remove(name).is_some();
    let placed = spaces.order.len();
    spaces.order.retain(|n| n != name);
    if had_space || had_title || spaces.order.len() != placed {
        spaces.save()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::session_id;

    #[test]
    fn derives_zmx_names_as_termarchy_does() {
        assert_eq!(session_id("backup", &[]), "backup");
        assert_eq!(session_id("  Sessão de teste ", &[]), "Sessao-de-teste");
        assert_eq!(session_id("lira", &["lira", "lira-2"]), "lira-3");
        assert_eq!(session_id("✓✓", &[]), "session");
    }
}
