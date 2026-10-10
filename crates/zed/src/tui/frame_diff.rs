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
const MIN_SCROLL_COLUMNS: usize = 2;
const MAX_MOVES: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GridScroll {
    pub top: usize,
    pub bottom: usize,
    pub shift: isize,
    pub columns: Option<Range<usize>>,
}

impl GridScroll {
    pub fn fits(&self, rows: usize, cols: usize) -> bool {
        let columns = self.columns.clone().unwrap_or(0..cols);
        self.shift != 0
            && self.top + self.shift.unsigned_abs() < self.bottom
            && self.bottom <= rows
            && columns.len() >= MIN_SCROLL_COLUMNS
            && columns.end <= cols
    }

    pub fn apply(&self, grid: &mut CellGrid, fill: Cell) {
        let cols = grid.cols as usize;
        if !self.fits(grid.rows as usize, cols) {
            return;
        }
        self.copy(grid);
        let columns = self.columns.clone().unwrap_or(0..cols);
        for row in self.exposed_rows() {
            if let Some(cells) = grid.row_mut(row as u16).get_mut(columns.clone()) {
                cells.fill(fill);
            }
        }
    }

    pub fn copy(&self, grid: &mut CellGrid) {
        let cols = grid.cols as usize;
        if !self.fits(grid.rows as usize, cols) {
            return;
        }
        let columns = self.columns.clone().unwrap_or(0..cols);
        let exposed = self.exposed_rows();
        let mut copy_row = |row: usize| {
            let source = (row as isize + self.shift) as usize;
            grid.cells.copy_within(
                source * cols + columns.start..source * cols + columns.end,
                row * cols + columns.start,
            );
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
            columns: None,
        }
    } else {
        GridScroll {
            top: first - distance,
            bottom: last + 1,
            shift,
            columns: None,
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
    allow_column_window: bool,
) -> Option<GridScroll> {
    find_scroll_in(shown, target, rows, cols, 0..cols, allow_column_window)
}

fn find_scroll_in(
    shown: &CellGrid,
    target: &CellGrid,
    rows: usize,
    cols: usize,
    window: Range<usize>,
    allow_column_window: bool,
) -> Option<GridScroll> {
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

    let (gain, mut scroll) = candidates
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
    if gain <= MIN_SCROLL_GAIN as isize {
        return None;
    }
    let whole_width = 0..cols;
    if allow_column_window {
        let columns = margin_columns(shown, target, &scroll, window.clone())
            .into_iter()
            .chain([window])
            .find(|columns| !splits_wide_character(shown, &scroll, columns))?;
        scroll.columns = (columns != whole_width).then_some(columns);
    } else if window != whole_width {
        return None;
    }
    Some(scroll)
}

pub fn find_moves(
    shown: &CellGrid,
    target: &CellGrid,
    rows: usize,
    cols: usize,
    allow_column_window: bool,
    moved: &mut Option<CellGrid>,
    apply: impl Fn(&GridScroll, &mut CellGrid),
) -> Vec<GridScroll> {
    let Some(first) = find_scroll(shown, target, rows, cols, allow_column_window) else {
        return Vec::new();
    };
    let moved = moved.get_or_insert_with(|| CellGrid::new(0, 0, Rgb::default()));
    moved.clone_from(shown);
    let mut moves = vec![first];
    while let Some(last) = moves.last() {
        apply(last, moved);
        if moves.len() == MAX_MOVES {
            break;
        }
        let next = unmoved_columns(&moves, cols)
            .into_iter()
            .filter(|_| allow_column_window)
            .chain([0..cols])
            .find_map(|window| {
                find_scroll_in(moved, target, rows, cols, window, allow_column_window)
            });
        match next {
            Some(next) => moves.push(next),
            None => break,
        }
    }
    moves
}

fn unmoved_columns(moves: &[GridScroll], cols: usize) -> Vec<Range<usize>> {
    let mut moved: Vec<Range<usize>> = moves
        .iter()
        .map(|scroll| scroll.columns.clone().unwrap_or(0..cols))
        .collect();
    moved.sort_by_key(|columns| columns.start);
    let mut free = Vec::new();
    let mut start = 0;
    for columns in moved {
        if columns.start >= start + MIN_SCROLL_COLUMNS {
            free.push(start..columns.start);
        }
        start = start.max(columns.end);
    }
    if cols >= start + MIN_SCROLL_COLUMNS {
        free.push(start..cols);
    }
    free
}

fn splits_wide_character(shown: &CellGrid, scroll: &GridScroll, columns: &Range<usize>) -> bool {
    (scroll.top..scroll.bottom).any(|row| {
        [columns.start, columns.end].iter().any(|col| {
            shown
                .cell(*col as i32, row as i32)
                .is_some_and(|cell| cell.is_wide_continuation())
        })
    })
}

fn margin_columns(
    shown: &CellGrid,
    target: &CellGrid,
    scroll: &GridScroll,
    window: Range<usize>,
) -> Option<Range<usize>> {
    let mut gains = vec![0isize; window.len()];
    let exposed = scroll.exposed_rows();
    for row in scroll.top..scroll.bottom {
        let shown_row = window_row(shown, row, window.clone());
        let target_row = window_row(target, row, window.clone());
        let source_row = (!exposed.contains(&row)).then(|| {
            window_row(
                shown,
                (row as isize + scroll.shift) as usize,
                window.clone(),
            )
        });
        for (col, gain) in gains.iter_mut().enumerate() {
            let Some(wanted) = target_row.get(col) else {
                continue;
            };
            let in_place = shown_row
                .get(col)
                .is_some_and(|cell| cell.looks_like(wanted));
            let moved = source_row
                .and_then(|source| source.get(col))
                .is_some_and(|cell| cell.looks_like(wanted));
            *gain += moved as isize - in_place as isize;
        }
    }

    let (columns, _) = best_run(gains.into_iter().enumerate())?;
    (columns.len() >= MIN_SCROLL_COLUMNS)
        .then(|| window.start + columns.start..window.start + columns.end)
}

const MAX_ROW_SHIFT: usize = 8;
const ROW_SHIFT_OVERHEAD: isize = 16;
const STYLE_CHANGE_BYTES: isize = 12;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowShift {
    pub start: usize,
    pub shift: isize,
}

impl RowShift {
    pub fn vacated(&self, cols: usize) -> Range<usize> {
        let distance = self.shift.unsigned_abs();
        if self.shift > 0 {
            self.start..self.start + distance
        } else {
            cols - distance..cols
        }
    }

    pub fn apply(&self, row: &mut [Cell], fill: Cell) {
        let cols = row.len();
        let distance = self.shift.unsigned_abs();
        if self.shift > 0 {
            row.copy_within(self.start..cols - distance, self.start + distance);
        } else {
            row.copy_within(self.start + distance..cols, self.start);
        }
        if let Some(cells) = row.get_mut(self.vacated(cols)) {
            cells.fill(fill);
        }
    }
}

fn is_continuation(cells: &[Cell], col: usize) -> bool {
    cells
        .get(col)
        .is_some_and(|cell| cell.is_wide_continuation())
}

fn redraw_weights(wanted: &[Cell]) -> Vec<isize> {
    let style = |cell: &Cell| {
        let cell = cell.appearance();
        (cell.fg, cell.bg, cell.attrs)
    };
    let mut previous = None;
    wanted
        .iter()
        .map(|cell| {
            let current = style(cell);
            let changed = previous.replace(current) != Some(current);
            1 + if changed { STYLE_CHANGE_BYTES } else { 0 }
        })
        .collect()
}

pub fn find_row_shift(shown: &[Cell], wanted: &[Cell]) -> Option<RowShift> {
    let cols = shown.len().min(wanted.len());
    let (shown, wanted) = (&shown[..cols], &wanted[..cols]);
    let start = (0..cols).find(|&col| !shown[col].looks_like(&wanted[col]))?;
    if is_continuation(shown, start) {
        return None;
    }
    let weights = redraw_weights(wanted);
    let matches_now: Vec<bool> = (0..cols)
        .map(|col| shown[col].looks_like(&wanted[col]))
        .collect();
    let largest_gain: isize = (start..cols)
        .filter(|&col| !matches_now[col])
        .map(|col| weights[col])
        .sum();
    if largest_gain <= ROW_SHIFT_OVERHEAD {
        return None;
    }
    let gain = |shifted: &dyn Fn(usize) -> Option<Cell>| {
        (start..cols)
            .map(|col| {
                let matches_after = shifted(col).is_some_and(|cell| cell.looks_like(&wanted[col]));
                weights[col] * (matches_after as isize - matches_now[col] as isize)
            })
            .sum::<isize>()
    };
    let mut best: Option<(RowShift, isize)> = None;
    for distance in 1..=MAX_ROW_SHIFT.min(cols.saturating_sub(start + 1)) {
        let inserted = if is_continuation(shown, cols - distance) {
            isize::MIN
        } else {
            gain(&|col| {
                col.checked_sub(distance)
                    .filter(|source| *source >= start)
                    .map(|source| shown[source])
            })
        };
        let deleted = if is_continuation(shown, start + distance) {
            isize::MIN
        } else {
            gain(&|col| shown.get(col + distance).copied())
        };
        for (shift, total) in [
            (distance as isize, inserted),
            (-(distance as isize), deleted),
        ] {
            if total > ROW_SHIFT_OVERHEAD && best.as_ref().is_none_or(|(_, best)| total > *best) {
                best = Some((RowShift { start, shift }, total));
            }
        }
    }
    best.map(|(shift, _)| shift)
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

    fn colored_row(text: &str) -> Vec<Cell> {
        let mut cells = row(text);
        for (index, cell) in cells.iter_mut().enumerate() {
            cell.fg = Rgb::new(index as u8 / 4, 100, 200);
        }
        cells
    }

    #[test]
    fn typing_and_deleting_mid_line_are_found_as_row_shifts() {
        let before = colored_row("let value = compute(1, 2, 3);       ");
        let mut typed = before.clone();
        typed.insert(
            4,
            Cell {
                glyph: 'X'.into(),
                ..before[4]
            },
        );
        typed.pop();
        assert_eq!(
            find_row_shift(&before, &typed),
            Some(RowShift { start: 4, shift: 1 })
        );
        assert_eq!(
            find_row_shift(&typed, &before),
            Some(RowShift {
                start: 4,
                shift: -1
            })
        );
        assert_eq!(find_row_shift(&before, &before), None);
    }

    #[test]
    fn short_tails_are_redrawn_instead_of_shifted() {
        let before = colored_row("abc");
        let mut typed = before.clone();
        typed.insert(
            1,
            Cell {
                glyph: 'X'.into(),
                ..before[1]
            },
        );
        typed.pop();
        assert_eq!(find_row_shift(&before, &typed), None);
    }

    #[test]
    fn row_shifts_never_split_wide_characters() {
        let mut before = colored_row("ab한 cdefghijklmnopqrstuvwxyz0123456789");
        before[3].attrs = CellAttrs::WIDE_CONTINUATION;
        let mut deleted = before.clone();
        deleted.remove(2);
        deleted.push(before[0]);
        assert_eq!(find_row_shift(&before, &deleted), None);
    }

    fn text(cells: &[Cell]) -> String {
        cells
            .iter()
            .filter_map(|cell| cell.glyph.as_char())
            .collect()
    }

    #[test]
    fn applying_a_row_shift_matches_the_terminal() {
        let mut cells = row("abcdef");
        let fill = Cell {
            glyph: '?'.into(),
            ..Cell::blank(Rgb::new(0, 0, 0))
        };
        let insert = RowShift { start: 1, shift: 2 };
        assert_eq!(insert.vacated(cells.len()), 1..3);
        insert.apply(&mut cells, fill);
        assert_eq!(text(&cells), "a??bcd");
        let delete = RowShift {
            start: 0,
            shift: -2,
        };
        assert_eq!(delete.vacated(cells.len()), 4..6);
        delete.apply(&mut cells, fill);
        assert_eq!(text(&cells), "?bcd??");
    }

    #[test]
    fn inserting_never_pushes_half_a_wide_character_off_the_row() {
        let mut before = colored_row("let value = compute(1, 2, 3);       한 ");
        let last = before.len() - 1;
        before[last].attrs = CellAttrs::WIDE_CONTINUATION;
        let mut typed = before.clone();
        typed.insert(
            4,
            Cell {
                glyph: 'X'.into(),
                ..before[4]
            },
        );
        typed.pop();
        assert_eq!(find_row_shift(&before, &typed), None);
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
    fn margin_windows_are_at_least_two_columns_wide() {
        let shown = grid_of(&["a---", "b---", "c---"]);
        let target = grid_of(&["b---", "c---", "x---"]);
        let scroll = GridScroll {
            top: 0,
            bottom: 3,
            shift: 1,
            columns: None,
        };
        assert_eq!(margin_columns(&shown, &target, &scroll, 0..4), None);
    }

    #[test]
    fn scrolls_the_terminal_would_ignore_do_not_fit() {
        let scroll = |top, bottom, shift, columns| GridScroll {
            top,
            bottom,
            shift,
            columns,
        };
        assert!(scroll(1, 13, 3, None).fits(14, 60));
        assert!(scroll(1, 13, -3, Some(0..2)).fits(14, 60));
        assert!(!scroll(2, 3, 1, None).fits(14, 60));
        assert!(!scroll(1, 4, 3, None).fits(14, 60));
        assert!(!scroll(1, 13, 0, None).fits(14, 60));
        assert!(!scroll(1, 15, 3, None).fits(14, 60));
        assert!(!scroll(1, 13, 3, Some(5..6)).fits(14, 60));
        assert!(!scroll(1, 13, 3, Some(50..61)).fits(14, 60));
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
        let moves = find_moves(shown, target, 20, 60, true, &mut moved, |scroll, grid| {
            scroll.copy(grid)
        });
        (moves, moved.unwrap_or_else(|| shown.clone()))
    }

    #[test]
    fn split_panes_scrolling_apart_move_separately() {
        for right_shift in [-2, 10] {
            let target = split_panes(3, right_shift);
            let (moves, moved) = moves_between(&split_panes(0, 0), &target);
            let mut shifts: Vec<isize> = moves.iter().map(|scroll| scroll.shift).collect();
            shifts.sort();
            let mut expected = [3, right_shift];
            expected.sort();
            assert_eq!(shifts, expected, "{moves:?}");
            assert!(
                moves.iter().all(|scroll| scroll.columns.is_some()),
                "{moves:?}"
            );
            let still_wrong = matching_cells(&target.cells, &target.cells)
                - matching_cells(&moved.cells, &target.cells);
            let exposed = (3 + right_shift.unsigned_abs()) * 30;
            assert!(still_wrong <= exposed, "{still_wrong} cells left to draw");
        }
    }

    #[test]
    fn a_single_scroll_is_one_move() {
        let (moves, _) = moves_between(&split_panes(0, 0), &split_panes(3, 3));
        assert_eq!(moves.len(), 1, "{moves:?}");
        assert_eq!(moves[0].shift, 3);
        assert!(moves[0].columns.is_none());
    }
}
