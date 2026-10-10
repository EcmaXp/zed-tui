use std::ops::Range;

use gpui_tui::Cell;

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
    }
}
