//! The Ghostty windows that show sessions, and bringing one to the front. Ghostty runs one process
//! per window here (gtk-single-instance off), and a session's `zmx attach` runs inside the window
//! showing it, so the window is found by process, without marking its title or class.

use std::io;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::procs::{Process, Processes};

/// The session a zmx client shows, when [process] is one.
fn attached_session(process: &Process) -> Option<&str> {
    match process.args.as_slice() {
        [zmx, attach, session, ..]
            if zmx.rsplit('/').next() == Some("zmx") && attach == "attach" =>
        {
            Some(session.as_str())
        }
        _ => None,
    }
}

/// The zmx client of [session] inside a Ghostty window, with that window's Ghostty process. The
/// lowest pid wins, so every call picks the same window when there are several.
fn window_client(processes: &Processes, session: &str) -> Option<(u32, u32)> {
    processes
        .iter()
        .filter(|(_, process)| attached_session(process) == Some(session))
        .filter_map(|(pid, _)| {
            processes
                .ancestors(pid)
                .find(|(_, ancestor)| ancestor.name == "ghostty")
                .map(|(ghostty, _)| (pid, ghostty))
        })
        .min_by_key(|(pid, _)| *pid)
}

/// The Ghostty process of a window showing [session].
pub fn window_of(session: &str) -> Option<u32> {
    window_client(&Processes::read(), session).map(|(_, ghostty)| ghostty)
}

/// Makes the zmx client in the window of [session] the session's leader, so the session takes
/// that window's size and the program repaints for it. Waits up to [wait] for the window to start.
/// A client that does not catch SIGUSR1 yet (still starting, or an older zmx) is left alone: the
/// signal would end it.
pub fn lead(session: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        if let Some((client, _)) = window_client(&Processes::read(), session) {
            if catches_usr1(client) {
                unsafe { libc::kill(client as libc::pid_t, libc::SIGUSR1) };
                return;
            }
        }
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Makes zmx client [pid] its session's leader, as [lead] does, once it catches SIGUSR1.
pub fn lead_client(pid: u32, wait: Duration) {
    let deadline = Instant::now() + wait;
    while !catches_usr1(pid) {
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGUSR1) };
}

/// Whether process [pid] has a handler for SIGUSR1, from the SigCgt mask in /proc.
fn catches_usr1(pid: u32) -> bool {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return false;
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix("SigCgt:"))
        .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
        .is_some_and(|mask| mask & (1 << (libc::SIGUSR1 - 1)) != 0)
}

/// The sessions shown in the window of Ghostty process [window].
pub fn sessions_in(window: u32) -> Vec<String> {
    let processes = Processes::read();
    processes
        .iter()
        .filter_map(|(pid, process)| Some((pid, attached_session(process)?)))
        .filter(|(pid, _)| {
            processes
                .ancestors(*pid)
                .any(|(ancestor, _)| ancestor == window)
        })
        .map(|(_, session)| session.to_owned())
        .collect()
}

/// Brings the window showing [session] to the front, or opens one of its own in Ghostty: never a
/// window or pane already in use. Run in the graphical session (see [`crate::run::show`]).
pub fn open_window(session: &str) -> io::Result<()> {
    if let Some(window) = window_of(session)
        && focus(window)?
    {
        lead(session, Duration::from_millis(500));
        return Ok(());
    }
    let skiff = std::env::current_exe()?;
    let status = std::process::Command::new("setsid")
        .arg("-f")
        .arg("ghostty")
        .arg("-e")
        .arg(&skiff)
        .args(["open", session])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if !status.success() {
        return Err(io::Error::other("could not start ghostty"));
    }
    lead(session, Duration::from_secs(5));
    Ok(())
}

/// Brings the window of process [pid] to the front. False where no compositor is supported, so
/// the caller opens another window instead.
pub fn focus(pid: u32) -> io::Result<bool> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if desktop.split(':').any(|name| name == "KDE") {
        kwin_focus(pid)?;
        return Ok(true);
    }
    Ok(false)
}

/// Runs a one-off KWin script that activates the window, through KWin's scripting interface.
fn kwin_focus(pid: u32) -> io::Result<()> {
    const NAME: &str = "skiff-focus";
    let dir = crate::store::runtime_dir();
    std::fs::create_dir_all(&dir)?;
    let script = dir.join("focus.js");
    std::fs::write(
        &script,
        format!(
            "for (const w of workspace.windowList()) {{\n\
             \x20   if (w.pid === {pid} && w.normalWindow) {{\n\
             \x20       w.minimized = false;\n\
             \x20       workspace.activeWindow = w;\n\
             \x20       break;\n\
             \x20   }}\n\
             }}\n"
        ),
    )?;
    // a script left loaded by an earlier call would keep the name taken
    qdbus(&["/Scripting", "org.kde.kwin.Scripting.unloadScript", NAME])?;
    let id = qdbus(&[
        "/Scripting",
        "org.kde.kwin.Scripting.loadScript",
        &script.to_string_lossy(),
        NAME,
    ])?;
    let id = id.trim();
    if id.parse::<i32>().is_err() || id.starts_with('-') {
        return Err(io::Error::other(format!(
            "KWin did not load the focus script ({id})"
        )));
    }
    qdbus(&[&format!("/Scripting/Script{id}"), "org.kde.kwin.Script.run"])?;
    qdbus(&["/Scripting", "org.kde.kwin.Scripting.unloadScript", NAME])?;
    Ok(())
}

fn qdbus(args: &[&str]) -> io::Result<String> {
    let output = Command::new("qdbus6")
        .arg("org.kde.KWin")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
