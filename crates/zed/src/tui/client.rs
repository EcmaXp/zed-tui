use std::{
    io::{self, BufReader, Write},
    ops::Range,
    os::unix::net::UnixStream,
    path::Path,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use crossterm::{cursor, event, style, terminal};
use gpui::{CursorStyle, Modifiers};
use gpui_tui::{Cell, CellAttrs, CellGrid, CursorShape, Glyph, Rgb};
use parking_lot::Mutex;

use crate::tui::protocol::{
    ClientMessage, FrameDecoder, KeyCode, MouseAction, MouseButtonKind, PROTOCOL_VERSION,
    ServerMessage, TermEvent, read_message, write_message,
};
const PUSH_TITLE: &str = "\x1b[22;0t";
const POP_TITLE: &str = "\x1b[23;0t";
const POINTER_RESET: &[u8] = b"\x1b]22;text\x1b\\";
const CURSOR_SHAPE_RESET: &[u8] = b"\x1b[0 q";
const KEYBOARD_FLAGS_QUERY: &str = "\x1b[?u";
const VERSION_QUERY: &str = "\x1b[>0q";
const DEVICE_ATTRIBUTES_QUERY: &str = "\x1b[c";
const MAX_VERSION_REPLY: usize = 64;
const GHOSTTY_VERSION_PREFIXES: [&[u8]; 2] = [b"ghostty", b"libghostty"];
const QUERY_TIMEOUT: Duration = Duration::from_millis(500);
const RESIZE_FRAME_WAIT: Duration = Duration::from_millis(100);
const ATTRIBUTE_CODES: [(CellAttrs, &str); 2] = [(CellAttrs::BOLD, "1"), (CellAttrs::ITALIC, "3")];

pub enum Exit {
    Detached,
    ServerShutdown,
    Disconnected,
    Rejected(String),
}

#[derive(Clone, Copy, Default)]
struct TerminalFeatures {
    ghostty: bool,
}

#[derive(Default)]
struct TerminalSetup {
    entered: bool,
    keyboard_enhanced: bool,
    features: TerminalFeatures,
}

impl TerminalSetup {
    fn run<W: Write>(
        &mut self,
        output: &mut W,
        query: impl FnOnce(&mut W) -> io::Result<QueryReplies>,
    ) -> io::Result<()> {
        write!(output, "{PUSH_TITLE}")?;
        self.entered = true;
        crossterm::execute!(
            output,
            terminal::EnterAlternateScreen,
            cursor::Hide,
            event::EnableMouseCapture,
            event::EnableBracketedPaste,
        )?;
        let replies = query(output)?;
        self.features = replies.features;
        if replies.keyboard_flags {
            crossterm::execute!(
                output,
                event::PushKeyboardEnhancementFlags(
                    event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                )
            )?;
            self.keyboard_enhanced = true;
        }
        Ok(())
    }

    fn undo(&self, output: &mut impl Write) {
        if self.keyboard_enhanced {
            crossterm::execute!(output, event::PopKeyboardEnhancementFlags).ok();
        }
        if self.features.ghostty {
            output.write_all(POINTER_RESET).ok();
        }
        if self.entered {
            output.write_all(CURSOR_SHAPE_RESET).ok();
            crossterm::execute!(
                output,
                style::ResetColor,
                event::DisableBracketedPaste,
                event::DisableMouseCapture,
                cursor::Show,
                terminal::LeaveAlternateScreen,
            )
            .ok();
            write!(output, "{POP_TITLE}").ok();
        }
        output.flush().ok();
    }
}

struct TerminalGuard(TerminalSetup);

impl TerminalGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode().context("enabling raw mode")?;
        let mut guard = Self(TerminalSetup::default());
        guard.0.run(&mut io::stdout(), query_terminal)?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.0.undo(&mut io::stdout());
        terminal::disable_raw_mode().ok();
    }
}

fn query_terminal(stdout: &mut impl Write) -> io::Result<QueryReplies> {
    let mut query = KEYBOARD_FLAGS_QUERY.to_owned();
    query.push_str(VERSION_QUERY);
    query.push_str(DEVICE_ATTRIBUTES_QUERY);
    stdout.write_all(query.as_bytes())?;
    stdout.flush()?;
    Ok(read_query_reply(QUERY_TIMEOUT))
}

#[derive(Default)]
struct QueryReplies {
    device_attributes: bool,
    keyboard_flags: bool,
    features: TerminalFeatures,
    version: Option<Vec<u8>>,
}

impl vte::Perform for QueryReplies {
    fn hook(&mut self, _params: &vte::Params, intermediates: &[u8], ignore: bool, action: char) {
        self.version = (!ignore && intermediates == b">" && action == '|').then(Vec::new);
    }

    fn put(&mut self, byte: u8) {
        if let Some(version) = &mut self.version
            && version.len() < MAX_VERSION_REPLY
        {
            version.push(byte);
        }
    }

    fn unhook(&mut self) {
        if let Some(version) = self.version.take() {
            self.features.ghostty |= GHOSTTY_VERSION_PREFIXES
                .iter()
                .any(|prefix| version.starts_with(prefix));
        }
    }

    fn csi_dispatch(
        &mut self,
        _params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if ignore {
            return;
        }
        match (intermediates, action) {
            (b"?", 'c') => self.device_attributes = true,
            (b"?", 'u') => self.keyboard_flags = true,
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
enum Layer {
    Foreground,
    Background,
    Underline,
}

impl Layer {
    fn extended_code(self) -> &'static str {
        match self {
            Self::Foreground => "38",
            Self::Background => "48",
            Self::Underline => "58",
        }
    }
}

fn read_query_reply(timeout: Duration) -> QueryReplies {
    let deadline = Instant::now() + timeout;
    let mut parser = vte::Parser::new();
    let mut replies = QueryReplies::default();
    while !replies.device_attributes {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut poll_fd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
        if unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) } <= 0 {
            break;
        }
        let mut buffer = [0u8; 4096];
        let count =
            unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
        match usize::try_from(count) {
            Ok(count) if count > 0 => {
                parser.advance(&mut replies, buffer.get(..count).unwrap_or_default())
            }
            _ => break,
        }
    }
    replies
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum PenColor {
    #[default]
    Default,
    Color(Rgb),
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Pen {
    style: Option<Style>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Style {
    fg: Option<PenColor>,
    bg: PenColor,
    underline: Option<PenColor>,
    attrs: CellAttrs,
}

impl Style {
    fn blank(bg: PenColor) -> Self {
        Self {
            fg: None,
            bg,
            underline: None,
            attrs: CellAttrs::empty(),
        }
    }

    fn of_cell(cell: &Cell, styled_underlines: bool) -> Self {
        if cell.is_plain_blank() {
            return Self::blank(PenColor::Color(cell.bg));
        }
        let mut attrs = cell.attrs - CellAttrs::WIDE_CONTINUATION;
        if !styled_underlines {
            attrs.remove(CellAttrs::CURLY_UNDERLINE);
        }
        let underline = (styled_underlines && attrs.contains(CellAttrs::UNDERLINE)).then(|| {
            match cell.underline.rgb() {
                Some(color) if color != cell.fg => PenColor::Color(color),
                _ => PenColor::Default,
            }
        });
        Self {
            fg: Some(PenColor::Color(cell.fg)),
            bg: PenColor::Color(cell.bg),
            underline,
            attrs,
        }
    }
}

impl Pen {
    fn write_style(&mut self, output: &mut impl Write, style: Style) -> io::Result<()> {
        if self.style == Some(style) {
            return Ok(());
        }
        output.write_all(b"\x1b[0")?;
        for (flag, on) in ATTRIBUTE_CODES {
            if style.attrs.contains(flag) {
                write!(output, ";{on}")?;
            }
        }
        if let Some(code) = underline_code(style.attrs) {
            write!(output, ";{code}")?;
        }
        for (layer, color) in [
            (Layer::Foreground, style.fg),
            (Layer::Background, Some(style.bg)),
            (Layer::Underline, style.underline),
        ] {
            if let Some(PenColor::Color(Rgb { r, g, b })) = color {
                write!(output, ";{};2;{r};{g};{b}", layer.extended_code())?;
            }
        }
        output.write_all(b"m")?;
        self.style = Some(style);
        Ok(())
    }
}

fn underline_code(attrs: CellAttrs) -> Option<&'static str> {
    if !attrs.contains(CellAttrs::UNDERLINE) {
        None
    } else if attrs.contains(CellAttrs::CURLY_UNDERLINE) {
        Some("4:3")
    } else {
        Some("4")
    }
}

struct Terminal<W: Write> {
    output: W,
    body: Vec<u8>,
    cols: u16,
    rows: u16,
    pen: Pen,
    cursor: Option<(usize, u16)>,
    cursor_visible: bool,
    cursor_shape: Option<CursorShape>,
    features: TerminalFeatures,
    pointer: Option<&'static str>,
}

impl<W: Write> Terminal<W> {
    fn clear(&mut self) -> io::Result<()> {
        self.body.write_all(b"\x1b[m\x1b[2J")?;
        self.pen = Pen::default();
        self.cursor = None;
        Ok(())
    }

    fn flush_frame(&mut self) -> io::Result<()> {
        if self.body.is_empty() {
            return Ok(());
        }
        crossterm::queue!(self.output, terminal::BeginSynchronizedUpdate)?;
        self.output.write_all(&self.body)?;
        self.body.clear();
        crossterm::queue!(self.output, terminal::EndSynchronizedUpdate)?;
        self.output.flush()
    }

    fn move_to(&mut self, col: usize, row: u16) -> io::Result<()> {
        if self.cursor == Some((col, row)) {
            return Ok(());
        }
        write!(self.body, "\x1b[{};{}H", row + 1, col + 1)?;
        self.cursor = Some((col, row));
        Ok(())
    }

    fn draw(&mut self, row: u16, cells: &[Cell], range: Range<usize>) -> io::Result<()> {
        let mut scratch = DrawScratch::default();
        self.plan_draw(cells, range, &mut scratch)?;
        for segment in &scratch.segments {
            self.draw_segment(row, segment, &scratch.text)?;
        }
        Ok(())
    }

    fn plan_draw(
        &self,
        cells: &[Cell],
        range: Range<usize>,
        scratch: &mut DrawScratch,
    ) -> io::Result<()> {
        let cols = self.cols as usize;
        let visible_cols = cells.len().min(cols);
        let mut col = range.start;
        while col < range.end {
            let Some(cell) = cells.get(col) else {
                break;
            };
            if cell.is_wide_continuation() {
                col += 1;
                continue;
            }
            let is_wide = cells
                .get(col + 1)
                .is_some_and(|next| next.is_wide_continuation());
            let (glyph, width) = match (is_wide, col + 2 <= visible_cols) {
                (true, true) => (cell.glyph, 2),
                (true, false) => (Glyph::from_char(' '), 1),
                (false, _) => (cell.glyph, 1),
            };
            let next_col = col + width;
            let character = glyph.as_char();
            let text_start = scratch.text.len();
            match character {
                Some(character) => scratch
                    .text
                    .extend_from_slice(character.encode_utf8(&mut [0; 4]).as_bytes()),
                None => glyph.write_to(&mut scratch.text)?,
            }
            let text_end = scratch.text.len();
            let cursor_after = (next_col < cols && character.is_some()).then_some(next_col);
            let style = Style::of_cell(cell, self.features.ghostty);
            match scratch.segments.last_mut() {
                Some(DrawSegment {
                    style: last_style,
                    end,
                    cursor_after: last_cursor_after,
                    ..
                }) if *last_cursor_after == Some(col) && *last_style == style => {
                    *end = text_end;
                    *last_cursor_after = cursor_after;
                }
                _ => {
                    scratch.segments.push(DrawSegment {
                        col,
                        style,
                        start: text_start,
                        end: text_end,
                        cursor_after,
                    });
                }
            }
            col += 1;
        }
        Ok(())
    }

    fn draw_segment(&mut self, row: u16, segment: &DrawSegment, text: &[u8]) -> io::Result<()> {
        self.move_to(segment.col, row)?;
        self.pen.write_style(&mut self.body, segment.style)?;
        self.body
            .extend_from_slice(text.get(segment.start..segment.end).unwrap_or_default());
        self.cursor = segment.cursor_after.map(|col| (col, row));
        Ok(())
    }
}

#[derive(Default)]
struct DrawScratch {
    segments: Vec<DrawSegment>,
    text: Vec<u8>,
}

#[derive(Clone, Copy)]
struct DrawSegment {
    col: usize,
    style: Style,
    start: usize,
    end: usize,
    cursor_after: Option<usize>,
}

struct Renderer<W: Write> {
    terminal: Terminal<W>,
    grid: Option<CellGrid>,
    drawn_size: Option<(u16, u16)>,
    decoder: FrameDecoder,
}

impl<W: Write> Renderer<W> {
    fn new(output: W, cols: u16, rows: u16) -> Self {
        Self {
            terminal: Terminal {
                output,
                body: Vec::new(),
                cols,
                rows,
                pen: Pen::default(),
                cursor: None,
                cursor_visible: false,
                cursor_shape: None,
                features: TerminalFeatures::default(),
                pointer: None,
            },
            grid: None,
            drawn_size: None,
            decoder: FrameDecoder,
        }
    }

    fn apply(&mut self, message: &ServerMessage) -> io::Result<bool> {
        match message {
            ServerMessage::FullFrame(..) => {
                self.decoder.apply(&mut self.grid, message);
                return Ok(true);
            }
            ServerMessage::Clipboard(text) => self.copy_to_clipboard(text)?,
            ServerMessage::Title(title) => self.set_title(title)?,
            ServerMessage::Pointer(style) => self.set_pointer(*style)?,
            ServerMessage::Shutdown | ServerMessage::Error(_) => {}
        }
        Ok(false)
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.terminal.cols = cols;
        self.terminal.rows = rows;
        self.drawn_size = None;
    }

    fn grid_fills_terminal(&self) -> bool {
        self.grid
            .as_ref()
            .is_some_and(|grid| grid.cols == self.terminal.cols && grid.rows == self.terminal.rows)
    }

    fn render(&mut self) -> io::Result<()> {
        let Some(grid) = &self.grid else {
            return Ok(());
        };
        let terminal = &mut self.terminal;
        if self.drawn_size != Some((grid.cols, grid.rows)) {
            terminal.clear()?;
            self.drawn_size = Some((grid.cols, grid.rows));
        }
        let visible_cols = (grid.cols.min(terminal.cols)) as usize;
        let visible_rows = grid.rows.min(terminal.rows) as usize;
        for row in 0..visible_rows as u16 {
            terminal.draw(row, grid.row(row), 0..visible_cols)?;
        }
        let caret = grid.cursor.filter(|cursor| {
            (cursor.col as usize) < visible_cols && (cursor.row as usize) < visible_rows
        });
        match caret {
            Some(caret) => {
                let target = (caret.col as usize, caret.row);
                if terminal.cursor != Some(target) || !terminal.cursor_visible {
                    terminal.move_to(target.0, target.1)?;
                }
                if terminal.cursor_shape != Some(grid.cursor_shape) {
                    terminal
                        .body
                        .write_all(cursor_shape_sequence(grid.cursor_shape))?;
                    terminal.cursor_shape = Some(grid.cursor_shape);
                }
                if !terminal.cursor_visible {
                    crossterm::queue!(terminal.body, cursor::Show)?;
                    terminal.cursor_visible = true;
                }
            }
            None if terminal.cursor_visible => {
                crossterm::queue!(terminal.body, cursor::Hide)?;
                terminal.cursor_visible = false;
            }
            None => {}
        }
        terminal.flush_frame()
    }

    fn set_title(&mut self, title: &str) -> io::Result<()> {
        let title: String = title.chars().filter(|ch| !ch.is_control()).collect();
        write!(self.terminal.output, "\x1b]2;{title}\x07")?;
        self.terminal.output.flush()
    }

    fn copy_to_clipboard(&mut self, text: &str) -> io::Result<()> {
        crossterm::queue!(
            self.terminal.output,
            crossterm::clipboard::CopyToClipboard::to_clipboard_from(text)
        )?;
        self.terminal.output.flush()
    }

    fn set_pointer(&mut self, style: CursorStyle) -> io::Result<()> {
        let name = pointer_name(style);
        if !self.terminal.features.ghostty || self.terminal.pointer == Some(name) {
            return Ok(());
        }
        self.terminal.pointer = Some(name);
        write!(self.terminal.output, "\x1b]22;{name}\x1b\\")?;
        self.terminal.output.flush()
    }
}

fn cursor_shape_sequence(shape: CursorShape) -> &'static [u8] {
    match shape {
        CursorShape::Block => b"\x1b[2 q",
        CursorShape::Underline => b"\x1b[4 q",
        CursorShape::Bar => b"\x1b[6 q",
    }
}

fn pointer_name(style: CursorStyle) -> &'static str {
    match style {
        CursorStyle::Arrow => "default",
        CursorStyle::IBeam => "text",
        CursorStyle::Crosshair => "crosshair",
        CursorStyle::ClosedHand => "grabbing",
        CursorStyle::OpenHand => "grab",
        CursorStyle::PointingHand => "pointer",
        CursorStyle::ResizeLeft => "w-resize",
        CursorStyle::ResizeRight => "e-resize",
        CursorStyle::ResizeLeftRight => "ew-resize",
        CursorStyle::ResizeUp => "n-resize",
        CursorStyle::ResizeDown => "s-resize",
        CursorStyle::ResizeUpDown => "ns-resize",
        CursorStyle::ResizeUpLeftDownRight => "nwse-resize",
        CursorStyle::ResizeUpRightDownLeft => "nesw-resize",
        CursorStyle::ResizeColumn => "col-resize",
        CursorStyle::ResizeRow => "row-resize",
        CursorStyle::IBeamCursorForVerticalLayout => "vertical-text",
        CursorStyle::OperationNotAllowed => "not-allowed",
        CursorStyle::DragLink => "alias",
        CursorStyle::DragCopy => "copy",
        CursorStyle::ContextualMenu => "context-menu",
    }
}

enum ClientEvent {
    Terminal(event::Event),
    Exit(Exit),
}

fn connect(socket: &Path, (cols, rows): (u16, u16)) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("connecting to {}", socket.display()))?;
    write_message(
        &mut stream,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            cols,
            rows,
        },
    )?;
    Ok(stream)
}

fn exit_of(message: &ServerMessage) -> Option<Exit> {
    match message {
        ServerMessage::Shutdown => Some(Exit::ServerShutdown),
        ServerMessage::Error(error) => Some(Exit::Rejected(error.clone())),
        ServerMessage::FullFrame(..)
        | ServerMessage::Clipboard(_)
        | ServerMessage::Title(_)
        | ServerMessage::Pointer(_) => None,
    }
}

pub fn attach(socket: &Path) -> Result<Exit> {
    let (cols, rows) = terminal::size().context("reading the terminal size")?;
    let stream = connect(socket, (cols, rows))?;
    let socket_writer = Arc::new(Mutex::new(stream.try_clone()?));
    let mut reader = BufReader::new(stream);

    let guard = TerminalGuard::enter()?;
    let mut renderer = Renderer::new(io::stdout(), cols, rows);
    renderer.terminal.features = guard.0.features;
    let (render_sender, render_receiver) = mpsc::channel();
    thread::Builder::new()
        .name("Socket reader".to_owned())
        .spawn({
            let render_sender = render_sender.clone();
            move || {
                while let Ok(message) = read_message::<ServerMessage>(&mut reader) {
                    if render_sender.send(RenderEvent::Server(message)).is_err() {
                        break;
                    }
                }
            }
        })?;
    let (event_sender, events) = mpsc::channel();
    thread::Builder::new().name("Render".to_owned()).spawn({
        let event_sender = event_sender.clone();
        move || {
            let exit = render_messages(&mut renderer, &render_receiver);
            event_sender.send(ClientEvent::Exit(exit)).ok();
        }
    })?;
    thread::Builder::new()
        .name("Terminal input".to_owned())
        .spawn(move || {
            while let Ok(event) = event::read() {
                if event_sender.send(ClientEvent::Terminal(event)).is_err() {
                    return;
                }
            }
            event_sender
                .send(ClientEvent::Exit(Exit::Disconnected))
                .ok();
        })?;

    let mut detach_pending = false;
    while let Ok(first) = events.recv() {
        let mut inputs = Vec::new();
        let mut resize = None;
        for event in std::iter::once(first).chain(events.try_iter()) {
            let event = match event {
                ClientEvent::Exit(exit) => return Ok(exit),
                ClientEvent::Terminal(event) => event,
            };
            match event {
                event::Event::Key(key) => {
                    if key.kind == event::KeyEventKind::Release {
                        continue;
                    }
                    let was_pending = std::mem::take(&mut detach_pending);
                    if !was_pending && is_detach_prefix(&key) {
                        detach_pending = true;
                        continue;
                    }
                    if was_pending && key.code == event::KeyCode::Char('d') {
                        let mut socket = socket_writer.lock();
                        for input in inputs {
                            write_message(&mut *socket, &ClientMessage::Input(input)).ok();
                        }
                        write_message(&mut *socket, &ClientMessage::Detach).ok();
                        return Ok(Exit::Detached);
                    }
                    inputs.extend(term_key(&key));
                }
                event::Event::Mouse(mouse) => inputs.push(term_mouse(&mouse)),
                event::Event::Paste(text) => inputs.push(TermEvent::Paste(text)),
                event::Event::Resize(cols, rows) => resize = Some((cols, rows)),
                event::Event::FocusGained | event::Event::FocusLost => {}
            }
        }
        let mut messages: Vec<ClientMessage> =
            inputs.into_iter().map(ClientMessage::Input).collect();
        if let Some((cols, rows)) = resize {
            render_sender.send(RenderEvent::Resized(cols, rows)).ok();
            messages.push(ClientMessage::Resize { cols, rows });
        }
        for message in &messages {
            if write_message(&mut *socket_writer.lock(), message).is_err() {
                return Ok(Exit::Disconnected);
            }
        }
    }
    Ok(Exit::Disconnected)
}

enum RenderEvent {
    Server(ServerMessage),
    Resized(u16, u16),
}

fn render_messages<W: Write>(
    renderer: &mut Renderer<W>,
    events: &mpsc::Receiver<RenderEvent>,
) -> Exit {
    let mut resize_deadline: Option<Instant> = None;
    loop {
        let timeout = resize_deadline.map_or(Duration::MAX, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        });
        let event = match events.recv_timeout(timeout) {
            Ok(event) => Some(event),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Exit::Disconnected,
        };
        let mut is_frame = false;
        match event {
            Some(RenderEvent::Resized(cols, rows)) => {
                renderer.resize(cols, rows);
                resize_deadline = Some(Instant::now() + RESIZE_FRAME_WAIT);
            }
            Some(RenderEvent::Server(message)) => {
                if let Some(exit) = exit_of(&message) {
                    return exit;
                }
                match renderer.apply(&message) {
                    Ok(frame) => is_frame = frame,
                    Err(_) => return Exit::Disconnected,
                }
            }
            None => {}
        }
        let resized_frame_arrived = is_frame && renderer.grid_fills_terminal();
        resize_deadline =
            resize_deadline.filter(|deadline| !resized_frame_arrived && Instant::now() < *deadline);
        if resize_deadline.is_none() && renderer.render().is_err() {
            return Exit::Disconnected;
        }
    }
}

fn is_detach_prefix(key: &event::KeyEvent) -> bool {
    key.modifiers.contains(event::KeyModifiers::CONTROL)
        && matches!(
            key.code,
            event::KeyCode::Char('\\') | event::KeyCode::Char('4')
        )
}

fn term_modifiers(modifiers: event::KeyModifiers) -> Modifiers {
    Modifiers {
        control: modifiers.contains(event::KeyModifiers::CONTROL),
        alt: modifiers.contains(event::KeyModifiers::ALT),
        shift: modifiers.contains(event::KeyModifiers::SHIFT),
        platform: modifiers.contains(event::KeyModifiers::SUPER),
        function: false,
    }
}

fn term_key(key: &event::KeyEvent) -> Option<TermEvent> {
    let code = match key.code {
        event::KeyCode::Char(ch) => KeyCode::Char(ch),
        event::KeyCode::Enter => KeyCode::Enter,
        event::KeyCode::Esc => KeyCode::Escape,
        event::KeyCode::Backspace => KeyCode::Backspace,
        event::KeyCode::Tab => KeyCode::Tab,
        event::KeyCode::BackTab => KeyCode::BackTab,
        event::KeyCode::Left => KeyCode::Left,
        event::KeyCode::Right => KeyCode::Right,
        event::KeyCode::Up => KeyCode::Up,
        event::KeyCode::Down => KeyCode::Down,
        event::KeyCode::Home => KeyCode::Home,
        event::KeyCode::End => KeyCode::End,
        event::KeyCode::PageUp => KeyCode::PageUp,
        event::KeyCode::PageDown => KeyCode::PageDown,
        event::KeyCode::Insert => KeyCode::Insert,
        event::KeyCode::Delete => KeyCode::Delete,
        event::KeyCode::F(number) => KeyCode::Function(number),
        _ => return None,
    };
    Some(TermEvent::Key {
        code,
        modifiers: term_modifiers(key.modifiers),
    })
}

fn term_button(button: event::MouseButton) -> MouseButtonKind {
    match button {
        event::MouseButton::Left => MouseButtonKind::Left,
        event::MouseButton::Right => MouseButtonKind::Right,
        event::MouseButton::Middle => MouseButtonKind::Middle,
    }
}

fn term_mouse(mouse: &event::MouseEvent) -> TermEvent {
    let action = match mouse.kind {
        event::MouseEventKind::Down(button) => MouseAction::Down(term_button(button)),
        event::MouseEventKind::Up(button) => MouseAction::Up(term_button(button)),
        event::MouseEventKind::Drag(button) => MouseAction::Drag(term_button(button)),
        event::MouseEventKind::Moved => MouseAction::Moved,
        event::MouseEventKind::ScrollUp => MouseAction::ScrollUp,
        event::MouseEventKind::ScrollDown => MouseAction::ScrollDown,
        event::MouseEventKind::ScrollLeft => MouseAction::ScrollLeft,
        event::MouseEventKind::ScrollRight => MouseAction::ScrollRight,
    };
    TermEvent::Mouse {
        action,
        col: mouse.column,
        row: mouse.row,
        modifiers: term_modifiers(mouse.modifiers),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::{Random, text_row};
    use alacritty_terminal::{
        Term,
        event::VoidListener,
        index::{Column, Line},
        term::{Config, TermMode, cell::Flags, test::TermSize},
        vte::ansi::{self, Processor},
    };
    use gpui_tui::{CursorPosition, UnderlineColor};

    const WIDE: [char; 2] = ['한', '글'];

    fn is_wide(cell: &Cell) -> bool {
        cell.glyph.cells() == 2
    }

    struct Emulator {
        term: Term<VoidListener>,
        processor: Processor,
        styled_underlines: bool,
    }

    impl Emulator {
        fn new(cols: u16, rows: u16) -> Self {
            Self {
                term: Term::new(
                    Config::default(),
                    &TermSize::new(cols as usize, rows as usize),
                    VoidListener,
                ),
                processor: Processor::new(),
                styled_underlines: false,
            }
        }

        fn feed(&mut self, renderer: &mut Renderer<Vec<u8>>) -> usize {
            renderer.render().unwrap();
            let bytes = std::mem::take(&mut renderer.terminal.output);
            self.processor.advance(&mut self.term, &bytes);
            bytes.len()
        }

        fn assert_shows(&self, grid: &CellGrid, cols: u16) {
            let spec = |rgb: Rgb| {
                ansi::Color::Spec(ansi::Rgb {
                    r: rgb.r,
                    g: rgb.g,
                    b: rgb.b,
                })
            };
            for row in 0..grid.rows {
                for col in 0..grid.cols.min(cols) {
                    let Some(expected) = grid.cell(col as i32, row as i32) else {
                        continue;
                    };
                    let shown = &self.term.grid()[Line(row as i32)][Column(col as usize)];
                    let at = format!("({col}, {row}) {expected:?} shown as {shown:?}");
                    if expected.is_wide_continuation() {
                        assert!(shown.flags.contains(Flags::WIDE_CHAR_SPACER), "{at}");
                        continue;
                    }
                    let clipped = is_wide(expected) && col + 1 == cols;
                    let mut shown_text = String::from(shown.c);
                    shown_text.extend(shown.zerowidth().into_iter().flatten());
                    let mut expected_text = String::new();
                    if clipped {
                        expected_text.push(' ');
                    } else {
                        expected.glyph.push_to(&mut expected_text);
                    }
                    assert_eq!(shown_text, expected_text, "{at}");
                    assert_eq!(shown.bg, spec(expected.bg), "{at}");
                    let underline = expected.attrs.contains(CellAttrs::UNDERLINE);
                    let curly = underline
                        && self.styled_underlines
                        && expected.attrs.contains(CellAttrs::CURLY_UNDERLINE);
                    assert_eq!(
                        shown.flags.contains(Flags::UNDERLINE),
                        underline && !curly,
                        "{at}"
                    );
                    assert_eq!(shown.flags.contains(Flags::UNDERCURL), curly, "{at}");
                    if underline {
                        let color = expected
                            .underline
                            .rgb()
                            .filter(|color| self.styled_underlines && *color != expected.fg);
                        assert_eq!(shown.underline_color(), color.map(spec), "{at}");
                    }
                    if !expected.is_plain_blank() {
                        assert_eq!(shown.fg, spec(expected.fg), "{at}");
                        let bold = expected.attrs.contains(CellAttrs::BOLD);
                        assert_eq!(shown.flags.contains(Flags::BOLD), bold, "{at}");
                        let italic = expected.attrs.contains(CellAttrs::ITALIC);
                        assert_eq!(shown.flags.contains(Flags::ITALIC), italic, "{at}");
                    }
                }
            }
        }
    }

    fn mutate(grid: &mut CellGrid, random: &mut Random, edits: usize) {
        let palette = [
            Rgb::new(30, 33, 40),
            Rgb::new(40, 44, 52),
            Rgb::new(200, 120, 60),
        ];
        let glyphs = [
            Glyph::from_char('a'),
            Glyph::from_char('b'),
            Glyph::from_char('─'),
            Glyph::from_char('│'),
            Glyph::from_char(' '),
            Glyph::from_char(' '),
            Glyph::from_char(' '),
            Glyph::from_char(WIDE[0]),
            Glyph::from_char(WIDE[1]),
            Glyph::from_cluster("e\u{301}"),
        ];
        let attrs = [
            CellAttrs::empty(),
            CellAttrs::empty(),
            CellAttrs::BOLD,
            CellAttrs::ITALIC,
            CellAttrs::UNDERLINE,
            CellAttrs::UNDERLINE | CellAttrs::CURLY_UNDERLINE,
        ];
        let underlines = [
            UnderlineColor::default(),
            UnderlineColor::of(Rgb::new(224, 108, 117)),
            UnderlineColor::of(Rgb::new(200, 120, 60)),
        ];
        for _ in 0..edits {
            let row = random.next(grid.rows as usize) as i32;
            let start = random.next(grid.cols as usize) as i32;
            let length = 1 + random.next(12) as i32;
            let bg = palette[random.next(palette.len())];
            for col in start..start + length {
                if let Some(cell) = grid.cell_mut(col, row) {
                    *cell = Cell {
                        glyph: glyphs[random.next(glyphs.len())],
                        fg: palette[random.next(palette.len())],
                        bg,
                        attrs: attrs[random.next(attrs.len())],
                        underline: underlines[random.next(underlines.len())],
                    };
                }
            }
        }
        let last_col = grid.cols as i32 - 1;
        for row in 0..grid.rows as i32 {
            for col in 0..=last_col {
                let lead_is_wide = grid.cell(col - 1, row).is_some_and(is_wide);
                let Some(cell) = grid.cell_mut(col, row) else {
                    continue;
                };
                if lead_is_wide {
                    *cell = Cell {
                        glyph: ' '.into(),
                        attrs: CellAttrs::WIDE_CONTINUATION,
                        ..*cell
                    };
                } else if cell.is_wide_continuation() || (is_wide(cell) && col == last_col) {
                    *cell = Cell::blank(cell.bg);
                }
            }
        }
    }

    fn shift_within_rows(grid: &mut CellGrid, random: &mut Random) {
        let cols = grid.cols as usize;
        for _ in 0..3 {
            let row = random.next(grid.rows as usize) as u16;
            let start = random.next(cols / 2);
            let distance = 1 + random.next(8);
            let end = if random.next(2) == 0 {
                cols
            } else {
                cols - 1 - random.next(4)
            };
            let cells = &mut grid.row_mut(row)[start..end];
            if random.next(2) == 0 {
                cells.rotate_right(distance.min(cells.len()));
            } else {
                cells.rotate_left(distance.min(cells.len()));
            }
        }
        mutate(grid, random, 0);
    }

    fn output_text(renderer: &Renderer<Vec<u8>>) -> String {
        String::from_utf8(renderer.terminal.output.clone()).unwrap()
    }

    fn render_checked(
        renderer: &mut Renderer<Vec<u8>>,
        emulator: &mut Emulator,
        grid: &CellGrid,
    ) -> String {
        renderer.grid = Some(grid.clone());
        renderer.render().unwrap();
        let output = output_text(renderer);
        emulator.feed(renderer);
        emulator.assert_shows(grid, grid.cols);
        output
    }

    fn parse_replies(chunks: &[&[u8]]) -> QueryReplies {
        let mut parser = vte::Parser::new();
        let mut replies = QueryReplies::default();
        for chunk in chunks {
            parser.advance(&mut replies, chunk);
        }
        replies
    }

    struct FailingWriter {
        written: Vec<u8>,
        flushes: usize,
        fail_on_flush: usize,
    }

    impl FailingWriter {
        fn failing_on_flush(fail_on_flush: usize) -> Self {
            Self {
                written: Vec::new(),
                flushes: 0,
                fail_on_flush,
            }
        }
    }

    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.flushes == self.fail_on_flush {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            Ok(())
        }
    }

    fn undo_after_setup(
        fail_on_flush: usize,
        query: impl FnOnce(&mut FailingWriter) -> io::Result<QueryReplies>,
    ) -> (io::Result<()>, String) {
        let mut setup = TerminalSetup::default();
        let result = setup.run(&mut FailingWriter::failing_on_flush(fail_on_flush), query);
        let mut undo = Vec::new();
        setup.undo(&mut undo);
        (result, String::from_utf8(undo).unwrap())
    }

    const LEAVE_ALTERNATE_SCREEN: &str = "\x1b[?1049l";
    const POP_KEYBOARD_FLAGS: &str = "\x1b[<1u";

    fn keyboard_replies(_: &mut FailingWriter) -> io::Result<QueryReplies> {
        Ok(QueryReplies {
            keyboard_flags: true,
            ..QueryReplies::default()
        })
    }

    #[test]
    fn a_failed_startup_undoes_only_the_steps_it_started() {
        let (result, undo) = undo_after_setup(1, keyboard_replies);
        assert!(result.is_err());
        assert!(undo.contains(LEAVE_ALTERNATE_SCREEN) && undo.ends_with(POP_TITLE));
        assert!(!undo.contains(POP_KEYBOARD_FLAGS));

        let (result, undo) = undo_after_setup(0, |_| Err(io::ErrorKind::TimedOut.into()));
        assert!(result.is_err());
        assert!(undo.contains(LEAVE_ALTERNATE_SCREEN) && undo.ends_with(POP_TITLE));
        assert!(!undo.contains(POP_KEYBOARD_FLAGS));

        let (result, undo) = undo_after_setup(2, keyboard_replies);
        assert!(result.is_err());
        assert!(undo.contains(LEAVE_ALTERNATE_SCREEN));
        assert!(!undo.contains(POP_KEYBOARD_FLAGS));
    }

    #[test]
    fn a_completed_startup_restores_every_step() {
        let (result, undo) = undo_after_setup(0, keyboard_replies);
        assert!(result.is_ok());
        assert!(undo.contains(POP_KEYBOARD_FLAGS));
        assert!(undo.contains(LEAVE_ALTERNATE_SCREEN) && undo.ends_with(POP_TITLE));
    }

    #[test]
    fn clipboard_uses_osc52_with_st() {
        let mut renderer = Renderer::new(Vec::new(), 10, 2);
        renderer
            .apply(&ServerMessage::Clipboard("hi".into()))
            .unwrap();
        assert_eq!(renderer.terminal.output, b"\x1b]52;c;aGk=\x1b\\");
    }

    #[test]
    fn pointer_shapes_use_osc22_with_st_only_on_change() {
        let mut renderer = Renderer::new(Vec::new(), 10, 2);
        renderer.terminal.features.ghostty = true;
        for style in [
            CursorStyle::IBeam,
            CursorStyle::IBeam,
            CursorStyle::PointingHand,
        ] {
            renderer.apply(&ServerMessage::Pointer(style)).unwrap();
        }
        assert_eq!(
            renderer.terminal.output,
            b"\x1b]22;text\x1b\\\x1b]22;pointer\x1b\\"
        );
    }

    #[test]
    fn pointer_shapes_are_not_sent_to_other_terminals() {
        let mut renderer = Renderer::new(Vec::new(), 10, 2);
        renderer
            .apply(&ServerMessage::Pointer(CursorStyle::PointingHand))
            .unwrap();
        assert!(renderer.terminal.output.is_empty());
    }

    #[test]
    fn every_cursor_style_maps_to_a_ghostty_shape_name() {
        const GHOSTTY_W3C_SHAPES: [&str; 34] = [
            "default",
            "context-menu",
            "help",
            "pointer",
            "progress",
            "wait",
            "cell",
            "crosshair",
            "text",
            "vertical-text",
            "alias",
            "copy",
            "move",
            "no-drop",
            "not-allowed",
            "grab",
            "grabbing",
            "all-scroll",
            "col-resize",
            "row-resize",
            "n-resize",
            "e-resize",
            "s-resize",
            "w-resize",
            "ne-resize",
            "nw-resize",
            "se-resize",
            "sw-resize",
            "ew-resize",
            "ns-resize",
            "nesw-resize",
            "nwse-resize",
            "zoom-in",
            "zoom-out",
        ];
        let styles = [
            CursorStyle::Arrow,
            CursorStyle::IBeam,
            CursorStyle::Crosshair,
            CursorStyle::ClosedHand,
            CursorStyle::OpenHand,
            CursorStyle::PointingHand,
            CursorStyle::ResizeLeft,
            CursorStyle::ResizeRight,
            CursorStyle::ResizeLeftRight,
            CursorStyle::ResizeUp,
            CursorStyle::ResizeDown,
            CursorStyle::ResizeUpDown,
            CursorStyle::ResizeUpLeftDownRight,
            CursorStyle::ResizeUpRightDownLeft,
            CursorStyle::ResizeColumn,
            CursorStyle::ResizeRow,
            CursorStyle::IBeamCursorForVerticalLayout,
            CursorStyle::OperationNotAllowed,
            CursorStyle::DragLink,
            CursorStyle::DragCopy,
            CursorStyle::ContextualMenu,
        ];
        for style in styles {
            assert!(
                GHOSTTY_W3C_SHAPES.contains(&pointer_name(style)),
                "{style:?} maps to {:?}",
                pointer_name(style)
            );
        }
    }

    #[test]
    fn up_left_down_right_resizes_use_the_nwse_shape() {
        assert_eq!(
            pointer_name(CursorStyle::ResizeUpLeftDownRight),
            "nwse-resize"
        );
        assert_eq!(
            pointer_name(CursorStyle::ResizeUpRightDownLeft),
            "nesw-resize"
        );
    }

    #[test]
    fn ghostty_pointer_is_reset_on_exit() {
        let contains_reset = |bytes: &[u8]| {
            bytes
                .windows(POINTER_RESET.len())
                .any(|window| window == POINTER_RESET)
        };
        for ghostty in [true, false] {
            let mut setup = TerminalSetup::default();
            setup
                .run(&mut FailingWriter::failing_on_flush(0), |_| {
                    Ok(QueryReplies {
                        features: TerminalFeatures { ghostty },
                        ..QueryReplies::default()
                    })
                })
                .unwrap();
            let mut undo = Vec::new();
            setup.undo(&mut undo);
            assert_eq!(contains_reset(&undo), ghostty, "{undo:?}");
        }
    }

    #[test]
    fn keyboard_flags_come_from_the_flags_report() {
        assert!(parse_replies(&[b"\x1b[?0u\x1b[?62;22c"]).keyboard_flags);
        assert!(parse_replies(&[b"\x1b[?69;2$y\x1b[?15u\x1b[?62c"]).keyboard_flags);
        assert!(!parse_replies(&[b"\x1b[?62;22c"]).keyboard_flags);
    }

    #[test]
    fn device_attributes_end_the_query_reply() {
        assert!(parse_replies(&[b"\x1b[?62;22c"]).device_attributes);
        assert!(!parse_replies(&[b"\x1b[?62;22"]).device_attributes);
    }

    #[test]
    fn ghostty_comes_from_the_version_report() {
        assert!(
            parse_replies(&[b"\x1bP>|ghostty 1.2.0\x1b\\\x1b[?62;22c"])
                .features
                .ghostty
        );
        assert!(
            parse_replies(&[b"\x1bP>|libghostty\x1b\\\x1b[?62;22c"])
                .features
                .ghostty
        );
        assert!(
            !parse_replies(&[b"\x1bP>|XTerm(390)\x1b\\\x1b[?64;1;28c"])
                .features
                .ghostty
        );
        assert!(
            !parse_replies(&[b"\x1bP>|tmux 3.5a\x1b\\\x1b[?62;22c"])
                .features
                .ghostty
        );
        assert!(
            !parse_replies(&[b"\x1bP1$r0m\x1b\\ghostty\x1b[?62;22c"])
                .features
                .ghostty
        );
        assert!(!parse_replies(&[b"\x1b[?62;22c"]).features.ghostty);
    }

    #[test]
    fn rendered_frames_match_an_emulated_terminal() {
        let mut curled = false;
        let mut colored_underlines = false;
        let cases = [(1, 40), (2, 40), (3, 33), (4, 27), (5, 44), (6, 40)];
        for (ghostty, (seed, terminal_cols)) in [false, true]
            .into_iter()
            .flat_map(|ghostty| cases.map(|case| (ghostty, case)))
        {
            let mut random = Random::new(seed);
            let mut grid = CellGrid::new(40, 12, Rgb::new(40, 44, 52));
            let mut renderer = Renderer::new(Vec::new(), terminal_cols, 12);
            renderer.terminal.features.ghostty = ghostty;
            let mut emulator = Emulator::new(terminal_cols, 12);
            emulator.styled_underlines = ghostty;
            paint_run(&mut grid, &mut random);
            mutate(&mut grid, &mut random, 60);
            renderer.grid = Some(grid.clone());
            emulator.feed(&mut renderer);
            emulator.assert_shows(&grid, terminal_cols);
            for step in 0..20 {
                if step % 3 == 1 {
                    shift_within_rows(&mut grid, &mut random);
                }
                paint_run(&mut grid, &mut random);
                mutate(&mut grid, &mut random, 3);
                renderer.grid = Some(grid.clone());
                renderer.render().unwrap();
                let output = String::from_utf8_lossy(&renderer.terminal.output);
                let styled = output.contains("4:3") || output.contains(";58;");
                assert!(ghostty || !styled, "{output:?}");
                curled |= output.contains("4:3");
                colored_underlines |= output.contains(";58;") || output.contains("[58;");
                emulator.feed(&mut renderer);
                emulator.assert_shows(&grid, terminal_cols);
            }
        }
        assert!(curled && colored_underlines);
    }

    fn underlined_output(ghostty: bool) -> String {
        let red = Rgb::new(224, 108, 117);
        let mut grid = CellGrid::new(12, 1, Rgb::new(40, 44, 52));
        text_row(&mut grid, 0, 0, "abcdefgh");
        for (col, cell) in grid.row_mut(0).iter_mut().enumerate().take(8) {
            cell.fg = Rgb::new(200, 200, 200);
            cell.attrs = match col {
                0..2 => CellAttrs::UNDERLINE | CellAttrs::CURLY_UNDERLINE,
                2..4 => CellAttrs::UNDERLINE,
                4..6 => CellAttrs::UNDERLINE | CellAttrs::CURLY_UNDERLINE,
                _ => CellAttrs::empty(),
            };
            if col < 4 {
                cell.underline = UnderlineColor::of(red);
            }
        }
        let mut renderer = Renderer::new(Vec::new(), 12, 1);
        renderer.terminal.features.ghostty = ghostty;
        let mut emulator = Emulator::new(12, 1);
        emulator.styled_underlines = ghostty;
        render_checked(&mut renderer, &mut emulator, &grid)
    }

    #[test]
    fn ghostty_underlines_carry_their_curl_and_color() {
        let output = underlined_output(true);
        assert!(
            output.contains("4:3") && output.contains(";58;2;224;108;117m"),
            "{output:?}"
        );
        let output = underlined_output(false);
        assert!(
            !output.contains("4:3") && !output.contains(";58;"),
            "{output:?}"
        );
    }

    fn paint_run(grid: &mut CellGrid, random: &mut Random) {
        let glyphs = ['─', '=', 'a', WIDE[0]];
        let attrs = [
            CellAttrs::empty(),
            CellAttrs::BOLD,
            CellAttrs::UNDERLINE,
            CellAttrs::UNDERLINE | CellAttrs::CURLY_UNDERLINE,
        ];
        let run = Cell {
            glyph: glyphs[random.next(glyphs.len())].into(),
            fg: Rgb::new(200, 120, 60),
            bg: Rgb::new(30, 33, 40),
            attrs: attrs[random.next(attrs.len())],
            underline: UnderlineColor::of(Rgb::new(224, 108, 117)),
        };
        let row = random.next(grid.rows as usize) as i32;
        let start = random.next(grid.cols as usize) as i32;
        for col in start..start + 2 + random.next(24) as i32 {
            if let Some(cell) = grid.cell_mut(col, row) {
                *cell = run;
            }
        }
    }

    #[test]
    fn combining_marks_reach_the_terminal_with_their_base() {
        let mut grid = CellGrid::new(20, 2, Rgb::new(40, 44, 52));
        text_row(&mut grid, 0, 0, "cafe x");
        if let Some(cell) = grid.cell_mut(3, 0) {
            cell.glyph = Glyph::from_cluster("e\u{301}");
        }
        let mut renderer = Renderer::new(Vec::new(), 20, 2);
        let mut emulator = Emulator::new(20, 2);
        renderer.grid = Some(grid.clone());
        emulator.feed(&mut renderer);
        emulator.assert_shows(&grid, 20);
    }

    #[test]
    fn clusters_the_terminal_splits_do_not_shift_later_cells() {
        let mut grid = CellGrid::new(20, 2, Rgb::new(40, 44, 52));
        if let Some(cell) = grid.cell_mut(0, 0) {
            cell.glyph = Glyph::from_cluster("👩\u{200d}💻");
        }
        if let Some(cell) = grid.cell_mut(1, 0) {
            cell.attrs = CellAttrs::WIDE_CONTINUATION;
        }
        text_row(&mut grid, 0, 2, "XY");
        let mut renderer = Renderer::new(Vec::new(), 20, 2);
        let mut emulator = Emulator::new(20, 2);
        renderer.grid = Some(grid);
        emulator.feed(&mut renderer);
        let shown = |col: usize| emulator.term.grid()[Line(0)][Column(col)].c;
        assert_eq!((shown(2), shown(3)), ('X', 'Y'));
    }

    #[test]
    fn the_terminal_cursor_follows_the_frame_cursor() {
        let mut grid = CellGrid::new(20, 4, Rgb::new(40, 44, 52));
        text_row(&mut grid, 1, 0, "some text");
        let mut renderer = Renderer::new(Vec::new(), 20, 4);
        let mut emulator = Emulator::new(20, 4);
        emulator.processor.advance(&mut emulator.term, b"\x1b[?25l");
        let shown_at = |emulator: &Emulator| {
            let cursor = emulator.term.grid().cursor.point;
            emulator
                .term
                .mode()
                .contains(TermMode::SHOW_CURSOR)
                .then_some((cursor.column.0, cursor.line.0))
        };

        renderer.grid = Some(grid.clone());
        emulator.feed(&mut renderer);
        assert_eq!(shown_at(&emulator), None);

        grid.cursor = Some(CursorPosition { col: 4, row: 1 });
        renderer.grid = Some(grid.clone());
        emulator.feed(&mut renderer);
        assert_eq!(shown_at(&emulator), Some((4, 1)));

        text_row(&mut grid, 3, 0, "more");
        grid.cursor = Some(CursorPosition { col: 9, row: 1 });
        renderer.grid = Some(grid.clone());
        emulator.feed(&mut renderer);
        assert_eq!(shown_at(&emulator), Some((9, 1)));
        emulator.assert_shows(&grid, 20);

        grid.cursor = None;
        renderer.grid = Some(grid.clone());
        emulator.feed(&mut renderer);
        assert_eq!(shown_at(&emulator), None);
    }

    #[test]
    fn the_terminal_cursor_takes_the_frame_cursor_shape() {
        let mut grid = CellGrid::new(20, 4, Rgb::new(40, 44, 52));
        text_row(&mut grid, 1, 0, "some text");
        grid.cursor = Some(CursorPosition { col: 4, row: 1 });
        let mut renderer = Renderer::new(Vec::new(), 20, 4);
        let mut emulator = Emulator::new(20, 4);
        for (shape, expected) in [
            (CursorShape::Underline, ansi::CursorShape::Underline),
            (CursorShape::Block, ansi::CursorShape::Block),
            (CursorShape::Bar, ansi::CursorShape::Beam),
        ] {
            grid.cursor_shape = shape;
            renderer.grid = Some(grid.clone());
            emulator.feed(&mut renderer);
            let style = emulator.term.cursor_style();
            assert_eq!(
                (style.shape, style.blinking),
                (expected, false),
                "{shape:?}"
            );
        }
    }

    #[test]
    fn the_cursor_shape_is_reset_on_exit() {
        let contains_reset = |bytes: &[u8]| {
            bytes
                .windows(CURSOR_SHAPE_RESET.len())
                .any(|window| window == CURSOR_SHAPE_RESET)
        };
        let mut setup = TerminalSetup::default();
        setup
            .run(&mut FailingWriter::failing_on_flush(0), |_| {
                Ok(QueryReplies::default())
            })
            .unwrap();
        let mut undo = Vec::new();
        setup.undo(&mut undo);
        assert!(contains_reset(&undo), "{undo:?}");
    }

    #[derive(Clone, Default)]
    struct SharedOutput(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn full_frame(cols: u16, rows: u16) -> RenderEvent {
        let mut grid = CellGrid::new(cols, rows, Rgb::new(40, 44, 52));
        text_row(&mut grid, 0, 0, "hello");
        RenderEvent::Server(crate::tui::protocol::FrameEncoder.full_frame(&grid))
    }

    #[test]
    fn a_resize_waits_for_the_resized_frame_before_repainting() {
        let output = SharedOutput::default();
        let (sender, receiver) = mpsc::channel();
        let render_thread = thread::spawn({
            let output = output.clone();
            move || {
                let mut renderer = Renderer::new(output, 20, 4);
                render_messages(&mut renderer, &receiver);
            }
        });
        let take_output = || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while output.0.lock().is_empty() {
                assert!(Instant::now() < deadline, "nothing was rendered");
                thread::sleep(Duration::from_millis(5));
            }
            String::from_utf8(std::mem::take(&mut *output.0.lock())).unwrap()
        };

        sender.send(full_frame(20, 4)).unwrap();
        assert!(take_output().contains("hello"));

        sender.send(RenderEvent::Resized(30, 6)).unwrap();
        thread::sleep(RESIZE_FRAME_WAIT / 3);
        assert!(output.0.lock().is_empty());
        sender.send(full_frame(30, 6)).unwrap();
        assert_eq!(take_output().matches("\x1b[2J").count(), 1);

        sender.send(RenderEvent::Resized(25, 5)).unwrap();
        assert!(take_output().contains("\x1b[2J"));

        drop(sender);
        render_thread.join().unwrap();
    }

    #[test]
    fn resizing_redraws_everything() {
        let mut grid = CellGrid::new(20, 4, Rgb::new(40, 44, 52));
        mutate(&mut grid, &mut Random::new(9), 10);
        let mut renderer = Renderer::new(Vec::new(), 20, 4);
        renderer.grid = Some(grid.clone());
        Emulator::new(20, 4).feed(&mut renderer);

        let mut emulator = Emulator::new(20, 4);
        renderer.resize(20, 4);
        emulator.feed(&mut renderer);
        emulator.assert_shows(&grid, 20);
    }
}
