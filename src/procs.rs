//! The running processes, read from /proc: who started whom, and with what command.

use std::collections::HashMap;

pub struct Process {
    pub parent: u32,
    /// The command name, as `ps -o comm` shows it.
    pub name: String,
    pub args: Vec<String>,
}

pub struct Processes(HashMap<u32, Process>);

impl Processes {
    pub fn read() -> Processes {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Processes(HashMap::new());
        };
        let table = entries
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
            .filter_map(|pid| {
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                // "pid (comm) state ppid ...": the name may hold spaces and parentheses
                let open = stat.find('(')?;
                let close = stat.rfind(')')?;
                let parent = stat[close + 1..].split_whitespace().nth(1)?.parse().ok()?;
                let args = std::fs::read(format!("/proc/{pid}/cmdline"))
                    .unwrap_or_default()
                    .split(|byte| *byte == 0)
                    .filter(|arg| !arg.is_empty())
                    .map(|arg| String::from_utf8_lossy(arg).into_owned())
                    .collect();
                let name = stat[open + 1..close].to_owned();
                Some((pid, Process { parent, name, args }))
            })
            .collect();
        Processes(table)
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, &Process)> {
        self.0.iter().map(|(pid, process)| (*pid, process))
    }

    /// The ancestors of [pid], nearest first.
    pub fn ancestors(&self, pid: u32) -> impl Iterator<Item = (u32, &Process)> {
        let mut current = pid;
        std::iter::from_fn(move || {
            let parent = self.0.get(&current)?.parent;
            if parent <= 1 {
                return None;
            }
            current = parent;
            Some((parent, self.0.get(&parent)?))
        })
        .take(64)
    }
}
