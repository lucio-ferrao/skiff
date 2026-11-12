//! `skiff ui`: the sessions as cards grouped by space, each with its agent's state and a live look
//! at its screen, as the Termarchy grid shows them. A session opens in a Ghostty window of its own;
//! closing that window leaves the session running.

use std::collections::HashMap;
use std::io;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::desktop;
use crate::session::{self, Session};
use crate::watch;

/// A card is at least this wide; the grid fits as many columns as it can.
const CARD_MIN_WIDTH: u16 = 44;
const CARD_HEIGHT: u16 = 10;
const PREVIEW_INTERVAL: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_millis(100);

// herdr's colors (Catppuccin Mocha), as Termarchy draws the states
const WORKING: Color = Color::Rgb(0xF9, 0xE2, 0xAF);
const ASKING: Color = Color::Rgb(0xF3, 0x8B, 0xA8);
const READY: Color = Color::Rgb(0x94, 0xE2, 0xD5);
const IDLE: Color = Color::Rgb(0xA6, 0xE3, 0xA1);
const DIM: Color = Color::Rgb(0x6C, 0x70, 0x86);
const TEXT: Color = Color::Rgb(0xCD, 0xD6, 0xF4);
const SUBTEXT: Color = Color::Rgb(0xA6, 0xAD, 0xC8);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

enum Message {
    Sessions(Vec<Session>),
    Preview(String, Text<'static>),
}

enum Dialog {
    New {
        title: String,
    },
    Edit {
        name: String,
        title: String,
        space: String,
        field: usize,
    },
    End {
        name: String,
        title: String,
    },
}

struct App {
    sessions: Vec<Session>,
    previews: HashMap<String, Text<'static>>,
    selected: Option<String>,
    filter: String,
    filtering: bool,
    dialog: Option<Dialog>,
    scroll: u16,
    /// Where each card was drawn last, for the mouse and for moving up and down.
    cards: Vec<(String, Rect)>,
    notice: Option<String>,
    started: Instant,
    quit: bool,
}

pub fn ui() -> io::Result<()> {
    let (messages, inbox) = mpsc::channel();
    let list = messages.clone();
    std::thread::spawn(move || {
        let _ = watch::follow(false, |sessions| {
            Ok(list.send(Message::Sessions(sessions.to_vec())).is_ok())
        });
    });
    let (wanted, requests) = mpsc::channel::<Vec<String>>();
    std::thread::spawn(move || {
        while let Ok(mut names) = requests.recv() {
            // only the latest request matters
            while let Ok(newer) = requests.try_recv() {
                names = newer;
            }
            for name in names {
                if let Some(text) = preview(&name)
                    && messages.send(Message::Preview(name, text)).is_err()
                {
                    return;
                }
            }
        }
    });

    let mut terminal = ratatui::init();
    execute!(io::stdout(), EnableMouseCapture)?;
    let result = run(&mut terminal, &inbox, &wanted);
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn run(
    terminal: &mut DefaultTerminal,
    inbox: &mpsc::Receiver<Message>,
    wanted: &mpsc::Sender<Vec<String>>,
) -> io::Result<()> {
    let mut app = App {
        sessions: Vec::new(),
        previews: HashMap::new(),
        selected: None,
        filter: String::new(),
        filtering: false,
        dialog: None,
        scroll: 0,
        cards: Vec::new(),
        notice: None,
        started: Instant::now(),
        quit: false,
    };
    let mut last_preview = Instant::now() - PREVIEW_INTERVAL;
    while !app.quit {
        while let Ok(message) = inbox.try_recv() {
            match message {
                Message::Sessions(sessions) => app.set_sessions(sessions),
                Message::Preview(name, text) => {
                    app.previews.insert(name, text);
                }
            }
        }
        terminal.draw(|frame| app.draw(frame))?;
        if last_preview.elapsed() >= PREVIEW_INTERVAL {
            let _ = wanted.send(app.cards.iter().map(|(name, _)| name.clone()).collect());
            last_preview = Instant::now();
        }
        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.key(key),
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => app.click(mouse.column, mouse.row),
                    MouseEventKind::ScrollDown => app.scroll = app.scroll.saturating_add(3),
                    MouseEventKind::ScrollUp => app.scroll = app.scroll.saturating_sub(3),
                    _ => {}
                },
                _ => {}
            }
        }
    }
    Ok(())
}

impl App {
    fn set_sessions(&mut self, sessions: Vec<Session>) {
        self.sessions = sessions;
        self.previews
            .retain(|name, _| self.sessions.iter().any(|s| &s.name == name));
        let visible = self.visible();
        if !visible
            .iter()
            .any(|s| Some(&s.name) == self.selected.as_ref())
        {
            self.selected = visible.first().map(|s| s.name.clone());
        }
    }

    /// The sessions shown, in grid order: those without a space first, then by space and name.
    fn visible(&self) -> Vec<&Session> {
        let filter = self.filter.to_lowercase();
        let mut shown = self
            .sessions
            .iter()
            .filter(|s| filter.is_empty() || s.shown_name().to_lowercase().contains(&filter))
            .collect::<Vec<_>>();
        shown.sort_by_key(|s| {
            (
                s.space.is_some(),
                s.space.clone(),
                s.shown_name().to_lowercase(),
            )
        });
        shown
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let grid = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
        let status = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
        self.draw_grid(frame, grid);
        self.draw_status(frame, status);
        if self.dialog.is_some() {
            self.draw_dialog(frame, area);
        }
    }

    fn draw_grid(&mut self, frame: &mut Frame, area: Rect) {
        let columns = (area.width / CARD_MIN_WIDTH).max(1);
        let width = area.width / columns;
        // Cards and headings in grid coordinates, before scrolling.
        let mut headings = Vec::new();
        let mut placed = Vec::new();
        let (mut y, mut column) = (0u16, 0u16);
        let mut group: Option<Option<String>> = None;
        let visible = self
            .visible()
            .into_iter()
            .map(|s| (s.name.clone(), s.space.clone()))
            .collect::<Vec<_>>();
        for (name, space) in visible {
            if group.as_ref() != Some(&space) {
                if column > 0 {
                    y += CARD_HEIGHT;
                    column = 0;
                }
                if let Some(heading) = &space {
                    headings.push((y, heading.clone()));
                    y += 1;
                }
                group = Some(space);
            }
            placed.push((name, Rect::new(column * width, y, width, CARD_HEIGHT)));
            column += 1;
            if column == columns {
                column = 0;
                y += CARD_HEIGHT;
            }
        }
        let content_height = if column > 0 { y + CARD_HEIGHT } else { y };
        self.reveal_selected(&placed, area.height);
        self.scroll = self.scroll.min(content_height.saturating_sub(area.height));

        if placed.is_empty() {
            let text = if self.sessions.is_empty() {
                "no sessions yet: n starts one"
            } else {
                "no session matches"
            };
            let empty = Paragraph::new(text)
                .style(Style::new().fg(DIM))
                .alignment(Alignment::Center);
            frame.render_widget(
                empty,
                Rect::new(area.x, area.y + area.height / 2, area.width, 1),
            );
        }
        for (y, heading) in headings {
            if let Some(row) = self.on_screen(area, Rect::new(0, y, area.width, 1)) {
                let line = Line::from(Span::styled(
                    format!(" {heading}"),
                    Style::new().fg(SUBTEXT).bold(),
                ));
                frame.render_widget(line, row);
            }
        }
        self.cards.clear();
        for (name, rect) in placed {
            let Some(rect) = self.on_screen(area, rect) else {
                continue;
            };
            if let Some(session) = self.sessions.iter().find(|s| s.name == name) {
                self.draw_card(frame, rect, session);
            }
            self.cards.push((name, rect));
        }
    }

    /// The card's place on screen, if it is entirely visible.
    fn on_screen(&self, area: Rect, rect: Rect) -> Option<Rect> {
        let top = rect.y.checked_sub(self.scroll)?;
        (top + rect.height <= area.height)
            .then(|| Rect::new(area.x + rect.x, area.y + top, rect.width, rect.height))
    }

    fn reveal_selected(&mut self, placed: &[(String, Rect)], height: u16) {
        let Some(rect) = placed
            .iter()
            .find(|(name, _)| Some(name) == self.selected.as_ref())
            .map(|(_, r)| *r)
        else {
            return;
        };
        // a heading right above the card comes into view with it
        let top = rect.y.saturating_sub(1);
        if top < self.scroll {
            self.scroll = top;
        } else if rect.bottom() > self.scroll + height {
            self.scroll = rect.bottom() - height;
        }
    }

    fn draw_card(&self, frame: &mut Frame, area: Rect, session: &Session) {
        let selected = self.selected.as_deref() == Some(session.name.as_str());
        let (mark, color) = match session.state.as_str() {
            "working" => {
                let frame = (self.started.elapsed().as_millis() / 100) as usize % SPINNER.len();
                (SPINNER[frame], WORKING)
            }
            "asking" => ("×", ASKING),
            "ready" => ("✓", READY),
            _ => ("○", IDLE),
        };
        let title = Line::from(vec![
            Span::raw(" "),
            Span::styled(mark, Style::new().fg(color)),
            Span::raw(" "),
            Span::styled(
                session.shown_name().to_owned(),
                Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]);
        let block = Block::bordered()
            .border_type(BorderType::Plain)
            .border_style(Style::new().fg(if selected { TEXT } else { DIM }))
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if let Some(text) = self.previews.get(&session.name) {
            // the bottom of the screen, where the prompt and the latest output are
            let lines = text.lines.len();
            let shown = usize::from(inner.height).min(lines);
            let tail = Text::from(text.lines[lines - shown..].to_vec());
            frame.render_widget(Paragraph::new(tail), inner);
        }
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect) {
        let line = if self.filtering || !self.filter.is_empty() {
            Line::from(vec![
                Span::styled(" / ", Style::new().fg(DIM)),
                Span::styled(self.filter.clone(), Style::new().fg(TEXT)),
                Span::styled(if self.filtering { "▏" } else { "" }, Style::new().fg(TEXT)),
            ])
        } else if let Some(notice) = &self.notice {
            Line::from(Span::styled(format!(" {notice}"), Style::new().fg(ASKING)))
        } else {
            Line::from(Span::styled(
                " enter open · n new · r name & space · x end · / filter · q quit",
                Style::new().fg(DIM),
            ))
        };
        frame.render_widget(line, area);
    }

    fn draw_dialog(&self, frame: &mut Frame, area: Rect) {
        let Some(dialog) = &self.dialog else { return };
        let field = |label: &str, value: &str, active: bool| {
            Line::from(vec![
                Span::styled(format!("{label:>6}  "), Style::new().fg(DIM)),
                Span::styled(value.to_owned(), Style::new().fg(TEXT)),
                Span::styled(if active { "▏" } else { "" }, Style::new().fg(TEXT)),
            ])
        };
        let (title, lines, hint) = match dialog {
            Dialog::New { title } => (
                " New session ",
                vec![field("Name", title, true)],
                "enter open · esc cancel",
            ),
            Dialog::Edit {
                title,
                space,
                field: active,
                ..
            } => (
                " Session ",
                vec![
                    field("Name", title, *active == 0),
                    field("Space", space, *active == 1),
                ],
                "tab next · enter save · esc cancel",
            ),
            Dialog::End { title, .. } => (
                " End session ",
                vec![Line::from(Span::styled(
                    format!("End {title}? Whatever runs in it stops."),
                    Style::new().fg(TEXT),
                ))],
                "y end · n keep",
            ),
        };
        let width = 56.min(area.width);
        let height = lines.len() as u16 + 4;
        let rect = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let block = Block::bordered()
            .border_type(BorderType::Plain)
            .border_style(Style::new().fg(TEXT))
            .title(title);
        let inner = block.inner(rect);
        frame.render_widget(Clear, rect);
        frame.render_widget(block, rect);
        let mut body = lines;
        body.push(Line::default());
        body.push(Line::from(Span::styled(hint, Style::new().fg(DIM))));
        frame.render_widget(Paragraph::new(body), inner);
    }

    fn key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        self.notice = None;
        if self.dialog.is_some() {
            self.dialog_key(key);
            return;
        }
        if self.filtering {
            match key.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filtering = false;
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) => self.filter.push(c),
                _ => {}
            }
            self.keep_selection_visible();
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => {
                self.filter.clear();
                self.keep_selection_visible();
            }
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Left | KeyCode::Char('h') => self.step(-1),
            KeyCode::Right | KeyCode::Char('l') => self.step(1),
            KeyCode::Up | KeyCode::Char('k') => self.vertical(false),
            KeyCode::Down | KeyCode::Char('j') => self.vertical(true),
            KeyCode::Enter => {
                if let Some(name) = self.selected.clone() {
                    self.open(&name);
                }
            }
            KeyCode::Char('n') => {
                self.dialog = Some(Dialog::New {
                    title: String::new(),
                })
            }
            KeyCode::Char('r') => {
                if let Some(session) = self.selected_session() {
                    self.dialog = Some(Dialog::Edit {
                        name: session.name.clone(),
                        title: session.shown_name().to_owned(),
                        space: session.space.clone().unwrap_or_default(),
                        field: 0,
                    });
                }
            }
            KeyCode::Char('x') => {
                if let Some(session) = self.selected_session() {
                    self.dialog = Some(Dialog::End {
                        name: session.name.clone(),
                        title: session.shown_name().to_owned(),
                    });
                }
            }
            _ => {}
        }
    }

    fn dialog_key(&mut self, key: KeyEvent) {
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        if key.code == KeyCode::Esc {
            self.dialog = None;
            return;
        }
        match dialog {
            Dialog::End { name, .. } => match key.code {
                KeyCode::Char('y') => {
                    let name = name.clone();
                    self.dialog = None;
                    self.report(session::kill(&name));
                }
                KeyCode::Char('n') => self.dialog = None,
                _ => {}
            },
            Dialog::New { title } => match key.code {
                KeyCode::Enter if !title.trim().is_empty() => {
                    let title = title.trim().to_owned();
                    self.dialog = None;
                    self.create(&title);
                }
                KeyCode::Backspace => {
                    title.pop();
                }
                KeyCode::Char(c) => title.push(c),
                _ => {}
            },
            Dialog::Edit {
                name,
                title,
                space,
                field,
            } => match key.code {
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                    *field = 1 - *field
                }
                KeyCode::Enter if !title.trim().is_empty() => {
                    let (name, title, space) = (
                        name.clone(),
                        title.trim().to_owned(),
                        space.trim().to_owned(),
                    );
                    self.dialog = None;
                    self.save(&name, &title, &space);
                }
                KeyCode::Backspace => {
                    if *field == 0 {
                        title.pop();
                    } else {
                        space.pop();
                    }
                }
                KeyCode::Char(c) => {
                    if *field == 0 {
                        title.push(c);
                    } else {
                        space.push(c);
                    }
                }
                _ => {}
            },
        }
    }

    fn click(&mut self, column: u16, row: u16) {
        if self.dialog.is_some() {
            return;
        }
        let hit = self
            .cards
            .iter()
            .find(|(_, rect)| rect.contains((column, row).into()))
            .map(|(name, _)| name.clone());
        if let Some(name) = hit {
            self.selected = Some(name.clone());
            self.open(&name);
        }
    }

    fn selected_session(&self) -> Option<&Session> {
        self.sessions
            .iter()
            .find(|s| Some(&s.name) == self.selected.as_ref())
    }

    fn keep_selection_visible(&mut self) {
        let visible = self.visible();
        if !visible
            .iter()
            .any(|s| Some(&s.name) == self.selected.as_ref())
        {
            self.selected = visible.first().map(|s| s.name.clone());
        }
    }

    fn step(&mut self, by: isize) {
        let names = self
            .visible()
            .into_iter()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>();
        let Some(at) = names.iter().position(|n| Some(n) == self.selected.as_ref()) else {
            return;
        };
        let next = (at as isize + by).clamp(0, names.len() as isize - 1) as usize;
        self.selected = Some(names[next].clone());
    }

    /// Moves to the card above or below, the nearest across; off screen, along the list instead.
    fn vertical(&mut self, down: bool) {
        let Some(from) = self
            .cards
            .iter()
            .find(|(n, _)| Some(n) == self.selected.as_ref())
            .map(|(_, r)| *r)
        else {
            return;
        };
        let target = self
            .cards
            .iter()
            .filter(|(_, r)| if down { r.y > from.y } else { r.y < from.y })
            .min_by_key(|(_, r)| (r.y.abs_diff(from.y), r.x.abs_diff(from.x)))
            .map(|(name, _)| name.clone());
        match target {
            Some(name) => self.selected = Some(name),
            None => {
                let columns = self
                    .cards
                    .iter()
                    .filter(|(_, r)| r.y == from.y)
                    .count()
                    .max(1);
                self.step(if down {
                    columns as isize
                } else {
                    -(columns as isize)
                });
            }
        }
    }

    /// Brings the session's window to the front, or opens it in a Ghostty window of its own, which
    /// outlives this list.
    fn open(&mut self, name: &str) {
        if let Some(window) = desktop::window_of(name) {
            match desktop::focus(window) {
                Ok(true) => {
                    self.report(session::mark_seen(name));
                    return;
                }
                Ok(false) => {}
                Err(err) => {
                    self.notice = Some(err.to_string());
                    return;
                }
            }
        }
        let result = Command::new("setsid")
            .args(["-f", "ghostty", "-e", "skiff", "open", name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .and_then(|status| {
                if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other("could not start ghostty"))
                }
            });
        self.report(result);
    }

    /// A new session called [title]: its zmx name comes from the title, as in Termarchy.
    fn create(&mut self, title: &str) {
        let taken = self
            .sessions
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>();
        let name = session::session_id(title, &taken);
        if title != name
            && let Err(err) = session::rename(&name, title)
        {
            self.notice = Some(err.to_string());
            return;
        }
        self.selected = Some(name.clone());
        self.open(&name);
    }

    fn save(&mut self, name: &str, title: &str, space: &str) {
        let Some(session) = self.sessions.iter().find(|s| s.name == name).cloned() else {
            return;
        };
        if title != session.shown_name() {
            let result = session::rename(name, if title == name { "-" } else { title });
            self.report(result);
        }
        if space != session.space.as_deref().unwrap_or_default() {
            let result = session::move_to(name, if space.is_empty() { "-" } else { space });
            self.report(result);
        }
    }

    fn report(&mut self, result: io::Result<()>) {
        if let Err(err) = result {
            self.notice = Some(err.to_string());
        }
    }
}

/// The session's screen as styled text, its trailing blank lines dropped.
fn preview(name: &str) -> Option<Text<'static>> {
    let output = Command::new("zmx")
        .args(["history", name, "--vt", "--screen"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let mut text = keep_text_and_colors(&output.stdout).into_text().ok()?;
    while text.lines.last().is_some_and(|line| line.width() == 0) {
        text.lines.pop();
    }
    while text.lines.first().is_some_and(|line| line.width() == 0) {
        text.lines.remove(0);
    }
    Some(text)
}

/// zmx's screen dump as plain text with colors: cursor moves forward become spaces, and every
/// other control sequence (modes, titles, cursor placement) goes.
fn keep_text_and_colors(dump: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(dump.len());
    let mut index = 0;
    while index < dump.len() {
        match (dump[index], dump.get(index + 1)) {
            (0x1b, Some(b'[')) => {
                let start = index + 2;
                let mut end = start;
                while end < dump.len() && !(0x40..=0x7e).contains(&dump[end]) {
                    end += 1;
                }
                let Some(&last) = dump.get(end) else { break };
                let params = &dump[start..end];
                match last {
                    b'm' => out.extend_from_slice(&dump[index..=end]),
                    b'C' => {
                        let count = std::str::from_utf8(params)
                            .ok()
                            .and_then(|p| p.parse().ok())
                            .unwrap_or(1);
                        out.extend(std::iter::repeat_n(b' ', count));
                    }
                    _ => {}
                }
                index = end + 1;
            }
            (0x1b, Some(b']')) => {
                // an OSC ends at BEL or at ESC \
                let mut end = index + 2;
                while end < dump.len()
                    && dump[end] != 0x07
                    && !(dump[end] == 0x1b && dump.get(end + 1) == Some(&b'\\'))
                {
                    end += 1;
                }
                index = if dump.get(end) == Some(&0x07) {
                    end + 1
                } else {
                    end + 2
                };
            }
            (0x1b, Some(_)) => {
                // ESC, any intermediate bytes (as in ESC ( B, the character set), then a final byte
                let mut end = index + 1;
                while end < dump.len() && (0x20..=0x2f).contains(&dump[end]) {
                    end += 1;
                }
                index = end + 1;
            }
            (b'\r', _) => index += 1,
            (byte, _) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::keep_text_and_colors;

    #[test]
    fn keeps_text_and_colors_and_turns_cursor_moves_into_spaces() {
        let dump =
            b"\x1b[?1000h\x1b[?2004h\r\n\x1b[0m\x1b[38;2;1;2;3mA\x1b[2CB\x1b]0;title\x07\x1b(B\r\nC";
        assert_eq!(
            keep_text_and_colors(dump),
            b"\n\x1b[0m\x1b[38;2;1;2;3mA  B\nC".to_vec()
        );
    }
}
