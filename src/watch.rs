//! `skiff watch`: the whole list as one JSON line, and again after every change.
//!
//! Changes are noticed by inotify, not polling: zmx creates and removes a socket per session,
//! skiff touches a file whenever it changes a session's labels, and spaces live in a file.
//!
//! Started by systemd for each connection to its socket, it ends as soon as the reader hangs up,
//! not at the next change it fails to write.

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::time::Duration;

use inotify::{Inotify, WatchMask};

use crate::session::{self, Session, sessions};
use crate::store;
use crate::zmx;

/// Changes that arrive together, like a hook setting a state while a session starts, are
/// reported once.
const SETTLE: Duration = Duration::from_millis(30);

/// How often the terminals are read for a password prompt, which nothing announces.
const INPUT_POLL: Duration = Duration::from_millis(150);

#[derive(serde::Serialize)]
struct Snapshot<'a> {
    sessions: &'a [Session],
    /// Spaces shown folded, their sessions hidden.
    collapsed: &'a [String],
}

pub fn watch() -> io::Result<()> {
    let stdout = io::stdout();
    follow(stdin_is_socket(), |sessions, collapsed| {
        let mut out = stdout.lock();
        serde_json::to_writer(&mut out, &Snapshot { sessions, collapsed })?;
        out.write_all(b"\n")?;
        out.flush()?;
        Ok(true)
    })
}

/// Calls [on_change] with the list and the folded spaces, and again whenever they change, until it
/// returns false or, with [reader_socket], the reader on stdin hangs up.
pub fn follow(
    reader_socket: bool,
    mut on_change: impl FnMut(&[Session], &[String]) -> io::Result<bool>,
) -> io::Result<()> {
    let socket_dir = zmx::socket_dir();
    let runtime_dir = store::runtime_dir();
    let config_dir = store::config_dir();
    for dir in [&socket_dir, &runtime_dir, &config_dir] {
        std::fs::create_dir_all(dir)?;
    }
    let mut inotify = Inotify::init()?;
    let mask = WatchMask::CREATE | WatchMask::DELETE | WatchMask::MOVED_TO | WatchMask::CLOSE_WRITE;
    for dir in [&socket_dir, &runtime_dir, &config_dir] {
        inotify.watches().add(dir, mask)?;
    }

    let mut last: Option<(Vec<Session>, Vec<String>)> = None;
    let mut current = (sessions()?, store::Spaces::load().collapsed);
    let mut buffer = [0u8; 4096];
    loop {
        if last.as_ref() != Some(&current) {
            if !on_change(&current.0, &current.1)? {
                return Ok(());
            }
            last = Some(current.clone());
        }
        match wait_for_change(&inotify, reader_socket, INPUT_POLL)? {
            Wait::Gone => return Ok(()),
            Wait::Changed => {
                std::thread::sleep(SETTLE);
                // drain what arrived meanwhile; one listing covers it all
                while inotify
                    .read_events(&mut buffer)
                    .is_ok_and(|events| events.count() > 0)
                {}
                current = (sessions()?, store::Spaces::load().collapsed);
            }
            // nothing changed but maybe a terminal's mode: read only those
            Wait::Timeout => {
                for session in &mut current.0 {
                    session.input = session::input_mode(session.pid);
                }
            }
        }
    }
}

enum Wait {
    Changed,
    Timeout,
    /// The reader on the socket hung up.
    Gone,
}

/// Whether systemd handed us a connection: then stdin is the socket, and its end is the reader's.
fn stdin_is_socket() -> bool {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat only writes into the buffer it is given
    unsafe {
        libc::fstat(0, stat.as_mut_ptr()) == 0
            && stat.assume_init().st_mode & libc::S_IFMT == libc::S_IFSOCK
    }
}

/// Waits for inotify to have events, at most [timeout], or for the reader on the socket to hang up.
fn wait_for_change(inotify: &Inotify, reader_socket: bool, timeout: Duration) -> io::Result<Wait> {
    let mut fds = [
        libc::pollfd {
            fd: inotify.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: 0,
            events: libc::POLLIN | libc::POLLRDHUP,
            revents: 0,
        },
    ];
    let count = if reader_socket { 2 } else { 1 };
    loop {
        // SAFETY: fds holds `count` valid pollfd entries
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), count, timeout.as_millis() as i32) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if ready == 0 {
            return Ok(Wait::Timeout);
        }
        // the reader never writes, so anything on the socket means it is gone
        if reader_socket && fds[1].revents != 0 {
            return Ok(Wait::Gone);
        }
        if fds[0].revents & libc::POLLIN != 0 {
            return Ok(Wait::Changed);
        }
    }
}
