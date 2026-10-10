use std::{hash::Hasher as _, ops::Range};

use collections::FxHasher;

use gpui_tui::{Cell, CellGrid, Rgb};

pub fn changed_ranges(previous: &[Cell], next: &[Cell], merge_gap: usize) -> Vec<Range<usize>> {
    if previous.len() != next.len() {
        return vec![0..next.len()];
    }
    let is_continuation_in_either =
        |index: usize| is_continuation(previous, index) || is_continuation(next, index);
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for (col, (before, after)) in previous.iter().zip(next).enumerate() {
        if before.looks_like(after) || ranges.last().is_some_and(|last| col < last.end) {
            continue;
        }
        let mut start = col;
        while start > 0 && is_continuation_in_either(start) {
            start -= 1;
        }
        let mut end = col + 1;
        while end < next.len() && is_continuation_in_either(end) {
            end += 1;
        }
        match ranges.last_mut() {
            Some(last) if start <= last.end + merge_gap => last.end = last.end.max(end),
            _ => ranges.push(start..end),
        }
    }
    ranges
}

const MAX_SCROLL_SHIFT: usize = 24;
const MIN_SCROLL_GAIN: usize = 32;
const MIN_SCROLL_ROWS: usize = 3;
const SCROLL_BLOCK: usize = 8;
const SCROLL_CANDIDATES: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GridScroll {
    pub top: usize,
    pub bottom: usize,
    pub shift: isize,
}

impl GridScroll {
    pub fn fits(&self, rows: usize) -> bool {
        self.shift != 0 && self.top + self.shift.unsigned_abs() < self.bottom && self.bottom <= rows
    }

    pub fn apply(&self, grid: &mut CellGrid, fill: Cell) {
        let cols = grid.cols as usize;
        if !self.fits(grid.rows as usize) {
            return;
        }
        self.copy(grid);
        for row in self.exposed_rows() {
            if let Some(cells) = grid.cells.get_mut(row * cols..(row + 1) * cols) {
                cells.fill(fill);
            }
        }
    }

    pub fn copy(&self, grid: &mut CellGrid) {
        let cols = grid.cols as usize;
        if !self.fits(grid.rows as usize) {
            return;
        }
        let exposed = self.exposed_rows();
        let mut copy_row = |row: usize| {
            let source = (row as isize + self.shift) as usize;
            grid.cells
                .copy_within(source * cols..(source + 1) * cols, row * cols);
        };
        if self.shift > 0 {
            (self.top..exposed.start).for_each(&mut copy_row);
        } else {
            (exposed.end..self.bottom).rev().for_each(&mut copy_row);
        }
    }

    pub fn exposed_rows(&self) -> Range<usize> {
        let distance = self.shift.unsigned_abs();
        if self.shift > 0 {
            self.bottom - distance..self.bottom
        } else {
            self.top..self.top + distance
        }
    }
}

fn matching_cells(shown: &[Cell], target: &[Cell]) -> usize {
    shown
        .iter()
        .zip(target)
        .filter(|(shown, target)| shown.looks_like(target))
        .count()
}

fn window_row(grid: &CellGrid, row: usize, window: Range<usize>) -> &[Cell] {
    let cells = grid.row(row as u16);
    let end = window.end.min(cells.len());
    cells.get(window.start.min(end)..end).unwrap_or_default()
}

fn best_run(gains: impl Iterator<Item = (usize, isize)>) -> Option<(Range<usize>, isize)> {
    let mut best: Option<(Range<usize>, isize)> = None;
    let mut run: Option<(usize, isize)> = None;
    for (index, gain) in gains {
        run = match run {
            Some((start, total)) if total > 0 => Some((start, total + gain)),
            _ => Some((index, gain)),
        };
        if let Some((start, total)) = run
            && best
                .as_ref()
                .is_none_or(|(_, best_total)| total > *best_total)
        {
            best = Some((start..index + 1, total));
        }
    }
    best
}

fn best_band(
    rows: usize,
    shift: isize,
    gain: impl Fn(usize, usize) -> isize,
    kept_in_place: impl Fn(usize) -> isize,
) -> Option<(isize, GridScroll)> {
    let distance = shift.unsigned_abs();
    let movable = if shift > 0 {
        0..rows.saturating_sub(distance)
    } else {
        distance.min(rows)..rows
    };
    let (band, total) = best_run(movable.map(|row| {
        let source = (row as isize + shift) as usize;
        (row, gain(row, source))
    }))?;
    let (first, last) = (band.start, band.end - 1);
    let scroll = if shift > 0 {
        GridScroll {
            top: first,
            bottom: last + 1 + distance,
            shift,
        }
    } else {
        GridScroll {
            top: first - distance,
            bottom: last + 1,
            shift,
        }
    };
    let lost: isize = scroll.exposed_rows().map(&kept_in_place).sum();
    Some((total - lost, scroll))
}

struct BlockHashes {
    hashes: Vec<u64>,
    blocks_per_row: usize,
}

impl BlockHashes {
    fn new(grid: &CellGrid, rows: usize, window: Range<usize>) -> Self {
        let blocks_per_row = window.len().div_ceil(SCROLL_BLOCK);
        let mut hashes = Vec::with_capacity(rows * blocks_per_row);
        for row in 0..rows {
            let cells = window_row(grid, row, window.clone());
            hashes.extend(cells.chunks(SCROLL_BLOCK).map(|block| {
                let mut hasher = FxHasher::default();
                for cell in block {
                    let cell = cell.appearance();
                    hasher.write_u64(
                        (cell.glyph.to_u32() as u64) << 40
                            ^ (u32::from(cell.bg) as u64) << 16
                            ^ (u32::from(cell.fg) as u64).rotate_left(29)
                            ^ cell.attrs.bits() as u64,
                    );
                }
                hasher.finish()
            }));
            hashes.resize((row + 1) * blocks_per_row, 0);
        }
        Self {
            hashes,
            blocks_per_row,
        }
    }

    fn row(&self, row: usize) -> &[u64] {
        let start = row * self.blocks_per_row;
        self.hashes
            .get(start..start + self.blocks_per_row)
            .unwrap_or_default()
    }
}

fn equal_blocks(shown: &[u64], target: &[u64]) -> isize {
    shown
        .iter()
        .zip(target)
        .filter(|(shown, target)| shown == target)
        .count() as isize
}

pub fn find_scroll(
    shown: &CellGrid,
    target: &CellGrid,
    rows: usize,
    cols: usize,
) -> Option<GridScroll> {
    let window = 0..cols;
    let width = window.len();
    let row_cells = |grid, row| window_row(grid, row, window.clone());
    let in_place: Vec<usize> = (0..rows)
        .map(|row| matching_cells(row_cells(shown, row), row_cells(target, row)))
        .collect();
    let changed: usize = in_place.iter().map(|matched| width - matched).sum();
    let changed_rows = in_place.iter().filter(|matched| **matched < width).count();
    if changed < MIN_SCROLL_GAIN || changed_rows < MIN_SCROLL_ROWS {
        return None;
    }

    let shown_blocks = BlockHashes::new(shown, rows, window.clone());
    let target_blocks = BlockHashes::new(target, rows, window.clone());
    let blocks_in_place: Vec<isize> = (0..rows)
        .map(|row| equal_blocks(shown_blocks.row(row), target_blocks.row(row)))
        .collect();
    let max_shift = MAX_SCROLL_SHIFT.min(rows.saturating_sub(1)) as isize;
    let mut candidates: Vec<(isize, isize)> = (-max_shift..=max_shift)
        .filter(|shift| *shift != 0)
        .filter_map(|shift| {
            let (estimate, _) = best_band(
                rows,
                shift,
                |row, source| {
                    equal_blocks(shown_blocks.row(source), target_blocks.row(row))
                        - blocks_in_place[row]
                },
                |row| blocks_in_place[row],
            )?;
            (estimate > 0).then_some((estimate, shift))
        })
        .collect();
    candidates.sort_by_key(|(estimate, _)| std::cmp::Reverse(*estimate));

    let (gain, scroll) = candidates
        .iter()
        .take(SCROLL_CANDIDATES)
        .filter_map(|(_, shift)| {
            best_band(
                rows,
                *shift,
                |row, source| {
                    matching_cells(row_cells(shown, source), row_cells(target, row)) as isize
                        - in_place[row] as isize
                },
                |row| in_place[row] as isize,
            )
        })
        .max_by_key(|(gain, _)| *gain)?;
    (gain > MIN_SCROLL_GAIN as isize).then_some(scroll)
}

pub fn find_moves(
    shown: &CellGrid,
    target: &CellGrid,
    rows: usize,
    cols: usize,
    moved: &mut Option<CellGrid>,
    apply: impl Fn(&GridScroll, &mut CellGrid),
) -> Vec<GridScroll> {
    let Some(first) = find_scroll(shown, target, rows, cols) else {
        return Vec::new();
    };
    let moved = moved.get_or_insert_with(|| CellGrid::new(0, 0, Rgb::default()));
    moved.clone_from(shown);
    apply(&first, moved);
    vec![first]
}

fn is_continuation(cells: &[Cell], col: usize) -> bool {
    cells
        .get(col)
        .is_some_and(|cell| cell.is_wide_continuation())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_tui::{CellAttrs, Rgb};

    fn row(text: &str) -> Vec<Cell> {
        text.chars()
            .map(|ch| Cell {
                glyph: ch.into(),
                ..Cell::blank(Rgb::new(0, 0, 0))
            })
            .collect()
    }

    #[test]
    fn changed_ranges_merge_small_gaps() {
        let previous = row("abcdefghijkl");
        let next = row("aXcdeYghijkZ");
        assert_eq!(changed_ranges(&previous, &next, 3), vec![1..6, 11..12]);
        assert_eq!(
            changed_ranges(&previous, &next, 0),
            vec![1..2, 5..6, 11..12]
        );
        assert!(changed_ranges(&previous, &previous, 3).is_empty());
    }

    #[test]
    fn changed_ranges_keep_wide_characters_whole() {
        let previous = row("ab  ");
        let mut next = row("a한  ");
        next[2].attrs = CellAttrs::WIDE_CONTINUATION;
        assert_eq!(changed_ranges(&previous, &next, 0), vec![1..3]);

        let mut shifted = next.clone();
        shifted[2].attrs = CellAttrs::WIDE_CONTINUATION;
        shifted[2].bg = Rgb::new(9, 9, 9);
        assert_eq!(changed_ranges(&next, &shifted, 0), vec![1..3]);
    }

    #[test]
    fn invisible_foreground_changes_are_ignored() {
        let previous = row("a b");
        let mut next = previous.clone();
        next[1].fg = Rgb::new(1, 2, 3);
        assert!(changed_ranges(&previous, &next, 0).is_empty());
        next[1].attrs = CellAttrs::UNDERLINE;
        assert_eq!(changed_ranges(&previous, &next, 0), vec![1..2]);
    }

    #[test]
    fn invisible_attribute_changes_on_blanks_are_ignored() {
        let previous = row("a b");
        let mut next = previous.clone();
        next[1].attrs = CellAttrs::BOLD | CellAttrs::ITALIC | CellAttrs::DEFAULT_FOREGROUND;
        assert!(changed_ranges(&previous, &next, 0).is_empty());
        let hashes = |cells: &[Cell]| {
            let mut grid = CellGrid::new(3, 1, Rgb::new(0, 0, 0));
            grid.row_mut(0).copy_from_slice(cells);
            BlockHashes::new(&grid, 1, 0..3).hashes
        };
        assert_eq!(hashes(&previous), hashes(&next));
    }

    fn grid_of(rows: &[&str]) -> CellGrid {
        let mut grid = CellGrid::new(rows[0].len() as u16, rows.len() as u16, Rgb::new(0, 0, 0));
        for (index, text) in rows.iter().enumerate() {
            grid.row_mut(index as u16).copy_from_slice(&row(text));
        }
        grid
    }

    #[test]
    fn scrolls_the_terminal_would_ignore_do_not_fit() {
        let scroll = |top, bottom, shift| GridScroll { top, bottom, shift };
        assert!(scroll(1, 13, 3).fits(14));
        assert!(scroll(1, 13, -3).fits(14));
        assert!(!scroll(2, 3, 1).fits(14));
        assert!(!scroll(1, 4, 3).fits(14));
        assert!(!scroll(1, 13, 0).fits(14));
        assert!(!scroll(1, 15, 3).fits(14));
    }

    fn line_text(line: usize, width: usize) -> String {
        (0..width)
            .map(|col| (b'a' + ((line * 31 + col * 7 + line * col) % 26) as u8) as char)
            .collect()
    }

    fn split_panes(left_offset: usize, right_offset: isize) -> CellGrid {
        let rows: Vec<String> = (0..20)
            .map(|row| {
                format!(
                    "{}|{}",
                    line_text(row + left_offset, 29),
                    line_text((row as isize + right_offset + 100) as usize, 30),
                )
            })
            .collect();
        grid_of(&rows.iter().map(String::as_str).collect::<Vec<_>>())
    }

    fn moves_between(shown: &CellGrid, target: &CellGrid) -> (Vec<GridScroll>, CellGrid) {
        let mut moved = None;
        let moves = find_moves(shown, target, 20, 60, &mut moved, |scroll, grid| {
            scroll.copy(grid)
        });
        (moves, moved.unwrap_or_else(|| shown.clone()))
    }

    #[test]
    fn a_single_scroll_is_one_move() {
        let (moves, _) = moves_between(&split_panes(0, 0), &split_panes(3, 3));
        assert_eq!(moves.len(), 1, "{moves:?}");
        assert_eq!(moves[0].shift, 3);
    }
}
