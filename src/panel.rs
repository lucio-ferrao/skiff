//! A pane beside the list that shows the session chosen in it: `skiff panel` runs in a pane split
//! off by hand and listens on a socket; `skiff ui` sends it a session's name, and the panel swaps
//! its zmx client for one attached to that session. The panel started last takes the socket. It
//! starts on the first session of the list, goes to the first one left when its session ends, and
//! ends with ctrl+\.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use crate::{desktop, session, store, zmx};

const WAITING: &str =
    "\x1b[2J\x1b[H\x1b[38;2;108;112;134mskiff panel: choose a session in skiff ui\x1b[0m\r\n";

/// Ctrl+\ while waiting: the terminal sends SIGQUIT, which would end the panel with a core dump.
static QUIT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_quit(_: libc::c_int) {
    QUIT.store(true, Ordering::SeqCst);
}

fn socket_path() -> PathBuf {
    store::runtime_dir().join("panel.sock")
}

/// Shows [session] in the running panel. False when there is none.
pub fn show(session: &str) -> io::Result<bool> {
    let mut stream = match UnixStream::connect(socket_path()) {
        Ok(stream) => stream,
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(false);
        }
        Err(err) => return Err(err),
    };
    stream.write_all(format!("{session}\n").as_bytes())?;
    Ok(true)
}

pub fn panel() -> io::Result<()> {
    // zmx would move this terminal to the chosen session instead of attaching inside it
    if std::env::var_os("ZMX_SESSION").is_some_and(|current| !current.is_empty()) {
        return Err(io::Error::other("skiff panel runs outside a session"));
    }
    let path = socket_path();
    std::fs::create_dir_all(store::runtime_dir())?;
    // a panel already running gives the socket up to this one
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;

    let (requests, chosen) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut line = String::new();
            if BufReader::new(stream).read_line(&mut line).is_ok() {
                let name = line.trim();
                if !name.is_empty() && requests.send(name.to_owned()).is_err() {
                    return;
                }
            }
        }
    });

    // attached, zmx reads ctrl+\ itself (raw mode) and detaches; waiting, it ends the panel
    unsafe { libc::signal(libc::SIGQUIT, on_quit as *const () as libc::sighandler_t) };
    let mut out = io::stdout();
    // the first session of the list right away, so the panel is never empty while there is one
    let mut client = first_session()?.map(|name| attach(&name)).transpose()?;
    if client.is_none() {
        out.write_all(WAITING.as_bytes())?;
        out.flush()?;
    }
    loop {
        match chosen.recv_timeout(Duration::from_millis(100)) {
            Ok(name) => {
                if let Some((_, mut old)) = client.take() {
                    // zmx restores the terminal on its way out
                    unsafe { libc::kill(old.id() as libc::pid_t, libc::SIGTERM) };
                    old.wait()?;
                }
                client = Some(attach(&name)?);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if QUIT.swap(false, Ordering::SeqCst) && client.is_none() {
                    return Ok(());
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        if let Some((name, child)) = &mut client
            && child.try_wait()?.is_some()
        {
            // the session still there: detached with ctrl+\, which ends the panel
            if zmx::list()?.iter().any(|s| s.name == *name) {
                return Ok(());
            }
            // it ended: the first one left, or wait for one
            client = first_session()?.map(|name| attach(&name)).transpose()?;
            if client.is_none() {
                out.write_all(WAITING.as_bytes())?;
                out.flush()?;
            }
        }
    }
}

/// The first session as skiff ui lists them: those without a space first.
fn first_session() -> io::Result<Option<String>> {
    let mut sessions = session::sessions()?;
    sessions.sort_by_key(|s| s.space.is_some());
    Ok(sessions.into_iter().next().map(|s| s.name))
}

/// A zmx client attached to [name] in this terminal, the session marked as seen.
fn attach(name: &str) -> io::Result<(String, Child)> {
    session::mark_seen(name)?;
    let child = Command::new("zmx").args(["attach", name]).spawn()?;
    let pid = child.id();
    std::thread::spawn(move || desktop::lead_client(pid, Duration::from_secs(5)));
    Ok((name.to_owned(), child))
}
