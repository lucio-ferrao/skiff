//! `skiff ui`: the sessions as cards grouped by space, each with its agent's state and a live look
//! at its screen, as the Termarchy grid shows them. A session opens in a Ghostty window of its own;
//! closing that window leaves the session running.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Widget};
use ratatui::{DefaultTerminal, Frame};

use crate::desktop;
use crate::session::{self, Session};
use crate::store;
use crate::watch;

/// A card is at least this wide; the grid fits as many columns as it can.
const CARD_MIN_WIDTH: u16 = 32;
const CARD_HEIGHT: u16 = 10;
/// Blank columns after a card, and blank rows under a row of cards.
const CARD_GAP: u16 = 2;
const ROW_GAP: u16 = 1;
/// The strip a card is dropped on to leave its space, while no session is without one.
const NO_SPACE_STRIP: u16 = 3;
const PREVIEW_INTERVAL: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_millis(100);

// herdr's colors (Catppuccin Mocha), as Termarchy draws the states
const WORKING: Color = Color::Rgb(0xF9, 0xE2, 0xAF);
const ASKING: Color = Color::Rgb(0xF3, 0x8B, 0xA8);
const READY: Color = Color::Rgb(0x94, 0xE2, 0xD5);
const DIM: Color = Color::Rgb(0x6C, 0x70, 0x86);
const TEXT: Color = Color::Rgb(0xCD, 0xD6, 0xF4);
const SUBTEXT: Color = Color::Rgb(0xA6, 0xAD, 0xC8);
// a card's title row: Mocha's surface 0, a shade lighter when selected, the text colour when lifted
const TITLE: Color = Color::Rgb(0x31, 0x32, 0x44);
const SELECTED: Color = Color::Rgb(0x58, 0x5B, 0x70);
const LIFTED: Color = Color::Rgb(0x6C, 0x5A, 0x2E);
const LIFTED_SCREEN: Color = Color::Rgb(0x2A, 0x26, 0x1C);
// the shades spaces lie on, a darker one and a slightly lighter one in turn, so neighbours differ
const SPACES: [Color; 2] = [Color::Rgb(0x1E, 0x1E, 0x2A), Color::Rgb(0x26, 0x26, 0x35)];
/// The rows a space starts with, its name on the middle one.
const SPACE_HEADING: u16 = 3;
/// Columns kept blank on the left.
const MARGIN: u16 = 2;
// the title's colour: Mocha's blue
const TITLE_COLOR: Color = Color::Rgb(0x89, 0xB4, 0xFA);
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
    /// A space's name, to change it; [old] is the one it has.
    Space {
        old: String,
        name: String,
    },
    /// The right button's menu over a card, where it was clicked.
    Menu {
        name: String,
        at: (u16, u16),
        item: usize,
    },
}

const MENU: [&str; 2] = ["Rename", "Close"];

/// Where a dragged card would land: beside another card (after it, or before), or at the end of
/// a space, or of the sessions without one.
enum Drop {
    Card(String, bool),
    Group(Option<String>),
}

/// Where each space and card goes, in list coordinates, before scrolling.
struct Layout {
    /// A space's frame, or the area of the sessions without a space.
    groups: Vec<(Option<String>, Rect)>,
    cards: Vec<(String, Rect)>,
    height: u16,
}

struct App {
    sessions: Vec<Session>,
    previews: HashMap<String, Text<'static>>,
    selected: Option<String>,
    filter: String,
    filtering: bool,
    dialog: Option<Dialog>,
    scroll: u16,
    /// Bring the selected card into view on the next draw; set when the keyboard moves the selection.
    reveal: bool,
    /// Where each card was drawn last, for the mouse and for moving up and down.
    cards: Vec<(String, Rect)>,
    /// The spaces shown folded, as skiff keeps them.
    collapsed: HashSet<String>,
    /// Where each group was drawn last, for dropping a card past the others.
    groups: Vec<(Option<String>, Rect)>,
    /// Where the menu was drawn last.
    menu: Rect,
    /// The card under the left button, and where it went down.
    press: Option<(String, (u16, u16))>,
    /// The card being dragged, where the pointer held it, and where the pointer is.
    dragging: Option<String>,
    grab: (u16, u16),
    pointer: (u16, u16),
    /// Each space's rule with its name, where it was drawn last: dragged to move the space.
    headers: Vec<(String, Rect)>,
    press_space: Option<(String, (u16, u16))>,
    dragging_space: Option<String>,
    notice: Option<String>,
    started: Instant,
    quit: bool,
    /// The session to enter here, leaving the list until it is detached.
    attach: Option<String>,
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

    let mut app = App {
        sessions: Vec::new(),
        previews: HashMap::new(),
        selected: None,
        filter: String::new(),
        filtering: false,
        dialog: None,
        scroll: 0,
        reveal: true,
        cards: Vec::new(),
        groups: Vec::new(),
        collapsed: store::Spaces::load().collapsed.into_iter().collect(),
        menu: Rect::default(),
        press: None,
        dragging: None,
        grab: (0, 0),
        pointer: (0, 0),
        headers: Vec::new(),
        press_space: None,
        dragging_space: None,
        notice: None,
        started: Instant::now(),
        quit: false,
        attach: None,
    };
    loop {
        let mut terminal = ratatui::init();
        execute!(io::stdout(), EnableMouseCapture)?;
        let result = run(&mut terminal, &mut app, &inbox, &wanted);
        let _ = execute!(io::stdout(), DisableMouseCapture);
        ratatui::restore();
        result?;
        let Some(name) = app.attach.take() else {
            return Ok(());
        };
        session::mark_seen(&name)?;
        // inside a zmx session the terminal moves to the chosen one (zmx's switch) and the list
        // ends, so no session ever runs inside another
        if std::env::var_os("ZMX_SESSION").is_some_and(|current| !current.is_empty()) {
            Command::new("zmx").args(["attach", &name]).status()?;
            return Ok(());
        }
        // the session takes this terminal; detaching from it (ctrl+\) comes back to the list
        Command::new("zmx").args(["attach", &name]).status()?;
        // pictures the session drew (kitty's graphics protocol) stay in the terminal over the text
        // until deleted; its program sends them again when the session is entered again
        let mut out = io::stdout();
        out.write_all(b"\x1b_Ga=d,d=A,q=2\x1b\\")?;
        out.flush()?;
    }
}

fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    inbox: &mpsc::Receiver<Message>,
    wanted: &mpsc::Sender<Vec<String>>,
) -> io::Result<()> {
    let mut last_preview = Instant::now() - PREVIEW_INTERVAL;
    while !app.quit && app.attach.is_none() {
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
                Event::Mouse(mouse) => app.mouse(mouse),
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
    /// The sessions the filter lets through, grouped by space as skiff lists them, those without one
    /// first, each in its arranged order.
    fn matching(&self) -> Vec<&Session> {
        let filter = self.filter.to_lowercase();
        let mut shown = self
            .sessions
            .iter()
            .filter(|s| filter.is_empty() || s.shown_name().to_lowercase().contains(&filter))
            .collect::<Vec<_>>();
        shown.sort_by_key(|s| s.space.is_some());
        shown
    }

    /// The sessions shown as cards: those matching, but for the ones in folded spaces.
    fn visible(&self) -> Vec<&Session> {
        self.matching()
            .into_iter()
            .filter(|s| {
                !s.space
                    .as_ref()
                    .is_some_and(|space| self.collapsed.contains(space))
            })
            .collect()
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        // a margin on the left, and above: a blank row, the title's rows, a blank row
        let inner = Rect::new(
            area.x + MARGIN,
            area.y,
            area.width.saturating_sub(MARGIN),
            area.height,
        );
        let rows = TITLE_LETTERS.len() as u16;
        let title = Rect::new(inner.x, inner.y + 1, inner.width, rows);
        // the spaces' shades reach the first column; the cards keep the margin
        let grid = Rect::new(
            area.x,
            inner.y + rows + 2,
            area.width,
            inner.height.saturating_sub(rows + 3),
        );
        let status = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
        // the title lies on the sessions without a space: the first, darker shade
        frame.buffer_mut().set_style(
            Rect::new(area.x, area.y, area.width, grid.y - area.y),
            Style::new().bg(SPACES[0]),
        );
        draw_title(frame, title);
        self.draw_grid(frame, grid);
        self.draw_status(frame, status);
        if self.dialog.is_some() {
            self.draw_dialog(frame, area);
        }
    }

    /// Each space on a shade, starting with three rows, its name in the middle of the middle one;
    /// the sessions without a space first, without those rows. Groups span the whole width; the
    /// cards keep the margin.
    fn layout(&self, width: u16) -> Layout {
        let visible = self
            .matching()
            .into_iter()
            .map(|s| (s.name.clone(), s.space.clone()))
            .collect::<Vec<_>>();
        let mut layout = Layout {
            groups: Vec::new(),
            cards: Vec::new(),
            height: 0,
        };
        let mut y = 0u16;
        // while a card is dragged and no session is without a space, a strip on top to drop it there
        if self.dragging.is_some() && visible.iter().all(|(_, space)| space.is_some()) {
            layout
                .groups
                .push((None, Rect::new(0, 0, width, NO_SPACE_STRIP)));
            y = NO_SPACE_STRIP;
        }
        let mut index = 0;
        while index < visible.len() {
            let space = visible[index].1.clone();
            let members = visible[index..]
                .iter()
                .take_while(|(_, s)| *s == space)
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            index += members.len();
            let named = space.is_some();
            let columns = (width.saturating_sub(MARGIN) / CARD_MIN_WIDTH).max(1);
            let card_width = width.saturating_sub(MARGIN) / columns;
            let top = y;
            let first_row = y + if named { SPACE_HEADING } else { 0 };
            // a folded space is its heading alone
            let folded = space.as_ref().is_some_and(|s| self.collapsed.contains(s));
            let members = if folded { Vec::new() } else { members };
            let count = members.len() as u16;
            for (position, name) in members.into_iter().enumerate() {
                let position = position as u16;
                let rect = Rect::new(
                    MARGIN + (position % columns) * card_width,
                    first_row + (position / columns) * (CARD_HEIGHT + ROW_GAP),
                    card_width,
                    CARD_HEIGHT,
                );
                layout.cards.push((name, rect));
            }
            y = first_row + count.div_ceil(columns) * (CARD_HEIGHT + ROW_GAP);
            layout
                .groups
                .push((space, Rect::new(0, top, width, y - top)));
        }
        layout.height = y;
        layout
    }

    fn draw_grid(&mut self, frame: &mut Frame, area: Rect) {
        let layout = self.layout(area.width);
        if std::mem::take(&mut self.reveal) {
            self.reveal_selected(&layout.cards, area.height);
        }
        self.scroll = self.scroll.min(layout.height.saturating_sub(area.height));
        if layout.cards.is_empty() {
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
        let target = self.drop_target();
        let space_target = self.space_target();
        self.groups.clear();
        self.headers.clear();
        // every group on a shade, the darker and the lighter in turn, those without a space included;
        // they share the title's, and without them the first space takes the other
        let after_title = usize::from(!matches!(layout.groups.first(), Some((None, _))));
        for (shade, (space, rect)) in layout.groups.into_iter().enumerate() {
            let shade = shade + after_title;
            let lit = matches!(&target, Some(Drop::Group(g)) if *g == space);
            let has_cards = layout
                .cards
                .iter()
                .any(|(_, card)| card.y >= rect.y && card.bottom() <= rect.bottom());
            if space.is_some()
                && rect.y >= self.scroll
                && rect.y + SPACE_HEADING <= self.scroll + area.height
            {
                let row = Rect::new(
                    area.x,
                    area.y + rect.y - self.scroll,
                    area.width,
                    SPACE_HEADING,
                );
                self.headers.push((space.clone().unwrap_or_default(), row));
            }
            let label = space.as_deref().map(|name| self.space_heading(name, lit));
            let shown = blit(
                frame,
                area,
                self.scroll,
                rect,
                Rect::default(),
                |buffer, whole| {
                    // a space on a shade of its own, its name in the middle of its first rows
                    buffer.set_style(whole, Style::new().bg(SPACES[shade % SPACES.len()]));
                    if let Some(label) = label {
                        let row = Rect::new(whole.x, whole.y + 1, whole.width, 1);
                        Paragraph::new(Line::from(label).centered()).render(row, buffer);
                        // the mark that folds or opens it, on the right, clear of the margin
                        let folded = space.as_ref().is_some_and(|s| self.collapsed.contains(s));
                        let toggle =
                            Rect::new(row.x, row.y, row.width.saturating_sub(MARGIN + 1), 1);
                        Paragraph::new(
                            Line::from(Span::styled(
                                if folded { "⯈" } else { "⯆" },
                                Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
                            ))
                            .right_aligned(),
                        )
                        .render(toggle, buffer);
                    } else if !has_cards {
                        Paragraph::new(Line::from(Span::styled(
                            "no space",
                            Style::new().fg(if lit { READY } else { DIM }),
                        )))
                        .alignment(Alignment::Center)
                        .render(
                            Rect::new(whole.x, whole.y + whole.height / 2, whole.width, 1),
                            buffer,
                        );
                    }
                },
            );
            if let Some(shown) = shown {
                self.groups.push((space, shown));
            }
        }
        self.cards.clear();
        for (name, rect) in layout.cards {
            let Some(session) = self.sessions.iter().find(|s| s.name == name) else {
                continue;
            };
            let marker = match &target {
                Some(Drop::Card(beside, after)) if *beside == name => Some(*after),
                _ => None,
            };
            // a card half out of view is drawn cut, so the screen's height is all used; all of it but
            // the blank columns after it is copied, on the terminal's background
            let card = Rect::new(0, 0, rect.width.saturating_sub(CARD_GAP), rect.height);
            let shown = blit(frame, area, self.scroll, rect, card, |buffer, whole| {
                self.draw_card(buffer, whole, session, false);
                // where a dragged card would go: a bar on this card's side
                if let Some(after) = marker {
                    let x = if after { whole.right() - 1 } else { whole.x };
                    for y in whole.y..whole.bottom() {
                        buffer[(x, y)].set_symbol("┃").set_fg(READY);
                    }
                }
            });
            if let Some(shown) = shown {
                self.cards.push((name, shown));
            }
        }
        // where a dragged space would go: a line on the border above or below the space under it
        if let Some((beside, after)) = &space_target
            && let Some((_, rect)) = self.groups.iter().find(|(s, _)| s.as_ref() == Some(beside))
        {
            let y = if *after { rect.bottom() } else { rect.y };
            if y < area.bottom() {
                let line = "━".repeat(usize::from(area.width));
                frame.render_widget(
                    Span::styled(line, Style::new().fg(READY)),
                    Rect::new(area.x, y, area.width, 1),
                );
            }
        }
        self.draw_dragged(frame, area);
    }

    /// What is being dragged follows the pointer, over the list: a card where the pointer holds
    /// it, or a space's rule on the pointer's row.
    fn draw_dragged(&self, frame: &mut Frame, area: Rect) {
        if let Some(name) = &self.dragging
            && let Some(session) = self.sessions.iter().find(|s| &s.name == name)
            && let Some((_, slot)) = self.cards.iter().find(|(n, _)| n == name)
        {
            let width = slot.width.min(area.width);
            let height = CARD_HEIGHT.min(area.height);
            let x = self
                .pointer
                .0
                .saturating_sub(self.grab.0)
                .clamp(area.x, area.right() - width);
            let y = self
                .pointer
                .1
                .saturating_sub(self.grab.1)
                .clamp(area.y, area.bottom() - height);
            let ghost = Rect::new(x, y, width, height);
            let whole = Rect::new(0, 0, width, height);
            let mut drawn = Buffer::empty(whole);
            self.draw_card(&mut drawn, whole, session, true);
            let screen = frame.buffer_mut();
            for row in 0..height {
                // the card only, not the blank columns after it
                for column in 0..width.saturating_sub(CARD_GAP) {
                    screen[(ghost.x + column, ghost.y + row)] = drawn[(column, row)].clone();
                }
            }
        }
        if let Some(name) = &self.dragging_space {
            let row = Rect::new(
                area.x,
                self.pointer.1.clamp(area.y, area.bottom() - 1),
                area.width,
                1,
            );
            // the space's name row, centred, lifted
            let heading = Paragraph::new(
                Line::from(Span::styled(
                    format!("{name} "),
                    Style::new().fg(WORKING).add_modifier(Modifier::BOLD),
                ))
                .centered(),
            )
            .style(Style::new().bg(LIFTED_SCREEN));
            frame.render_widget(Clear, row);
            frame.render_widget(heading, row);
        }
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

    /// A card; [lifted], the copy that follows the pointer in a drag.
    /// A card without a frame: a title row, on a shade of its own, with the name and right after it
    /// the state as herdr and Termarchy mark it (idle has none); the screen below; and blank columns
    /// on the right, between cards. [lifted]: the copy that follows the pointer in a drag.
    fn draw_card(&self, buffer: &mut Buffer, area: Rect, session: &Session, lifted: bool) {
        let selected =
            self.selected.as_deref() == Some(session.name.as_str()) && self.dragging.is_none();
        let mark = self.mark(session);
        let shade = if lifted {
            LIFTED
        } else if selected {
            SELECTED
        } else {
            TITLE
        };
        let mut title = vec![
            Span::raw(" "),
            Span::styled(
                session.shown_name().to_owned(),
                Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some((mark, color)) = mark {
            title.push(Span::raw(" "));
            title.push(Span::styled(mark, Style::new().fg(color)));
        }
        let width = area.width.saturating_sub(CARD_GAP);
        Paragraph::new(Line::from(title))
            .style(Style::new().bg(shade))
            .render(Rect::new(area.x, area.y, width, 1), buffer);
        if let Some(text) = self.previews.get(&session.name) {
            // the bottom of the screen, where the prompt and the latest output are
            let screen = Rect::new(area.x, area.y + 1, width, area.height.saturating_sub(1));
            let lines = text.lines.len();
            let shown = usize::from(screen.height).min(lines);
            let tail = Text::from(text.lines[lines - shown..].to_vec());
            // on the terminal's own background, as the session shows; the lifted copy on a tint
            if lifted {
                buffer.set_style(screen, Style::new().bg(LIFTED_SCREEN));
            }
            Paragraph::new(tail).render(screen, buffer);
            if lifted {
                for y in screen.y..screen.bottom() {
                    for x in screen.x..screen.right() {
                        if buffer[(x, y)].bg == Color::Reset {
                            buffer[(x, y)].set_bg(LIFTED_SCREEN);
                        }
                    }
                }
            }
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
                " enter open · w window · n new · r name & space · del end · / filter · esc quit",
                Style::new().fg(DIM),
            ))
        };
        frame.render_widget(line, area);
    }

    fn draw_dialog(&mut self, frame: &mut Frame, area: Rect) {
        if let Some(Dialog::Menu { at, item, .. }) = &self.dialog {
            let width = 12.min(area.width);
            let height = (MENU.len() as u16 + 2).min(area.height);
            let rect = Rect::new(
                at.0.min(area.right().saturating_sub(width)),
                at.1.min(area.bottom().saturating_sub(height)),
                width,
                height,
            );
            let block = Block::bordered()
                .border_type(BorderType::Plain)
                .border_style(Style::new().fg(TEXT));
            let inner = block.inner(rect);
            frame.render_widget(Clear, rect);
            frame.render_widget(block, rect);
            let lines = MENU
                .iter()
                .enumerate()
                .map(|(index, label)| {
                    let style = if index == *item {
                        Style::new().fg(TEXT).add_modifier(Modifier::REVERSED)
                    } else {
                        Style::new().fg(TEXT)
                    };
                    Line::from(Span::styled(format!(" {label:<9}"), style))
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines), inner);
            self.menu = rect;
            return;
        }
        let Some(dialog) = &self.dialog else { return };
        let field = |label: &str, value: &str, active: bool| {
            Line::from(vec![
                Span::styled(format!("{label:>6}  "), Style::new().fg(DIM)),
                Span::styled(value.to_owned(), Style::new().fg(TEXT)),
                Span::styled(if active { "▏" } else { "" }, Style::new().fg(TEXT)),
            ])
        };
        let (title, lines, hint) = match dialog {
            // drawn above, where it was asked for
            Dialog::Menu { .. } => return,
            Dialog::New { title } => (
                " New session ",
                vec![field("Name", title, true)],
                "enter open · esc cancel",
            ),
            Dialog::Space { name, .. } => (
                " Space ",
                vec![field("Name", name, true)],
                "enter save · esc cancel",
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
            // Esc leaves, after clearing a filter first
            KeyCode::Esc if self.filter.is_empty() => self.quit = true,
            KeyCode::Esc => {
                self.filter.clear();
                self.keep_selection_visible();
            }
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Left | KeyCode::Char('h') => self.step(-1),
            KeyCode::Right | KeyCode::Char('l') => self.step(1),
            // Tab goes round the list, from the last back to the first
            KeyCode::Tab => self.cycle(1),
            KeyCode::BackTab => self.cycle(-1),
            KeyCode::Up | KeyCode::Char('k') => self.vertical(false),
            KeyCode::Down | KeyCode::Char('j') => self.vertical(true),
            KeyCode::Char('w') => {
                if let Some(name) = self.selected.clone() {
                    self.open_window(&name);
                }
            }
            KeyCode::Enter => self.attach = self.selected.clone(),
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
            KeyCode::Char('x') | KeyCode::Delete | KeyCode::Backspace => {
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
            Dialog::Menu { name, item, .. } => match key.code {
                KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                    *item = item.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                    *item = (*item + 1).min(MENU.len() - 1)
                }
                KeyCode::Enter => {
                    let (name, item) = (name.clone(), *item);
                    self.choose(&name, item);
                }
                _ => {}
            },
            Dialog::End { name, .. } => match key.code {
                KeyCode::Char('y') => {
                    let name = name.clone();
                    self.dialog = None;
                    self.report(session::kill(&name));
                }
                KeyCode::Char('n') => self.dialog = None,
                _ => {}
            },
            Dialog::Space { old, name } => match key.code {
                KeyCode::Enter if !name.trim().is_empty() => {
                    let (old, name) = (old.clone(), name.trim().to_owned());
                    self.dialog = None;
                    if name != old {
                        let result = session::rename_space(&old, &name);
                        self.report(result);
                    }
                }
                KeyCode::Backspace => {
                    name.pop();
                }
                KeyCode::Char(c) => name.push(c),
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

    /// The left button enters a session, or drags its card to another place or space; the middle
    /// one opens it in a window, as it opens a link in a new tab; the right one opens its menu.
    fn mouse(&mut self, event: MouseEvent) {
        let at = (event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(Dialog::Menu { name, .. }) = &self.dialog {
                    let name = name.clone();
                    let inside = self.menu.contains(at.into());
                    self.dialog = None;
                    // the border rows are no item
                    if inside && at.1 > self.menu.y && at.1 < self.menu.bottom() - 1 {
                        self.choose(&name, usize::from(at.1 - self.menu.y - 1));
                    }
                    return;
                }
                if self.dialog.is_none() {
                    self.press = self.card_at(at).map(|name| (name, at));
                    self.press_space = self
                        .headers
                        .iter()
                        .find(|(_, row)| row.contains(at.into()))
                        .map(|(name, _)| (name.clone(), at));
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some((name, from)) = &self.press
                    && self.dragging.is_none()
                    && *from != at
                {
                    // the copy that follows the pointer keeps the spot it was held by
                    if let Some((_, rect)) = self.cards.iter().find(|(n, _)| n == name) {
                        self.grab = (from.0 - rect.x, from.1 - rect.y);
                    }
                    self.selected = Some(name.clone());
                    self.dragging = Some(name.clone());
                }
                if let Some((name, from)) = &self.press_space
                    && self.dragging_space.is_none()
                    && *from != at
                {
                    self.dragging_space = Some(name.clone());
                }
                self.pointer = at;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.pointer = at;
                if self.dragging_space.is_some() {
                    self.drop_space();
                } else if self.dragging.is_some() {
                    self.drop_dragged();
                } else if let Some((name, _)) = self.press.take() {
                    self.selected = Some(name.clone());
                    self.attach = Some(name);
                } else if let Some((space, _)) = self.press_space.take() {
                    // a click on a space's name folds it, or opens it
                    match session::toggle_collapsed(&space) {
                        Ok(collapsed) => {
                            self.collapsed = collapsed.into_iter().collect();
                            self.keep_selection_visible();
                        }
                        Err(err) => self.notice = Some(err.to_string()),
                    }
                }
                self.press = None;
                self.press_space = None;
            }
            MouseEventKind::Down(MouseButton::Middle) if self.dialog.is_none() => {
                if let Some(name) = self.card_at(at) {
                    self.selected = Some(name.clone());
                    self.open_window(&name);
                }
            }
            MouseEventKind::Down(MouseButton::Right) if self.dialog.is_none() => {
                if let Some(name) = self.card_at(at) {
                    self.selected = Some(name.clone());
                    self.dialog = Some(Dialog::Menu { name, at, item: 0 });
                } else if let Some((space, _)) =
                    self.headers.iter().find(|(_, row)| row.contains(at.into()))
                {
                    // on a space's name, renaming the space
                    self.dialog = Some(Dialog::Space {
                        name: space.clone(),
                        old: space.clone(),
                    });
                }
            }
            MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_add(3),
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(3),
            _ => {}
        }
    }

    fn card_at(&self, at: (u16, u16)) -> Option<String> {
        self.cards
            .iter()
            .find(|(_, rect)| rect.contains(at.into()))
            .map(|(name, _)| name.clone())
    }

    /// A menu item for session [name]: its name and space, or ending it.
    fn choose(&mut self, name: &str, item: usize) {
        self.dialog = None;
        let Some(session) = self.sessions.iter().find(|s| s.name == name) else {
            return;
        };
        self.dialog = Some(match item {
            0 => Dialog::Edit {
                name: session.name.clone(),
                title: session.shown_name().to_owned(),
                space: session.space.clone().unwrap_or_default(),
                field: 0,
            },
            _ => Dialog::End {
                name: session.name.clone(),
                title: session.shown_name().to_owned(),
            },
        });
    }

    /// A session's state as herdr and Termarchy mark it; idle has none.
    fn mark(&self, session: &Session) -> Option<(&'static str, Color)> {
        match session.state.as_str() {
            "working" => {
                let frame = (self.started.elapsed().as_millis() / 100) as usize % SPINNER.len();
                Some((SPINNER[frame], WORKING))
            }
            "asking" => Some(("●", ASKING)),
            "ready" => Some(("✓", READY)),
            _ => None,
        }
    }

    /// A space's name; folded, with how many sessions it holds and the marks of those not idle.
    fn space_heading(&self, name: &str, lit: bool) -> Vec<Span<'static>> {
        let style = Style::new()
            .fg(if lit { READY } else { SUBTEXT })
            .add_modifier(Modifier::BOLD);
        if !self.collapsed.contains(name) {
            return vec![Span::styled(name.to_owned(), style)];
        }
        let members = self
            .sessions
            .iter()
            .filter(|s| s.space.as_deref() == Some(name))
            .collect::<Vec<_>>();
        let mut spans = vec![Span::styled(format!("{name} · {}", members.len()), style)];
        for session in members {
            if let Some((mark, color)) = self.mark(session) {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(mark, Style::new().fg(color)));
            }
        }
        spans
    }

    /// Where a dragged space would land: above the space under the pointer, or below it when the
    /// pointer is in its lower half; over the sessions without a space, above the first space.
    fn space_target(&self) -> Option<(String, bool)> {
        let dragged = self.dragging_space.as_ref()?;
        let (x, y) = self.pointer;
        let under = self
            .groups
            .iter()
            .find(|(_, rect)| rect.contains((x, y).into()));
        match under {
            Some((Some(space), rect)) if space != dragged => {
                Some((space.clone(), y >= rect.y + rect.height / 2))
            }
            Some((None, _)) => self
                .groups
                .iter()
                .find_map(|(space, _)| space.clone().filter(|s| s != dragged))
                .map(|first| (first, false)),
            _ => None,
        }
    }

    fn drop_space(&mut self) {
        let target = self.space_target();
        let Some(dragged) = self.dragging_space.take() else {
            return;
        };
        let Some((beside, after)) = target else {
            return;
        };
        let mut order = Vec::<String>::new();
        for space in self.sessions.iter().filter_map(|s| s.space.clone()) {
            if space != dragged && !order.contains(&space) {
                order.push(space);
            }
        }
        let index = order
            .iter()
            .position(|s| *s == beside)
            .unwrap_or(order.len());
        order.insert(index + usize::from(after), dragged);
        let result = session::arrange_spaces(order);
        self.report(result);
    }

    /// Where the dragged card would land, by the pointer; over itself, nowhere.
    fn drop_target(&self) -> Option<Drop> {
        let dragged = self.dragging.as_ref()?;
        let at = self.pointer.into();
        let (name, rect) = self
            .cards
            .iter()
            .find(|(_, rect)| rect.contains(at))?
            .clone();
        if &name == dragged {
            return None;
        }
        Some(Drop::Card(name, self.pointer.0 >= rect.x + rect.width / 2))
    }

    /// Puts the dragged card where it was let go: beside a card, in that card's space, or at the
    /// end of the space under the pointer.
    fn drop_dragged(&mut self) {
        let target = self.drop_target().or_else(|| {
            let at = self.pointer.into();
            self.groups
                .iter()
                .find(|(_, rect)| rect.contains(at))
                .map(|(space, _)| Drop::Group(space.clone()))
        });
        let Some(dragged) = self.dragging.take() else {
            return;
        };
        let Some(target) = target else {
            return;
        };
        let space_of = |name: &str| {
            self.sessions
                .iter()
                .find(|s| s.name == name)
                .and_then(|s| s.space.clone())
        };
        let mut order = self
            .sessions
            .iter()
            .map(|s| s.name.clone())
            .filter(|name| *name != dragged)
            .collect::<Vec<_>>();
        let (space, index) = match target {
            Drop::Card(beside, after) => {
                let index = order
                    .iter()
                    .position(|n| *n == beside)
                    .unwrap_or(order.len());
                (space_of(&beside), index + usize::from(after))
            }
            Drop::Group(space) => {
                let end = order
                    .iter()
                    .rposition(|n| space_of(n) == space)
                    .map_or(order.len(), |last| last + 1);
                (space, end)
            }
        };
        order.insert(index, dragged.clone());
        let result = session::arrange(&dragged, space.as_deref(), order);
        self.report(result);
    }

    fn selected_session(&self) -> Option<&Session> {
        self.sessions
            .iter()
            .find(|s| Some(&s.name) == self.selected.as_ref())
    }

    fn keep_selection_visible(&mut self) {
        self.reveal = true;
        let visible = self.visible();
        if !visible
            .iter()
            .any(|s| Some(&s.name) == self.selected.as_ref())
        {
            self.selected = visible.first().map(|s| s.name.clone());
        }
    }

    fn cycle(&mut self, by: isize) {
        self.reveal = true;
        let names = self
            .visible()
            .into_iter()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>();
        let Some(at) = names.iter().position(|n| Some(n) == self.selected.as_ref()) else {
            return;
        };
        let next = (at as isize + by).rem_euclid(names.len() as isize) as usize;
        self.selected = Some(names[next].clone());
    }

    fn step(&mut self, by: isize) {
        self.reveal = true;
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
        self.reveal = true;
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
    fn open_window(&mut self, name: &str) {
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
        self.attach = Some(name);
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

/// The session's screen as styled text, ending at the cursor's line: below a prompt there is only
/// noise, such as Claude's status lines.
fn preview(name: &str) -> Option<Text<'static>> {
    let output = Command::new("zmx")
        .args(["history", name, "--vt", "--screen"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let mut text = keep_text_and_colors(&output.stdout).into_text().ok()?;
    // the dump's rows are the screen's, from its first; it ends by placing the cursor
    if let Some(row) = cursor_row(&output.stdout) {
        text.lines.truncate(row);
    }
    while text.lines.last().is_some_and(|line| line.width() == 0) {
        text.lines.pop();
    }
    while text.lines.first().is_some_and(|line| line.width() == 0) {
        text.lines.remove(0);
    }
    Some(text)
}

/// The cursor's row, 1-based, from the last cursor placement (CSI row ; column H) in the dump.
fn cursor_row(dump: &[u8]) -> Option<usize> {
    let mut row = None;
    let mut index = 0;
    while index + 1 < dump.len() {
        if dump[index] != 0x1b || dump[index + 1] != b'[' {
            index += 1;
            continue;
        }
        let start = index + 2;
        let mut end = start;
        while end < dump.len() && (dump[end].is_ascii_digit() || dump[end] == b';') {
            end += 1;
        }
        if dump.get(end) == Some(&b'H') {
            let params = std::str::from_utf8(&dump[start..end]).unwrap_or_default();
            row = params
                .split(';')
                .next()
                .and_then(|r| r.parse().ok())
                .or(Some(1));
        }
        index = end;
    }
    row
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

/// "skiff" in wassup's big letters (its bigtext.rs): box-drawing strokes, lowercase and rounded.
const TITLE_LETTERS: [&str; 3] = ["    ╷   ·  ╭╴  ╭╴", "╭─╴ ├┬╴ ╷ ╶┼╴ ╶┼╴", "╶─╯ ╵╰╴ ╵  ╵   ╵"];

/// The title in big letters, as wassup writes a chat's name over it, in a colour of its own.
fn draw_title(frame: &mut Frame, area: Rect) {
    let lines = TITLE_LETTERS
        .iter()
        .map(|row| Line::from(Span::styled(*row, Style::new().fg(TITLE_COLOR))))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

/// Draws [rect], in list coordinates, through [draw] into a buffer of its size, and copies the
/// part in view onto the screen; returns where it went. Cells left untouched are not copied, so
/// what is under them shows, except within [opaque] (in the drawing's coordinates), copied whole:
/// a card's screen keeps the terminal's background over its space's.
fn blit(
    frame: &mut Frame,
    area: Rect,
    scroll: u16,
    rect: Rect,
    opaque: Rect,
    draw: impl FnOnce(&mut Buffer, Rect),
) -> Option<Rect> {
    let top = rect.y.max(scroll);
    let bottom = rect.bottom().min(scroll + area.height);
    if top >= bottom {
        return None;
    }
    let whole = Rect::new(0, 0, rect.width, rect.height);
    let mut drawn = Buffer::empty(whole);
    draw(&mut drawn, whole);
    let shown = Rect::new(
        area.x + rect.x,
        area.y + top - scroll,
        rect.width,
        bottom - top,
    );
    let screen = frame.buffer_mut();
    for row in 0..shown.height {
        for column in 0..shown.width {
            let at = (column, top - rect.y + row);
            let cell = &drawn[at];
            if cell != &ratatui::buffer::Cell::EMPTY || opaque.contains(at.into()) {
                screen[(shown.x + column, shown.y + row)] = cell.clone();
            }
        }
    }
    Some(shown)
}

#[cfg(test)]
mod tests {
    use super::{cursor_row, keep_text_and_colors};

    #[test]
    fn finds_the_cursor_row_of_the_last_placement() {
        let dump =
            b"\x1b[?1049h  \r\nA\r\n\x1b[38;2;1;2;3mB\x1b[0m\x1b[>4;2m\x1b[2;3H\x1b[0m\x1b(B";
        assert_eq!(cursor_row(dump), Some(2));
        // a capital H in the text is not a placement
        assert_eq!(cursor_row(b"\x1b[5;1H\x1b[38;2;1;2;3mHello H"), Some(5));
    }

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
