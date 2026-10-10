use crate::{
    self as gpui, BoxShadow, Context, IntoElement, IsZero as _, ParentElement as _, Render,
    Styled as _, Window, black, div, point, px, red,
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
