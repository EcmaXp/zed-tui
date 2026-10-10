use std::{
    io::{Read, Write},
    path::PathBuf,
};

use anyhow::{Context as _, Result, bail};
use collections::HashMap;
use gpui::{CursorStyle, Modifiers};
use gpui_tui::{
    Cell, CellAttrs, CellGrid, CursorPosition, CursorShape, Glyph, Rgb, UnderlineColor,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::tui::frame_diff::{GridScroll, changed_ranges, find_moves};

pub const PROTOCOL_VERSION: u32 = 15;
pub const WAIT_ONLY_SIZE: (u16, u16) = (0, 0);
const MAX_MESSAGE_LEN: usize = 64 * 1024 * 1024;
const PATCH_MERGE_GAP: usize = 8;
const CONTINUATION: char = '\0';
const CLUSTER_EXTEND: char = '\u{1}';
const MAX_COLOR_TABLE: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyCode {
    #[serde(rename = "c")]
    Char(char),
    Enter,
    Escape,
    Backspace,
    Tab,
    BackTab,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Function(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButtonKind {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseAction {
    #[serde(rename = "d")]
    Down(MouseButtonKind),
    #[serde(rename = "u")]
    Up(MouseButtonKind),
    #[serde(rename = "g")]
    Drag(MouseButtonKind),
    #[serde(rename = "m")]
    Moved,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TermEvent {
    #[serde(rename = "k")]
    Key {
        #[serde(rename = "c")]
        code: KeyCode,
        #[serde(rename = "m", with = "wire_modifiers")]
        modifiers: Modifiers,
    },
    #[serde(rename = "m")]
    Mouse {
        #[serde(rename = "a")]
        action: MouseAction,
        #[serde(rename = "x")]
        col: u16,
        #[serde(rename = "y")]
        row: u16,
        #[serde(rename = "m", with = "wire_modifiers")]
        modifiers: Modifiers,
    },
    #[serde(rename = "p")]
    Paste(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    Hello {
        version: u32,
        cols: u16,
        rows: u16,
    },
    #[serde(rename = "i")]
    Input(TermEvent),
    #[serde(rename = "z")]
    Resize {
        #[serde(rename = "c")]
        cols: u16,
        #[serde(rename = "r")]
        rows: u16,
    },
    #[serde(rename = "d")]
    Detach,
    #[serde(rename = "k")]
    Kill,
    #[serde(rename = "o")]
    Open {
        #[serde(rename = "p")]
        paths: Vec<PathBuf>,
    },
    #[serde(rename = "r")]
    Rendered(u32),
    #[serde(rename = "w")]
    OpenAndWait {
        #[serde(rename = "p")]
        paths: Vec<PathBuf>,
        #[serde(rename = "q")]
        quit_session: bool,
    },
}

pub fn drop_superseded_moves<T>(events: &mut Vec<T>, input: impl Fn(&T) -> Option<&TermEvent>) {
    events.dedup_by(|next, kept| {
        let is_superseded = matches!(
            (input(kept), input(next)),
            (Some(kept), Some(next)) if is_superseded_by(kept, next)
        );
        if is_superseded {
            std::mem::swap(kept, next);
        }
        is_superseded
    });
}

fn is_superseded_by(event: &TermEvent, next: &TermEvent) -> bool {
    match (event, next) {
        (
            TermEvent::Mouse {
                action, modifiers, ..
            },
            TermEvent::Mouse {
                action: next_action,
                modifiers: next_modifiers,
                ..
            },
        ) => {
            matches!(action, MouseAction::Moved | MouseAction::Drag(_))
                && action == next_action
                && modifiers == next_modifiers
        }
        _ => false,
    }
}

mod wire_modifiers {
    use gpui::Modifiers;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    const CONTROL: u8 = 1;
    const ALT: u8 = 1 << 1;
    const SHIFT: u8 = 1 << 2;
    const PLATFORM: u8 = 1 << 3;
    const FUNCTION: u8 = 1 << 4;

    pub fn serialize<S: Serializer>(
        modifiers: &Modifiers,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        [
            (modifiers.control, CONTROL),
            (modifiers.alt, ALT),
            (modifiers.shift, SHIFT),
            (modifiers.platform, PLATFORM),
            (modifiers.function, FUNCTION),
        ]
        .into_iter()
        .filter(|(pressed, _)| *pressed)
        .fold(0u8, |bits, (_, bit)| bits | bit)
        .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Modifiers, D::Error> {
        let bits = u8::deserialize(deserializer)?;
        Ok(Modifiers {
            control: bits & CONTROL != 0,
            alt: bits & ALT != 0,
            shift: bits & SHIFT != 0,
            platform: bits & PLATFORM != 0,
            function: bits & FUNCTION != 0,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span(u32, u32, u8, String, Option<u32>);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowPatch(u16, u16, Vec<Span>);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireScroll(u16, u16, i16);

impl WireScroll {
    fn from_grid(scroll: &GridScroll) -> Self {
        Self(scroll.top as u16, scroll.bottom as u16, scroll.shift as i16)
    }

    pub(crate) fn to_grid(&self) -> GridScroll {
        let WireScroll(top, bottom, shift) = self;
        GridScroll {
            top: *top as usize,
            bottom: *bottom as usize,
            shift: *shift as isize,
        }
    }
}

fn scroll_fill() -> Cell {
    Cell::blank(Rgb::default())
}

pub type FrameCursor = Option<(CursorPosition, CursorShape)>;

fn frame_cursor(grid: &CellGrid) -> FrameCursor {
    grid.cursor.map(|cursor| (cursor, grid.cursor_shape))
}

fn set_frame_cursor(grid: &mut CellGrid, cursor: FrameCursor) {
    grid.cursor = cursor.map(|(position, _)| position);
    grid.cursor_shape = cursor.map_or(CursorShape::default(), |(_, shape)| shape);
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
    #[serde(rename = "f")]
    FullFrame(
        u16,
        u16,
        Vec<u32>,
        Vec<RowPatch>,
        #[serde(with = "wire_cursor")] FrameCursor,
    ),
    #[serde(rename = "d")]
    Diff(
        Vec<u32>,
        Vec<WireScroll>,
        Vec<RowPatch>,
        #[serde(with = "wire_cursor")] FrameCursor,
    ),
    #[serde(rename = "c")]
    Clipboard(String),
    #[serde(rename = "t")]
    Title(String),
    #[serde(rename = "s")]
    Shutdown,
    #[serde(rename = "e")]
    Error(String),
    #[serde(rename = "w")]
    WaitFinished {
        #[serde(rename = "s")]
        status: i32,
        #[serde(rename = "e")]
        errors: Vec<String>,
    },
    #[serde(rename = "p")]
    Pointer(CursorStyle),
}

mod wire_cursor {
    use gpui_tui::{CursorPosition, CursorShape};
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

    use super::FrameCursor;

    pub fn serialize<S: Serializer>(
        cursor: &FrameCursor,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        cursor
            .map(|(cursor, shape)| {
                let shape: u8 = match shape {
                    CursorShape::Bar => 0,
                    CursorShape::Block => 1,
                    CursorShape::Underline => 2,
                };
                (cursor.col, cursor.row, shape)
            })
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<FrameCursor, D::Error> {
        Option::<(u16, u16, u8)>::deserialize(deserializer)?
            .map(|(col, row, shape)| {
                let shape = match shape {
                    0 => CursorShape::Bar,
                    1 => CursorShape::Block,
                    2 => CursorShape::Underline,
                    unknown => {
                        return Err(D::Error::custom(format!("unknown cursor shape {unknown}")));
                    }
                };
                Ok((CursorPosition { col, row }, shape))
            })
            .transpose()
    }
}

#[derive(Default)]
pub struct MessageWriter {
    buffer: Vec<u8>,
}

impl MessageWriter {
    pub fn push<T: Serialize>(&mut self, message: &T) -> Result<()> {
        let start = self.buffer.len();
        self.buffer.extend_from_slice(&[0; 4]);
        if let Err(error) = ciborium::into_writer(message, &mut self.buffer) {
            self.buffer.truncate(start);
            return Err(error).context("encoding message");
        }
        let len = self.buffer.len() - start - 4;
        let Some(len) = u32::try_from(len).ok().filter(|_| len <= MAX_MESSAGE_LEN) else {
            self.buffer.truncate(start);
            bail!("message of {len} bytes exceeds the limit");
        };
        if let Some(prefix) = self.buffer.get_mut(start..start + 4) {
            prefix.copy_from_slice(&len.to_le_bytes());
        }
        Ok(())
    }

    pub fn flush(&mut self, writer: &mut impl Write) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let result = writer.write_all(&self.buffer);
        self.buffer.clear();
        Ok(result?)
    }
}

#[derive(Default)]
pub struct MessageReader {
    body: Vec<u8>,
}

impl MessageReader {
    pub fn read<T: DeserializeOwned>(&mut self, reader: &mut impl Read) -> Result<T> {
        let mut len = [0; 4];
        reader.read_exact(&mut len)?;
        let len = u32::from_le_bytes(len) as usize;
        if len > MAX_MESSAGE_LEN {
            bail!("message of {len} bytes exceeds the limit");
        }
        self.body.resize(len, 0);
        reader.read_exact(&mut self.body)?;
        ciborium::from_reader(self.body.as_slice()).context("decoding message")
    }
}

pub fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> Result<()> {
    let mut message_writer = MessageWriter::default();
    message_writer.push(message)?;
    message_writer.flush(writer)
}

pub fn read_message<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T> {
    MessageReader::default().read(reader)
}

fn underline_color(cell: &Cell) -> Option<u32> {
    cell.attrs
        .contains(CellAttrs::UNDERLINE)
        .then(|| cell.underline.rgb())
        .flatten()
        .map(u32::from)
}

fn encode_cells(cells: &[Cell]) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut foreground_open = false;
    for cell in cells {
        let is_continuation = cell.is_wide_continuation();
        if let Some(Span(fg, bg, attrs, text, underline)) = spans.last_mut() {
            if is_continuation {
                text.push(CONTINUATION);
                continue;
            }
            let is_blank = cell.is_plain_blank();
            if *bg == u32::from(cell.bg)
                && *attrs == cell.attrs.bits()
                && *underline == underline_color(cell)
                && (is_blank || foreground_open || *fg == u32::from(cell.fg))
            {
                if foreground_open && !is_blank {
                    *fg = u32::from(cell.fg);
                    foreground_open = false;
                }
                push_glyph(text, cell.glyph);
                continue;
            }
        }
        let attrs = cell.attrs - CellAttrs::WIDE_CONTINUATION;
        let mut text = String::new();
        if is_continuation {
            text.push(CONTINUATION);
        } else {
            push_glyph(&mut text, cell.glyph);
        }
        spans.push(Span(
            u32::from(cell.fg),
            u32::from(cell.bg),
            attrs.bits(),
            text,
            underline_color(cell),
        ));
        foreground_open = cell.is_plain_blank() || is_continuation;
    }
    spans
}

fn push_glyph(text: &mut String, glyph: Glyph) {
    glyph.with_str(|cluster| {
        for (index, ch) in cluster.chars().enumerate() {
            if index > 0 {
                text.push(CLUSTER_EXTEND);
            }
            text.push(if ch.is_control() { ' ' } else { ch });
        }
    });
}

fn decode_spans<'a>(
    spans: &'a [Span],
    color: &'a impl Fn(u32) -> Rgb,
) -> impl Iterator<Item = Cell> + 'a {
    spans
        .iter()
        .flat_map(move |Span(fg, bg, attrs, text, underline)| {
            let underline = underline.map_or(UnderlineColor::default(), |index| {
                UnderlineColor::of(color(index))
            });
            let (fg, bg) = (color(*fg), color(*bg));
            let mut chars = text.chars().peekable();
            let mut cluster = String::new();
            std::iter::from_fn(move || {
                let ch = chars.next()?;
                let (glyph, attrs, underline) = if ch == CONTINUATION {
                    (
                        Glyph::from_char(' '),
                        CellAttrs::WIDE_CONTINUATION,
                        UnderlineColor::default(),
                    )
                } else if chars.peek() == Some(&CLUSTER_EXTEND) {
                    cluster.clear();
                    cluster.push(ch);
                    while chars.next_if_eq(&CLUSTER_EXTEND).is_some() {
                        cluster.extend(chars.next());
                    }
                    (
                        Glyph::from_cluster(&cluster),
                        CellAttrs::from_bits_truncate(*attrs),
                        underline,
                    )
                } else {
                    (
                        Glyph::from_char(ch),
                        CellAttrs::from_bits_truncate(*attrs),
                        underline,
                    )
                };
                Some(Cell {
                    glyph,
                    fg,
                    bg,
                    attrs,
                    underline,
                })
            })
        })
}

#[derive(Default)]
pub struct FrameEncoder {
    colors: HashMap<u32, u32>,
    scrolled: Option<CellGrid>,
}

impl FrameEncoder {
    fn index_colors(&mut self, patches: &mut [RowPatch]) -> Vec<u32> {
        let mut added = Vec::new();
        for RowPatch(_, _, spans) in patches {
            for Span(fg, bg, _, _, underline) in spans {
                for color in [fg, bg].into_iter().chain(underline.as_mut()) {
                    let next = self.colors.len() as u32;
                    *color = *self.colors.entry(*color).or_insert_with(|| {
                        added.push(*color);
                        next
                    });
                }
            }
        }
        added
    }

    fn full_frame(&mut self, grid: &CellGrid) -> ServerMessage {
        self.colors.clear();
        let mut patches: Vec<RowPatch> = (0..grid.rows)
            .map(|row| RowPatch(row, 0, encode_cells(grid.row(row))))
            .collect();
        let colors = self.index_colors(&mut patches);
        ServerMessage::FullFrame(grid.cols, grid.rows, colors, patches, frame_cursor(grid))
    }

    pub fn update(
        &mut self,
        previous: Option<&CellGrid>,
        next: &CellGrid,
    ) -> Option<ServerMessage> {
        let previous = match previous {
            Some(previous)
                if previous.cols == next.cols
                    && previous.rows == next.rows
                    && self.colors.len() < MAX_COLOR_TABLE =>
            {
                previous
            }
            _ => return Some(self.full_frame(next)),
        };
        let moves = find_moves(
            previous,
            next,
            next.rows as usize,
            next.cols as usize,
            &mut self.scrolled,
            |scroll, grid| scroll.apply(grid, scroll_fill()),
        );
        let base = match &self.scrolled {
            Some(scrolled) if !moves.is_empty() => scrolled,
            _ => previous,
        };
        let mut patches = Vec::new();
        for row in 0..next.rows {
            let cells = next.row(row);
            for range in changed_ranges(base.row(row), cells, PATCH_MERGE_GAP) {
                if let Some(changed) = cells.get(range.clone()) {
                    patches.push(RowPatch(row, range.start as u16, encode_cells(changed)));
                }
            }
        }
        if moves.is_empty() && patches.is_empty() && frame_cursor(previous) == frame_cursor(next) {
            return None;
        }
        let colors = self.index_colors(&mut patches);
        Some(ServerMessage::Diff(
            colors,
            moves.iter().map(WireScroll::from_grid).collect(),
            patches,
            frame_cursor(next),
        ))
    }
}

#[derive(Default)]
pub struct FrameDecoder {
    colors: Vec<u32>,
}

impl FrameDecoder {
    fn apply_patches(&self, grid: &mut CellGrid, patches: &[RowPatch]) {
        let color = |index: u32| {
            self.colors
                .get(index as usize)
                .copied()
                .map_or(Rgb::default(), Rgb::from)
        };
        for RowPatch(row, col, spans) in patches {
            for (offset, cell) in decode_spans(spans, &color).enumerate() {
                if let Some(target) = grid.cell_mut(*col as i32 + offset as i32, *row as i32) {
                    *target = cell;
                }
            }
        }
    }

    pub fn apply(
        &mut self,
        grid: &mut Option<CellGrid>,
        message: &ServerMessage,
    ) -> Vec<GridScroll> {
        match message {
            ServerMessage::FullFrame(cols, rows, colors, patches, cursor) => {
                self.colors = colors.clone();
                let mut frame = CellGrid::new(*cols, *rows, Rgb::default());
                self.apply_patches(&mut frame, patches);
                set_frame_cursor(&mut frame, *cursor);
                *grid = Some(frame);
                Vec::new()
            }
            ServerMessage::Diff(colors, moves, patches, cursor) => {
                self.colors.extend_from_slice(colors);
                let moves: Vec<GridScroll> = moves.iter().map(WireScroll::to_grid).collect();
                if let Some(grid) = grid {
                    for scroll in &moves {
                        scroll.apply(grid, scroll_fill());
                    }
                    self.apply_patches(grid, patches);
                    set_frame_cursor(grid, *cursor);
                }
                moves
            }
            ServerMessage::Clipboard(_)
            | ServerMessage::Title(_)
            | ServerMessage::Pointer(_)
            | ServerMessage::Shutdown
            | ServerMessage::Error(_)
            | ServerMessage::WaitFinished { .. } => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::{Random, source_lines, text_row};
    use gpui_tui::Rgb;

    #[test]
    fn only_the_last_of_consecutive_moves_is_kept() {
        let mouse = |action: MouseAction, col: u16| TermEvent::Mouse {
            action,
            col,
            row: 0,
            modifiers: Modifiers::default(),
        };
        let drag = MouseAction::Drag(MouseButtonKind::Left);
        let mut events = vec![
            mouse(MouseAction::Moved, 1),
            mouse(MouseAction::Moved, 2),
            TermEvent::Key {
                code: KeyCode::Char('a'),
                modifiers: Modifiers::default(),
            },
            mouse(MouseAction::Moved, 3),
            mouse(drag, 4),
            mouse(drag, 5),
            mouse(drag, 6),
            mouse(MouseAction::Down(MouseButtonKind::Left), 7),
            mouse(MouseAction::Down(MouseButtonKind::Left), 8),
        ];
        drop_superseded_moves(&mut events, |event| Some(event));
        let columns: Vec<Option<u16>> = events
            .iter()
            .map(|event| match event {
                TermEvent::Mouse { col, .. } => Some(*col),
                _ => None,
            })
            .collect();
        assert_eq!(columns, [Some(2), None, Some(3), Some(6), Some(7), Some(8)]);
    }

    #[test]
    fn messages_round_trip() {
        let messages = vec![
            ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                cols: 80,
                rows: 24,
            },
            ClientMessage::Input(TermEvent::Key {
                code: KeyCode::Char('p'),
                modifiers: Modifiers {
                    control: true,
                    shift: true,
                    ..Default::default()
                },
            }),
            ClientMessage::Input(TermEvent::Paste("한글 paste".into())),
            ClientMessage::Resize {
                cols: 100,
                rows: 30,
            },
            ClientMessage::Detach,
            ClientMessage::OpenAndWait {
                paths: vec![PathBuf::from("/repo/.git/COMMIT_EDITMSG")],
                quit_session: true,
            },
        ];
        let mut buffer = Vec::new();
        for message in &messages {
            write_message(&mut buffer, message).unwrap();
        }
        let mut reader = buffer.as_slice();
        for message in &messages {
            let decoded: ClientMessage = read_message(&mut reader).unwrap();
            assert_eq!(&decoded, message);
        }
    }

    #[test]
    fn pointer_messages_round_trip() {
        let messages = [
            CursorStyle::Arrow,
            CursorStyle::IBeam,
            CursorStyle::PointingHand,
            CursorStyle::ResizeUpLeftDownRight,
            CursorStyle::ContextualMenu,
        ]
        .map(ServerMessage::Pointer);
        let mut buffer = Vec::new();
        for message in &messages {
            write_message(&mut buffer, message).unwrap();
        }
        let mut reader = buffer.as_slice();
        for message in &messages {
            let decoded: ServerMessage = read_message(&mut reader).unwrap();
            assert_eq!(&decoded, message);
        }
    }

    #[test]
    fn underline_colors_survive_the_wire_and_split_spans() {
        let (red, blue) = (Rgb::new(224, 108, 117), Rgb::new(97, 175, 239));
        let mut grid = CellGrid::new(6, 1, Rgb::new(40, 44, 52));
        for (col, cell) in grid.row_mut(0).iter_mut().enumerate() {
            cell.glyph = 'x'.into();
            cell.attrs = CellAttrs::UNDERLINE | CellAttrs::CURLY_UNDERLINE;
            cell.underline = UnderlineColor::of(if col < 3 { red } else { blue });
        }
        assert_eq!(encode_cells(grid.row(0)).len(), 2);
        let mut decoded = None;
        let message = FrameEncoder::default().update(None, &grid).unwrap();
        FrameDecoder::default().apply(&mut decoded, &message);
        assert_eq!(decoded.as_ref(), Some(&grid));

        let mut plain = grid.clone();
        for cell in plain.row_mut(0) {
            cell.attrs = CellAttrs::empty();
        }
        assert_eq!(encode_cells(plain.row(0)).len(), 1);
    }

    fn assert_looks_like(actual: &CellGrid, expected: &CellGrid) {
        assert_eq!((actual.cols, actual.rows), (expected.cols, expected.rows));
        assert_eq!(frame_cursor(actual), frame_cursor(expected));
        for (index, (actual, expected)) in actual.cells.iter().zip(&expected.cells).enumerate() {
            assert!(
                actual.looks_like(expected),
                "cell {index}: {actual:?} != {expected:?}"
            );
        }
    }

    fn sample_grid() -> CellGrid {
        let mut grid = CellGrid::new(12, 3, Rgb::new(10, 20, 30));
        for (col, ch) in "fn main".chars().enumerate() {
            if let Some(cell) = grid.cell_mut(col as i32, 0) {
                cell.glyph = ch.into();
                cell.fg = if col < 2 {
                    Rgb::new(200, 100, 0)
                } else {
                    Rgb::new(0, 200, 100)
                };
            }
        }
        if let Some(cell) = grid.cell_mut(0, 1) {
            cell.glyph = '한'.into();
            cell.attrs = CellAttrs::BOLD;
        }
        if let Some(cell) = grid.cell_mut(1, 1) {
            cell.attrs = CellAttrs::WIDE_CONTINUATION;
        }
        if let Some(cell) = grid.cell_mut(4, 2) {
            cell.attrs = CellAttrs::UNDERLINE;
        }
        if let Some(cell) = grid.cell_mut(9, 2) {
            cell.attrs = CellAttrs::DEFAULT_BACKGROUND;
        }
        grid
    }

    #[test]
    fn diffs_reproduce_the_full_frame() {
        let first = sample_grid();
        let mut second = first.clone();
        if let Some(cell) = second.cell_mut(3, 2) {
            cell.glyph = 'z'.into();
            cell.fg = Rgb::new(1, 2, 3);
        }
        second.cursor = Some(CursorPosition { col: 3, row: 2 });

        let mut encoder = FrameEncoder::default();
        let mut decoder = FrameDecoder::default();
        let mut client = None;
        let initial = encoder.update(None, &first).unwrap();
        assert!(matches!(initial, ServerMessage::FullFrame(..)));
        decoder.apply(&mut client, &initial);
        assert_looks_like(client.as_ref().unwrap(), &first);

        let diff = encoder.update(Some(&first), &second).unwrap();
        match &diff {
            ServerMessage::Diff(_, _, patches, _) => {
                assert_eq!(patches.len(), 1);
                assert_eq!(patches[0].0, 2);
            }
            other => panic!("expected a diff, got {other:?}"),
        }
        let mut buffer = Vec::new();
        write_message(&mut buffer, &diff).unwrap();
        assert!(buffer.len() < 48, "diff took {} bytes", buffer.len());
        let decoded: ServerMessage = read_message(&mut buffer.as_slice()).unwrap();
        decoder.apply(&mut client, &decoded);
        assert_looks_like(client.as_ref().unwrap(), &second);

        assert_eq!(encoder.update(Some(&second), &second), None);
    }

    #[test]
    fn cursor_shapes_reach_the_client_in_full_frames_and_diffs() {
        let mut first = sample_grid();
        first.cursor = Some(CursorPosition { col: 2, row: 0 });
        first.cursor_shape = CursorShape::Underline;
        let mut second = first.clone();
        second.cursor_shape = CursorShape::Block;

        let mut encoder = FrameEncoder::default();
        let mut decoder = FrameDecoder::default();
        let mut client = None;
        for (previous, grid) in [(None, &first), (Some(&first), &second)] {
            let message = encoder
                .update(previous, grid)
                .expect("a changed cursor shape needs a frame");
            let mut buffer = Vec::new();
            write_message(&mut buffer, &message).unwrap();
            let decoded: ServerMessage = read_message(&mut buffer.as_slice()).unwrap();
            decoder.apply(&mut client, &decoded);
            let shown = client.as_ref().unwrap();
            assert_eq!(
                (shown.cursor, shown.cursor_shape),
                (grid.cursor, grid.cursor_shape)
            );
        }
    }

    #[test]
    fn clusters_survive_full_frames_and_diffs() {
        let mut first = sample_grid();
        let clusters = [
            (2, 1, "e\u{301}"),
            (3, 1, "👩\u{200d}💻"),
            (11, 2, "a\u{301}\u{302}"),
        ];
        for (col, row, cluster) in clusters {
            if let Some(cell) = first.cell_mut(col, row) {
                cell.glyph = Glyph::from_cluster(cluster);
            }
        }
        if let Some(cell) = first.cell_mut(4, 1) {
            cell.attrs = CellAttrs::WIDE_CONTINUATION;
        }
        let mut second = first.clone();
        if let Some(cell) = second.cell_mut(5, 1) {
            cell.glyph = Glyph::from_cluster("o\u{308}");
        }

        let mut encoder = FrameEncoder::default();
        let mut decoder = FrameDecoder::default();
        let mut client = None;
        for (previous, grid) in [(None, &first), (Some(&first), &second)] {
            let message = encoder.update(previous, grid).unwrap();
            let mut buffer = Vec::new();
            write_message(&mut buffer, &message).unwrap();
            let decoded: ServerMessage = read_message(&mut buffer.as_slice()).unwrap();
            decoder.apply(&mut client, &decoded);
            assert_looks_like(client.as_ref().unwrap(), grid);
        }
        assert!(
            client
                .unwrap()
                .text()
                .contains("e\u{301}👩\u{200d}💻o\u{308}")
        );
    }

    #[test]
    fn spans_merge_blanks_into_neighboring_text() {
        let grid = sample_grid();
        let spans = encode_cells(grid.row(0));
        let texts: Vec<&str> = spans.iter().map(|span| span.3.as_str()).collect();
        assert_eq!(texts, vec!["fn ", "main     "]);
        let spans = encode_cells(grid.row(1));
        let texts: Vec<&str> = spans.iter().map(|span| span.3.as_str()).collect();
        assert_eq!(texts, vec!["한\0", "          "]);
    }

    fn editor_grid(lines: &[String], first_line: usize) -> CellGrid {
        let mut grid = CellGrid::new(60, 14, Rgb::new(40, 44, 52));
        for row in 1..13 {
            let text = lines
                .get(first_line + row)
                .map(String::as_str)
                .unwrap_or("");
            for (col, ch) in text.chars().take(44).enumerate() {
                if let Some(cell) = grid.cell_mut(col as i32, row as i32) {
                    cell.glyph = ch.into();
                    cell.fg = Rgb::new(100 + (col % 5) as u8 * 30, 120, 200);
                }
            }
            text_row(&mut grid, row as u16, 48, &format!("file_{row}.rs"));
        }
        grid
    }

    #[test]
    fn scrolled_frames_send_a_scroll_instead_of_every_row() {
        let lines = source_lines(200, 11);
        let first = editor_grid(&lines, 0);
        let second = editor_grid(&lines, 3);
        let full_size = {
            let mut buffer = Vec::new();
            write_message(&mut buffer, &FrameEncoder::default().full_frame(&second)).unwrap();
            buffer.len()
        };

        let mut encoder = FrameEncoder::default();
        let mut decoder = FrameDecoder::default();
        let mut client = None;
        decoder.apply(&mut client, &encoder.update(None, &first).unwrap());
        let update = encoder.update(Some(&first), &second).unwrap();
        let ServerMessage::Diff(_, moves, _, _) = &update else {
            panic!("expected a diff, got {update:?}");
        };
        assert_eq!(moves.len(), 1, "{moves:?}");
        let scroll = moves[0].to_grid();
        assert_eq!((scroll.top, scroll.bottom, scroll.shift), (1, 13, 3));

        let mut buffer = Vec::new();
        write_message(&mut buffer, &update).unwrap();
        assert!(
            buffer.len() * 3 < full_size,
            "{} bytes vs {full_size}",
            buffer.len()
        );

        decoder.apply(&mut client, &update);
        assert_looks_like(client.as_ref().unwrap(), &second);
    }

    #[test]
    fn random_scrolls_and_edits_reproduce_every_frame() {
        let lines = source_lines(200, 11);
        let mut random = Random::new(7);
        let mut next_random = |bound: usize| random.next(bound);
        let mut first_line = 0;
        let mut server = editor_grid(&lines, first_line);
        let mut encoder = FrameEncoder::default();
        let mut decoder = FrameDecoder::default();
        let mut client = None;
        let mut sent_scrolls = 0;
        decoder.apply(&mut client, &encoder.update(None, &server).unwrap());
        for _ in 0..300 {
            first_line = (first_line + next_random(9))
                .saturating_sub(4)
                .min(lines.len() - 14);
            let mut next = editor_grid(&lines, first_line);
            for _ in 0..next_random(4) {
                if let Some(cell) = next.cell_mut(next_random(60) as i32, next_random(14) as i32) {
                    cell.glyph = '#'.into();
                    cell.attrs = CellAttrs::UNDERLINE;
                }
            }
            next.cursor = Some(CursorPosition {
                col: next_random(60) as u16,
                row: next_random(14) as u16,
            });
            next.cursor_shape =
                [CursorShape::Bar, CursorShape::Block, CursorShape::Underline][next_random(3)];
            if let Some(update) = encoder.update(Some(&server), &next) {
                if matches!(&update, ServerMessage::Diff(_, moves, _, _) if !moves.is_empty()) {
                    sent_scrolls += 1;
                }
                let mut buffer = Vec::new();
                write_message(&mut buffer, &update).unwrap();
                let update: ServerMessage = read_message(&mut buffer.as_slice()).unwrap();
                decoder.apply(&mut client, &update);
            }
            let mirrored = client.as_ref().unwrap();
            assert_looks_like(mirrored, &next);
            assert_eq!(mirrored.cursor, next.cursor);
            assert_eq!(mirrored.cursor_shape, next.cursor_shape);
            server = next;
        }
        assert!(sent_scrolls > 50, "only {sent_scrolls} scrolls were sent");
    }

    #[test]
    fn a_full_color_table_starts_over_with_a_full_frame() {
        let mut grid = CellGrid::new(80, 60, Rgb::default());
        for (index, cell) in grid.cells.iter_mut().enumerate() {
            cell.glyph = 'x'.into();
            cell.fg = Rgb::new(index as u8, (index >> 8) as u8, 9);
        }
        let mut encoder = FrameEncoder::default();
        let mut decoder = FrameDecoder::default();
        let mut client = None;
        decoder.apply(&mut client, &encoder.update(None, &grid).unwrap());
        assert_looks_like(client.as_ref().unwrap(), &grid);

        let mut next = grid.clone();
        if let Some(cell) = next.cell_mut(0, 0) {
            cell.glyph = 'y'.into();
        }
        let update = encoder.update(Some(&grid), &next).unwrap();
        assert!(matches!(update, ServerMessage::FullFrame(..)));
        decoder.apply(&mut client, &update);
        assert_looks_like(client.as_ref().unwrap(), &next);
    }
}
