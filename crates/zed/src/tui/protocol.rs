use std::io::{Read, Write};

use anyhow::{Context as _, Result, bail};
use gpui::Modifiers;
use gpui_tui::{Cell, CellAttrs, CellGrid, CursorPosition, Glyph, Rgb, UnderlineColor};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const PROTOCOL_VERSION: u32 = 15;
const MAX_MESSAGE_LEN: usize = 64 * 1024 * 1024;
const CONTINUATION: char = '\0';
const CLUSTER_EXTEND: char = '\u{1}';

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyCode {
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TermEvent {
    Key { code: KeyCode, modifiers: Modifiers },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    Hello { version: u32, cols: u16, rows: u16 },
    Input(TermEvent),
    Resize { cols: u16, rows: u16 },
    Detach,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span(u32, u32, u8, String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowPatch(u16, u16, Vec<Span>);

pub type FrameCursor = Option<CursorPosition>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
    FullFrame(
        u16,
        u16,
        Vec<RowPatch>,
        #[serde(with = "wire_cursor")] FrameCursor,
    ),
    Shutdown,
    Error(String),
}

mod wire_cursor {
    use gpui_tui::CursorPosition;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::FrameCursor;

    pub fn serialize<S: Serializer>(
        cursor: &FrameCursor,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        cursor
            .map(|cursor| (cursor.col, cursor.row))
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<FrameCursor, D::Error> {
        Ok(Option::<(u16, u16)>::deserialize(deserializer)?
            .map(|(col, row)| CursorPosition { col, row }))
    }
}

pub fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> Result<()> {
    let mut body = Vec::new();
    ciborium::into_writer(message, &mut body).context("encoding message")?;
    let Some(len) = u32::try_from(body.len())
        .ok()
        .filter(|_| body.len() <= MAX_MESSAGE_LEN)
    else {
        bail!("message of {} bytes exceeds the limit", body.len());
    };
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&body)?;
    Ok(())
}

pub fn read_message<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T> {
    let mut len = [0; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE_LEN {
        bail!("message of {len} bytes exceeds the limit");
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    ciborium::from_reader(body.as_slice()).context("decoding message")
}

fn encode_cells(cells: &[Cell]) -> Vec<Span> {
    cells
        .iter()
        .map(|cell| {
            let mut text = String::new();
            if cell.is_wide_continuation() {
                text.push(CONTINUATION);
            } else {
                push_glyph(&mut text, cell.glyph);
            }
            Span(
                u32::from(cell.fg),
                u32::from(cell.bg),
                (cell.attrs - CellAttrs::WIDE_CONTINUATION).bits(),
                text,
            )
        })
        .collect()
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
    spans.iter().flat_map(move |Span(fg, bg, attrs, text)| {
        let (fg, bg) = (color(*fg), color(*bg));
        let mut chars = text.chars().peekable();
        let mut cluster = String::new();
        std::iter::from_fn(move || {
            let ch = chars.next()?;
            let (glyph, attrs) = if ch == CONTINUATION {
                (Glyph::from_char(' '), CellAttrs::WIDE_CONTINUATION)
            } else if chars.peek() == Some(&CLUSTER_EXTEND) {
                cluster.clear();
                cluster.push(ch);
                while chars.next_if_eq(&CLUSTER_EXTEND).is_some() {
                    cluster.extend(chars.next());
                }
                (
                    Glyph::from_cluster(&cluster),
                    CellAttrs::from_bits_truncate(*attrs),
                )
            } else {
                (Glyph::from_char(ch), CellAttrs::from_bits_truncate(*attrs))
            };
            Some(Cell {
                glyph,
                fg,
                bg,
                attrs,
                underline: UnderlineColor::default(),
            })
        })
    })
}

#[derive(Default)]
pub struct FrameEncoder;

impl FrameEncoder {
    pub fn full_frame(&self, grid: &CellGrid) -> ServerMessage {
        let patches = (0..grid.rows)
            .map(|row| RowPatch(row, 0, encode_cells(grid.row(row))))
            .collect();
        ServerMessage::FullFrame(grid.cols, grid.rows, patches, grid.cursor)
    }
}

#[derive(Default)]
pub struct FrameDecoder;

impl FrameDecoder {
    fn apply_patches(&self, grid: &mut CellGrid, patches: &[RowPatch]) {
        let color = |value: u32| Rgb::from(value);
        for RowPatch(row, col, spans) in patches {
            for (offset, cell) in decode_spans(spans, &color).enumerate() {
                if let Some(target) = grid.cell_mut(*col as i32 + offset as i32, *row as i32) {
                    *target = cell;
                }
            }
        }
    }

    pub fn apply(&mut self, grid: &mut Option<CellGrid>, message: &ServerMessage) {
        match message {
            ServerMessage::FullFrame(cols, rows, patches, cursor) => {
                let mut frame = CellGrid::new(*cols, *rows, Rgb::default());
                self.apply_patches(&mut frame, patches);
                frame.cursor = *cursor;
                *grid = Some(frame);
            }
            ServerMessage::Shutdown | ServerMessage::Error(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::{Random, source_lines, text_row};
    use gpui_tui::Rgb;

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
            ClientMessage::Resize {
                cols: 100,
                rows: 30,
            },
            ClientMessage::Detach,
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

    fn assert_same_cells(actual: &CellGrid, expected: &CellGrid) {
        assert_eq!((actual.cols, actual.rows), (expected.cols, expected.rows));
        assert_eq!(actual.cursor, expected.cursor);
        for (index, (actual, expected)) in actual.cells.iter().zip(&expected.cells).enumerate() {
            assert_eq!(actual, expected, "cell {index}");
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
    fn clusters_survive_full_frames() {
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

        let encoder = FrameEncoder;
        let mut decoder = FrameDecoder;
        let mut client = None;
        for grid in [&first, &second] {
            let message = encoder.full_frame(grid);
            let mut buffer = Vec::new();
            write_message(&mut buffer, &message).unwrap();
            let decoded: ServerMessage = read_message(&mut buffer.as_slice()).unwrap();
            decoder.apply(&mut client, &decoded);
            assert_same_cells(client.as_ref().unwrap(), grid);
        }
        assert!(
            client
                .unwrap()
                .text()
                .contains("e\u{301}👩\u{200d}💻o\u{308}")
        );
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
    fn random_scrolls_and_edits_reproduce_every_frame() {
        let lines = source_lines(200, 11);
        let mut random = Random::new(7);
        let mut next_random = |bound: usize| random.next(bound);
        let mut first_line = 0;
        let encoder = FrameEncoder;
        let mut decoder = FrameDecoder;
        let mut client = None;
        decoder.apply(
            &mut client,
            &encoder.full_frame(&editor_grid(&lines, first_line)),
        );
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
            let mut buffer = Vec::new();
            write_message(&mut buffer, &encoder.full_frame(&next)).unwrap();
            let update: ServerMessage = read_message(&mut buffer.as_slice()).unwrap();
            decoder.apply(&mut client, &update);
            let mirrored = client.as_ref().unwrap();
            assert_same_cells(mirrored, &next);
            assert_eq!(mirrored.cursor, next.cursor);
        }
    }
}
