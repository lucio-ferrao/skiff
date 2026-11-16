//! skiff: persistent terminal sessions on zmx, grouped in spaces, with the live state of the
//! agent in each.

mod desktop;
mod panel;
mod procs;
mod run;
mod session;
mod store;
mod ui;
mod watch;
mod zmx;

use std::process::ExitCode;

const USAGE: &str = "\
usage: skiff [<session>]                 open a session (no session: the session picker)
       skiff new <session> [--space <space>] [command...]
       skiff open <session>
       skiff list [--json]
       skiff move <session> <space>        (space \"-\" leaves it without one)
       skiff rename <session> <title>      (title \"-\" goes back to the session's name)
       skiff kill <session>
       skiff collapse <space>              fold a space, hiding its sessions
       skiff expand <space>
       skiff state <idle|working|asking|ready> [--from <state,...>]
                                           for the session in $ZMX_SESSION
       skiff seen <session>
       skiff run [--timeout <seconds>] <session> <command...>
                                           type a command into the session and wait for it
       skiff send <session> <text>         type keys into the session as they are
       skiff read <session> [--lines <n>]  the session's screen, with n rows above it
       skiff show <session>                show the session in skiff panel and floating in Termarchy
                                           (run and send show it too)
       skiff seen-window <pid>             the sessions in the Ghostty window of that process
       skiff watch
       skiff ui
       skiff panel                         show here the session chosen in skiff ui
       skiff master                        skiff ui without the screens, beside skiff panel";

fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    // the exit code of a command run in a session is skiff's
    if let ["run", rest @ ..] = args.as_slice() {
        let (timeout, rest) = match rest {
            ["--timeout", seconds, rest @ ..] => (seconds.parse().ok(), rest),
            rest => (Some(600), rest),
        };
        return match (timeout, rest) {
            (Some(timeout), [session, command @ ..]) if !command.is_empty() => {
                match run::run(session, command, std::time::Duration::from_secs(timeout)) {
                    Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
                    Err(err) => {
                        eprintln!("skiff: {err}");
                        ExitCode::FAILURE
                    }
                }
            }
            _ => {
                eprintln!("{USAGE}");
                ExitCode::FAILURE
            }
        };
    }
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        // a watcher whose reader went away just ends
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("skiff: {err}");
            ExitCode::FAILURE
        }
    }
}

fn usage_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, USAGE)
}

fn run(args: &[&str]) -> std::io::Result<()> {
    match args {
        [] | ["ui"] => ui::ui(false),
        ["-h" | "--help" | "help"] => {
            println!("{USAGE}");
            Ok(())
        }
        ["list"] => list(false),
        ["list", "--json"] => list(true),
        ["new", name, rest @ ..] => new(name, rest),
        ["open", name] => open(name),
        ["move", name, space] => session::move_to(name, space),
        ["rename", name, title] => session::rename(name, title),
        ["kill", name] => session::kill(name),
        ["collapse", space] => session::set_collapsed(space, true),
        ["expand", space] => session::set_collapsed(space, false),
        ["state", value] => state(value, None),
        ["state", value, "--from", from] => state(value, Some(from)),
        ["seen", name] => session::mark_seen(name),
        ["send", name, text] => run::send(name, text),
        ["show", name] => run::show(name),
        ["read", name] => run::read(name, 0),
        ["read", name, "--lines", lines] => run::read(name, lines.parse().map_err(|_| usage_error())?),
        ["seen-window", pid] => seen_window(pid),
        ["watch"] => watch::watch(),
        ["panel"] => panel::panel(),
        ["master"] => ui::ui(true),
        [name] if !name.starts_with('-') => open(name),
        _ => Err(usage_error()),
    }
}

fn list(json: bool) -> std::io::Result<()> {
    let sessions = session::sessions()?;
    if json {
        println!("{}", serde_json::to_string(&sessions)?);
        return Ok(());
    }
    let width = sessions
        .iter()
        .map(|s| s.title.as_deref().unwrap_or(&s.name).chars().count())
        .max()
        .unwrap_or(0);
    let mut space = None;
    for session in &sessions {
        if space != Some(&session.space) {
            println!("{}", session.space.as_deref().unwrap_or("(no space)"));
            space = Some(&session.space);
        }
        let shown = session.title.as_deref().unwrap_or(&session.name);
        println!("  {shown:width$}  {}", session.state);
    }
    Ok(())
}

fn new(name: &str, rest: &[&str]) -> std::io::Result<()> {
    let (space, command) = match rest {
        ["--space", space, command @ ..] => (Some(*space), command),
        command => (None, command),
    };
    if let Some(space) = space {
        let mut spaces = store::Spaces::load();
        spaces.sessions.insert(name.to_owned(), space.to_owned());
        spaces.save()?;
    }
    let command = command
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect::<Vec<_>>();
    Err(zmx::attach(name, &command))
}

/// Opens a session by its zmx name or, failing that, by the title a live session was given; a
/// name neither matches starts a session. A title left behind by a session that ended never
/// counts, or it would bring that session back.
fn open(name: &str) -> std::io::Result<()> {
    let live = zmx::list()?
        .into_iter()
        .map(|session| session.name)
        .collect::<Vec<_>>();
    let titles = store::Spaces::load().titles;
    let name = if live.iter().any(|session| session == name) {
        name.to_owned()
    } else {
        titles
            .into_iter()
            .find(|(session, title)| title == name && live.contains(session))
            .map_or_else(|| name.to_owned(), |(session, _)| session)
    };
    session::mark_seen(&name)?;
    Err(zmx::attach(&name, &[]))
}

/// Called by the agent's hooks. Outside a zmx session there is nothing to record. With `from`,
/// only a session in one of those states changes.
fn state(state: &str, from: Option<&str>) -> std::io::Result<()> {
    let from = from.map(|from| from.split(',').collect::<Vec<_>>());
    let known = |state: &&str| session::STATES.contains(state);
    if !known(&state) || from.as_ref().is_some_and(|from| !from.iter().all(known)) {
        return Err(usage_error());
    }
    match std::env::var("ZMX_SESSION") {
        Ok(name) if !name.is_empty() => {
            session::set_state(&name, state, from.as_deref())?;
            follow_claude_title(&name)
        }
        _ => Ok(()),
    }
}

/// A conversation renamed with Claude's `/rename` names its session too. The hook hands its
/// event on stdin, with the conversation's transcript, where `/rename` writes a `custom-title`
/// line. Read only when a rename may have happened, since a transcript can be large.
fn follow_claude_title(name: &str) -> std::io::Result<()> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return Ok(());
    }
    let Ok(event) = serde_json::from_reader::<_, serde_json::Value>(std::io::stdin()) else {
        return Ok(());
    };
    let event_name = event["hook_event_name"].as_str().unwrap_or_default();
    if !matches!(event_name, "SessionStart" | "UserPromptSubmit" | "Stop") {
        return Ok(());
    }
    let Some(transcript) = event["transcript_path"].as_str() else {
        return Ok(());
    };
    let Some(title) = last_custom_title(transcript) else {
        return Ok(());
    };
    let mut spaces = store::Spaces::load();
    let current = spaces.titles.get(name).map_or(name, String::as_str);
    if current == title {
        return Ok(());
    }
    if title == name {
        spaces.titles.remove(name);
    } else {
        spaces.titles.insert(name.to_owned(), title);
    }
    spaces.save()
}

fn last_custom_title(transcript: &str) -> Option<String> {
    let text = std::fs::read_to_string(transcript).ok()?;
    text.lines()
        .rev()
        .filter(|line| line.contains("\"custom-title\""))
        .find_map(|line| {
            let entry: serde_json::Value = serde_json::from_str(line).ok()?;
            (entry["type"] == "custom-title")
                .then(|| entry["customTitle"].as_str().map(str::to_owned))
                .flatten()
        })
}

/// Called when a Ghostty window gains the focus: the sessions it shows have been seen.
fn seen_window(pid: &str) -> std::io::Result<()> {
    let pid = pid.parse().map_err(|_| usage_error())?;
    for name in desktop::sessions_in(pid) {
        session::mark_seen(&name)?;
    }
    Ok(())
}
