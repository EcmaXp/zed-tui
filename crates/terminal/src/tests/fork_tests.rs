use super::*;

const PALETTE_OUTPUT: &[u8] =
    b"\x1b]4;16;rgb:12/34/56\x07\x1b]4;1;rgb:ab/cd/ef\x07\x1b[38;5;16;41mX\x1b[m";

fn colors_of_x(terminal: &Entity<Terminal>, cx: &mut TestAppContext) -> (Color, Color) {
    terminal.update(cx, |terminal, _| {
        let cell = terminal
            .last_content
            .cells
            .iter()
            .find(|cell| cell.cell.character() == 'X')
            .expect("the X cell is on screen");
        (cell.cell.foreground(), cell.cell.background())
    })
}

#[gpui::test]
async fn test_osc_4_palette_overrides_reach_rendered_cells(cx: &mut TestAppContext) {
    let terminal = init_terminal_test(cx, PALETTE_OUTPUT);

    assert_eq!(
        colors_of_x(&terminal, cx),
        (
            Color::Spec(Rgb {
                r: 0x12,
                g: 0x34,
                b: 0x56
            }),
            Color::Spec(Rgb {
                r: 0xab,
                g: 0xcd,
                b: 0xef
            }),
        )
    );
}

#[gpui::test]
async fn test_osc_104_returns_cells_to_the_theme_palette(cx: &mut TestAppContext) {
    let terminal = init_terminal_test(cx, PALETTE_OUTPUT);
    terminal.update(cx, |terminal, cx| {
        terminal.write_output(b"\x1b]104;16;1\x07", cx);
    });
    cx.run_until_parked();
    terminal.update(cx, |terminal, _| {
        let term_lock = terminal.term.lock();
        terminal.last_content = make_content(&term_lock, &terminal.last_content);
    });

    assert_eq!(
        colors_of_x(&terminal, cx),
        (Color::Indexed(16), Color::Named(NamedColor::Red))
    );
}
