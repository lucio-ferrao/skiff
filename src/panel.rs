//! A pane beside the list that shows the session chosen in it: `skiff panel` runs in a pane split
//! off by hand and listens on a socket; `skiff ui` sends it a session's name, and the panel swaps
//! its zmx client for one attached to that session. The panel started last takes the socket.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::mpsc;
use std::time::Duration;

use crate::{desktop, session, store};

const WAITING: &str =
    "\x1b[2J\x1b[H\x1b[38;2;108;112;134mskiff panel: choose a session in skiff ui\x1b[0m\r\n";

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

    let mut out = io::stdout();
    out.write_all(WAITING.as_bytes())?;
    out.flush()?;
    let mut client: Option<Child> = None;
    loop {
        match chosen.recv_timeout(Duration::from_millis(100)) {
            Ok(name) => {
                if let Some(mut old) = client.take() {
                    // zmx restores the terminal on its way out
                    unsafe { libc::kill(old.id() as libc::pid_t, libc::SIGTERM) };
                    old.wait()?;
                }
                session::mark_seen(&name)?;
                let child = Command::new("zmx").args(["attach", &name]).spawn()?;
                let pid = child.id();
                std::thread::spawn(move || desktop::lead_client(pid, Duration::from_secs(5)));
                client = Some(child);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        // detached (ctrl+\) or the session ended: wait for the next one
        if let Some(child) = &mut client
            && child.try_wait()?.is_some()
        {
            client = None;
            out.write_all(WAITING.as_bytes())?;
            out.flush()?;
        }
    }
}
