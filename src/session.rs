//! A session as skiff shows it: the zmx session, its space and the state of the agent in it.

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
            name: session.name,
        })
        .collect::<Vec<_>>();
    sessions.sort_by(|a, b| (&a.space, &a.name).cmp(&(&b.space, &b.name)));
    Ok(sessions)
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
    if spaces.titles.remove(name).is_some() || had_space {
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
