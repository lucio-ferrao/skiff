//! The Ghostty windows that show sessions, and bringing one to the front. Ghostty runs one process
//! per window here (gtk-single-instance off), and a session's `zmx attach` runs inside the window
//! showing it, so the window is found by process, without marking its title or class.

use std::io;
use std::process::{Command, Stdio};

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

/// The Ghostty process of a window showing [session].
pub fn window_of(session: &str) -> Option<u32> {
    let processes = Processes::read();
    processes
        .iter()
        .filter(|(_, process)| attached_session(process) == Some(session))
        .find_map(|(pid, _)| {
            processes
                .ancestors(pid)
                .find(|(_, ancestor)| ancestor.name == "ghostty")
                .map(|(ghostty, _)| ghostty)
        })
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
