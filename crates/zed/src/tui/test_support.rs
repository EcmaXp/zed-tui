use gpui_tui::CellGrid;
use rand::{Rng as _, SeedableRng as _, rngs::StdRng};

pub struct Random(StdRng);

impl Random {
    pub fn new(seed: u64) -> Self {
        Self(StdRng::seed_from_u64(seed))
    }

    pub fn next(&mut self, bound: usize) -> usize {
        self.0.random_range(0..bound)
    }
}

pub fn source_lines(count: usize, seed: u64) -> Vec<String> {
    const WORDS: [&str; 16] = [
        "let",
        "value",
        "=",
        "compute(",
        "self.",
        "items",
        ".iter()",
        "map(|item|",
        "item.len())",
        "fn",
        "render",
        "->",
        "Result<()>",
        "{",
        "}",
        "match",
    ];
    let mut random = Random::new(seed);
    (0..count)
        .map(|_| {
            let mut line = String::new();
            while line.len() < 36 {
                line.push_str(WORDS[random.next(WORDS.len())]);
                line.push(' ');
            }
            line
        })
        .collect()
}

pub fn text_row(grid: &mut CellGrid, row: u16, col: u16, text: &str) {
    for (offset, ch) in text.chars().enumerate() {
        if let Some(cell) = grid.cell_mut(col as i32 + offset as i32, row as i32) {
            cell.glyph = ch.into();
        }
    }
}
