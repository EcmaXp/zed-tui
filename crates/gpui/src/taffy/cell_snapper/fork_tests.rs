use crate::{
    self as gpui, BoxShadow, Context, IntoElement, IsZero as _, ParentElement as _, Render,
    Styled as _, TestAppContext, TestDispatcher, Window, black, div, point, px, red,
};

struct FramedSurface;

impl Render for FramedSurface {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .border_1()
            .border_color(red())
            .shadow(vec![BoxShadow {
                color: black(),
                offset: point(px(0.), px(2.)),
                blur_radius: px(4.),
                spread_radius: px(0.),
                inset: false,
            }])
            .child(div().size(px(20.)))
    }
}

#[gpui::test]
fn framed_surfaces_paint_all_four_borders_outside_a_cell_grid(cx: &mut crate::TestAppContext) {
    let (_, cx) = cx.add_window_view(|_, _| FramedSurface);
    let border_widths = cx
        .update(|window, _| {
            window
                .painted_quads()
                .into_iter()
                .map(|quad| quad.border_widths)
                .find(|widths| widths.any(|width| !width.is_zero()))
        })
        .expect("the framed surface paints its border");
    assert!(
        !border_widths.any(|width| width.is_zero()),
        "{border_widths:?}"
    );
}

#[test]
fn truncation_keeps_text_that_fits_exactly_beside_the_affix() {
    let cx = TestAppContext::build(TestDispatcher::new(0), None);
    let text_system = cx.text_system().clone();
    let font_id = text_system.resolve_font(&crate::font(".ZedMono"));
    let mut wrapper = crate::LineWrapper::new(font_id, px(16.), text_system.clone());
    let text = "aa bbb cccc ddddd eeee ffff gggg";
    let runs = [crate::TextRun {
        len: text.len(),
        ..Default::default()
    }];
    for (kept, expected, truncate_from) in [
        ("aa bbb cccc", "aa bbb cccc…", crate::TruncateFrom::End),
        ("ffff gggg", "…ffff gggg", crate::TruncateFrom::Start),
    ] {
        let exact_width: crate::Pixels = kept
            .chars()
            .chain(['…'])
            .map(|c| text_system.layout_width(font_id, px(16.), c))
            .sum();
        let (result, _) =
            wrapper.truncate_line(text.into(), exact_width, "…", &runs, truncate_from);
        assert_eq!(result, expected);
    }
}
