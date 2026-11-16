//! The zmx sessions underneath: listing them, their labels, attaching and killing.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// A live session as `zmx list` reports it.
pub struct ZmxSession {
    pub name: String,
    /// The session's shell.
    pub pid: Option<u32>,
    pub cwd: Option<String>,
    pub labels: Vec<(String, String)>,
}

impl ZmxSession {
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// The fields `zmx list` prints that are not labels.
const BUILTIN_FIELDS: [&str; 8] = [
    "name",
    "pid",
    "clients",
    "created",
    "cwd",
    "cmd",
    "ended",
    "exit_code",
];

pub fn list() -> io::Result<Vec<ZmxSession>> {
    let output = Command::new("zmx")
        .arg("list")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    Ok(parse_list(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_list(text: &str) -> Vec<ZmxSession> {
    text.lines()
        .filter_map(|line| {
            // inside a session the current one is marked with an arrow, the others indented
            let line = line.trim_start_matches('→').trim_start();
            let mut session = ZmxSession {
                name: String::new(),
                pid: None,
                cwd: None,
                labels: Vec::new(),
            };
            for field in line.split('\t') {
                let (key, value) = field.split_once('=')?;
                match key {
                    // a session that does not answer is skipped
                    "err" => return None,
                    "name" => session.name = value.to_owned(),
                    "pid" => session.pid = value.parse().ok(),
                    "cwd" => session.cwd = Some(path_of(value)),
                    _ if BUILTIN_FIELDS.contains(&key) => {}
                    _ => session.labels.push((key.to_owned(), value.to_owned())),
                }
            }
            (!session.name.is_empty()).then_some(session)
        })
        .collect()
}

/// The shell reports its directory as a `file://host/path` URL (OSC 7); skiff shows the path.
fn path_of(cwd: &str) -> String {
    let Some(rest) = cwd.strip_prefix("file://") else {
        return cwd.to_owned();
    };
    let path = rest.find('/').map_or("/", |start| &rest[start..]);
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = (bytes[index] == b'%')
            .then(|| path.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// The value of a session's label, or None when it is not set.
pub fn get(session: &str, key: &str) -> Option<String> {
    let output = Command::new("zmx")
        .args(["get", session, key])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub fn set(session: &str, labels: &[(&str, &str)]) -> io::Result<()> {
    let status = Command::new("zmx")
        .args(["set", session])
        .args(labels.iter().map(|(k, v)| format!("{k}={v}")))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("zmx set {session} failed")))
    }
}

/// The session's screen as plain text, with up to [scrollback] rows above it.
pub fn history(session: &str, scrollback: &str) -> io::Result<String> {
    let output = Command::new("zmx")
        .args(["history", session, "--scrollback", scrollback])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(io::Error::other(format!("zmx history {session} failed")))
    }
}

/// Types [text] into the session, as if on its keyboard.
pub fn send(session: &str, text: &str) -> io::Result<()> {
    let status = Command::new("zmx")
        .args(["send", session, text])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("zmx send {session} failed")))
    }
}

/// Replaces this process with a zmx client attached to the session, creating it if needed.
pub fn attach(session: &str, command: &[String]) -> io::Error {
    Command::new("zmx")
        .arg("attach")
        .arg(session)
        .args(command)
        .exec()
}

pub fn kill(session: &str) -> io::Result<bool> {
    Ok(Command::new("zmx")
        .args(["kill", session])
        .stdin(Stdio::null())
        .status()?
        .success())
}

/// Where zmx keeps its sockets, one per session: created and removed as sessions come and go.
pub fn socket_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ZMX_DIR") {
        return dir.into();
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("zmx");
    }
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_owned());
    PathBuf::from(format!(
        "{}/zmx-{}",
        tmp.trim_end_matches('/'),
        current_uid()
    ))
}

fn current_uid() -> u32 {
    // the uid of a running process is always readable from /proc
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("Uid:"))
                .and_then(|ids| ids.split_whitespace().next()?.parse().ok())
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_cwd_and_labels_and_skips_unreachable_sessions() {
        let text = "→ name=lira\tpid=10\tclients=1\tcreated=5\tcwd=file://Corsair/home/lef/lira\tstate=working\tsince=7\n  \
                    name=old\terr=ConnectionRefused\tstatus=cleaning up\n  \
                    name=build\tpid=11\tclients=0\tcreated=6\tcmd=cargo build\tended=9\texit_code=0\n";
        let sessions = parse_list(text);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].name, "lira");
        assert_eq!(sessions[0].pid, Some(10));
        assert_eq!(sessions[0].cwd.as_deref(), Some("/home/lef/lira"));
        assert_eq!(sessions[0].label("state"), Some("working"));
        assert_eq!(sessions[0].label("since"), Some("7"));
        assert_eq!(sessions[1].name, "build");
        assert_eq!(sessions[1].cwd, None);
        assert!(sessions[1].labels.is_empty());
    }

    #[test]
    fn reads_the_path_out_of_a_file_url() {
        assert_eq!(
            path_of("file://Corsair/home2/src/skiff"),
            "/home2/src/skiff"
        );
        assert_eq!(path_of("file://Corsair/home/lef/a%20b"), "/home/lef/a b");
        assert_eq!(path_of("/plain/path"), "/plain/path");
    }
}
