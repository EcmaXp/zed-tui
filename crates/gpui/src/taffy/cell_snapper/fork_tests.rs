use crate::{
    self as gpui, App, BoxShadow, Context, DispatchPhase, FocusHandle, InteractiveElement as _,
    IntoElement, IsZero as _, ParentElement as _, Render, Styled as _, TestAppContext,
    TestDispatcher, Window, black, div, point, px, red,
};
use std::{cell::RefCell, rc::Rc};

crate::actions!(fork_test, [TestAction]);

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

struct SharedActionTestView {
    focus_handle: FocusHandle,
    listener: crate::SharedActionListener,
}

impl Render for SharedActionTestView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .on_shared_action(std::any::TypeId::of::<TestAction>(), self.listener.clone())
    }
}

#[gpui::test]
fn test_shared_action_listener_dispatches_in_every_frame(cx: &mut TestAppContext) {
    let phases = Rc::new(RefCell::new(Vec::new()));
    let listener: crate::SharedActionListener = Rc::new({
        let phases = phases.clone();
        move |_: &dyn std::any::Any, phase: DispatchPhase, _: &mut Window, _: &mut App| {
            phases.borrow_mut().push(phase)
        }
    });
    let (view, cx) = cx.add_window_view(|_, cx| SharedActionTestView {
        focus_handle: cx.focus_handle(),
        listener: listener.clone(),
    });
    let focus_handle = cx.update(|_, cx| view.read(cx).focus_handle.clone());
    cx.update(|window, cx| {
        window.focus(&focus_handle, cx);
        window.activate_window();
    });

    for _ in 0..2 {
        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        cx.dispatch_action(TestAction);
    }

    assert_eq!(
        *phases.borrow(),
        vec![
            DispatchPhase::Capture,
            DispatchPhase::Bubble,
            DispatchPhase::Capture,
            DispatchPhase::Bubble,
        ]
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
