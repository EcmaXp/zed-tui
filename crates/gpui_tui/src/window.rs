use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::Arc,
};

use gpui::{
    AnyWindowHandle, Bounds, Capslock, DispatchEventResult, GpuSpecs, Hsla, Modifiers,
    ModifiersChangedEvent, Pixels, PlatformAtlas, PlatformDisplay, PlatformInput,
    PlatformInputHandler, PlatformWindow, Point, PromptButton, PromptLevel, RequestFrameOptions,
    Scene, Size, WindowAppearance, WindowBackgroundAppearance, WindowBounds, WindowControlArea,
    WindowParams, WindowVisibility, px,
};

use crate::{
    CELL_HEIGHT, CELL_WIDTH,
    atlas::TuiAtlas,
    caret_cell, cells_for_size,
    grid::{CellGrid, CursorPosition, Rgb},
    platform::{FrameOutput, PlatformOutputs, WindowRegistry},
    rasterize::{CaretCandidate, CaretMode, canvas_if_untouched, rasterize_scene, resolve_carets},
    size_for_cells, with_taken,
};

const FLOATING_BAR_COLS: u16 = 1;
const FLOATING_BAR_CONTRAST: f32 = 0.25;
const DARK_LUMA: u32 = 128_000;

#[derive(Default)]
struct Callbacks {
    request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    input: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    resize: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    active_status_change: Option<Box<dyn FnMut(bool)>>,
    hover_status_change: Option<Box<dyn FnMut(bool)>>,
}

pub(crate) struct WindowState {
    bounds: Bounds<Pixels>,
    display: Rc<dyn PlatformDisplay>,
    input_handler: Option<PlatformInputHandler>,
    title: String,
    mouse_position: Point<Pixels>,
    modifiers: Modifiers,
    is_fullscreen: bool,
    atlas: TuiAtlas,
    outputs: Rc<PlatformOutputs>,
    last_caret_color: Option<Rgb>,
    pending_frame: Option<(CellGrid, Vec<CaretCandidate>)>,
    floating: Option<Floating>,
}

struct Floating {
    requested: Bounds<Pixels>,
    underlay: CellGrid,
}

fn floating_bounds(requested: Bounds<Pixels>, display: Size<Pixels>) -> Bounds<Pixels> {
    let (display_cols, display_rows) = cells_for_size(display);
    let (cols, rows) = cells_for_size(requested.size);
    let cols = cols
        .min(display_cols.saturating_sub(FLOATING_BAR_COLS))
        .max(1);
    let rows = rows.min(display_rows).max(1);
    let place = |origin: Pixels, cell: f32, reserved: u16, extent: u16, display_extent: u16| {
        let first = i32::from(reserved);
        let last = i32::from(display_extent.saturating_sub(extent)).max(first);
        ((origin.as_f32() / cell).round() as i32).clamp(first, last) as f32 * cell
    };
    Bounds::new(
        Point::new(
            px(place(
                requested.origin.x,
                CELL_WIDTH,
                FLOATING_BAR_COLS,
                cols,
                display_cols,
            )),
            px(place(
                requested.origin.y,
                CELL_HEIGHT,
                0,
                rows,
                display_rows,
            )),
        ),
        size_for_cells(cols, rows),
    )
}

fn floating_bar(content: &CellGrid, canvas: Rgb) -> Rgb {
    let background = content.cells.first().map_or(Rgb::default(), |cell| cell.bg);
    floating_bar_color(canvas_if_untouched(background, canvas))
}

fn floating_bar_color(background: Rgb) -> Rgb {
    let luma = 299 * u32::from(background.r)
        + 587 * u32::from(background.g)
        + 114 * u32::from(background.b);
    let toward = if luma < DARK_LUMA {
        Hsla::white()
    } else {
        Hsla::black()
    };
    background.blend(toward.opacity(FLOATING_BAR_CONTRAST))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameOutcome {
    NotDrawn,
    Unchanged,
    Changed,
}

#[derive(Clone)]
pub(crate) struct TuiWindowHandle {
    handle: AnyWindowHandle,
    registry: Weak<WindowRegistry>,
    state: Rc<RefCell<WindowState>>,
    callbacks: Rc<RefCell<Callbacks>>,
    frame_requested: Rc<Cell<bool>>,
}

pub(crate) struct TuiWindow(TuiWindowHandle);

impl TuiWindow {
    pub(crate) fn new(
        handle: AnyWindowHandle,
        params: WindowParams,
        floating_request: Option<Bounds<Pixels>>,
        display: Rc<dyn PlatformDisplay>,
        outputs: Rc<PlatformOutputs>,
        registry: Weak<WindowRegistry>,
    ) -> Self {
        let floating = floating_request.map(|requested| {
            let (cols, rows) = cells_for_size(params.bounds.size);
            Floating {
                requested,
                underlay: outputs
                    .frame
                    .borrow()
                    .last_delivered
                    .clone()
                    .unwrap_or_else(|| CellGrid::new(cols, rows, Rgb::default())),
            }
        });
        let bounds = match &floating {
            Some(floating) => floating_bounds(floating.requested, params.bounds.size),
            None => params.bounds,
        };
        Self(TuiWindowHandle {
            handle,
            registry,
            state: Rc::new(RefCell::new(WindowState {
                bounds,
                display,
                input_handler: None,
                title: String::new(),
                mouse_position: Point::default(),
                modifiers: Modifiers::default(),
                is_fullscreen: false,
                atlas: TuiAtlas::default(),
                outputs,
                last_caret_color: None,
                pending_frame: None,
                floating,
            })),
            callbacks: Rc::default(),
            frame_requested: Rc::new(Cell::new(true)),
        })
    }

    pub(crate) fn handle(&self) -> TuiWindowHandle {
        self.0.clone()
    }
}

impl TuiWindowHandle {
    pub(crate) fn handle_input(&self, input: PlatformInput) {
        let input = self.to_window_coordinates(input);
        {
            let mut state = self.state.borrow_mut();
            match &input {
                PlatformInput::MouseMove(event) => {
                    state.mouse_position = event.position;
                    state.modifiers = event.modifiers;
                }
                PlatformInput::MouseDown(event) => {
                    state.mouse_position = event.position;
                    state.modifiers = event.modifiers;
                }
                PlatformInput::MouseUp(event) => {
                    state.mouse_position = event.position;
                    state.modifiers = event.modifiers;
                }
                PlatformInput::KeyDown(event) => state.modifiers = event.keystroke.modifiers,
                PlatformInput::ModifiersChanged(event) => state.modifiers = event.modifiers,
                _ => {}
            }
        }

        let (typed, releases_modifiers) = match &input {
            PlatformInput::KeyDown(event) => {
                let modifiers = event.keystroke.modifiers;
                let typed = modifiers
                    .is_subset_of(&Modifiers::shift())
                    .then(|| event.keystroke.key_char.clone())
                    .flatten();
                (typed, modifiers.modified())
            }
            _ => (None, false),
        };
        let result = with_taken(
            &self.callbacks,
            |callbacks| &mut callbacks.input,
            |callback| callback(input),
        );
        let handled = result.is_some_and(|result| !result.propagate);
        if !handled && let Some(typed) = typed {
            self.insert_text(&typed);
        }

        if releases_modifiers {
            self.handle_input(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                modifiers: Modifiers::default(),
                capslock: Capslock::default(),
            }));
        }
    }

    pub(crate) fn insert_text(&self, text: &str) {
        with_taken(
            &self.state,
            |state| &mut state.input_handler,
            |input_handler| input_handler.replace_text_in_range(None, text),
        );
    }

    fn to_window_coordinates(&self, mut input: PlatformInput) -> PlatformInput {
        let state = self.state.borrow();
        if state.floating.is_none() {
            return input;
        }
        let origin = state.bounds.origin;
        match &mut input {
            PlatformInput::MouseDown(event) => event.position -= origin,
            PlatformInput::MouseUp(event) => event.position -= origin,
            PlatformInput::MouseMove(event) => event.position -= origin,
            PlatformInput::MouseExited(event) => event.position -= origin,
            PlatformInput::ScrollWheel(event) => event.position -= origin,
            _ => {}
        }
        input
    }

    pub(crate) fn resize(&self, cols: u16, rows: u16) {
        let mut size = size_for_cells(cols, rows);
        {
            let mut state = self.state.borrow_mut();
            let state = &mut *state;
            if let Some(floating) = &mut state.floating {
                let mut moved = false;
                if (floating.underlay.cols, floating.underlay.rows) != (cols, rows) {
                    floating.underlay = CellGrid::new(cols, rows, Rgb::default());
                    moved = true;
                }
                let bounds = floating_bounds(floating.requested, size);
                size = bounds.size;
                if state.bounds.origin != bounds.origin {
                    state.bounds.origin = bounds.origin;
                    moved = true;
                }
                if moved {
                    self.schedule_frame();
                }
            }
            if state.bounds.size == size {
                return;
            }
            state.bounds.size = size;
        }
        with_taken(
            &self.callbacks,
            |callbacks| &mut callbacks.resize,
            |callback| callback(size, 1.0),
        );
        self.frame_requested.set(true);
    }

    pub(crate) fn handle(&self) -> AnyWindowHandle {
        self.handle
    }

    pub(crate) fn notify_active_status(&self, active: bool) {
        with_taken(
            &self.callbacks,
            |callbacks| &mut callbacks.active_status_change,
            |callback| callback(active),
        );
        with_taken(
            &self.callbacks,
            |callbacks| &mut callbacks.hover_status_change,
            |callback| callback(active),
        );
    }

    pub(crate) fn has_frame_request(&self) -> bool {
        self.frame_requested.get()
    }

    pub(crate) fn schedule_frame(&self) {
        self.frame_requested.set(true);
    }

    pub(crate) fn take_frame_request(&self) -> bool {
        self.frame_requested.replace(false)
    }

    pub(crate) fn request_frame(&self) -> FrameOutcome {
        self.draw_frame();
        self.deliver_pending_frame()
    }

    fn draw_frame(&self) {
        let require_presentation = self.is_floating();
        with_taken(
            &self.callbacks,
            |callbacks| &mut callbacks.request_frame,
            |callback| {
                callback(RequestFrameOptions {
                    require_presentation,
                    ..Default::default()
                })
            },
        );
    }

    pub(crate) fn is_floating(&self) -> bool {
        self.state.borrow().floating.is_some()
    }

    pub(crate) fn draw_underlay(&self) -> Option<CellGrid> {
        self.draw_frame();
        let (grid, carets) = self.state.borrow_mut().pending_frame.take()?;
        Some(self.resolve_caret(grid, carets, CaretMode::Underlay))
    }

    pub(crate) fn set_underlay(&self, underlay: CellGrid) {
        if let Some(floating) = &mut self.state.borrow_mut().floating {
            floating.underlay = underlay;
        }
        self.schedule_frame();
    }

    fn deliver_pending_frame(&self) -> FrameOutcome {
        let pending_frame = self.state.borrow_mut().pending_frame.take();
        let fresh = pending_frame
            .map(|(grid, carets)| self.resolve_caret(grid, carets, CaretMode::TerminalCursor));
        let mut state = self.state.borrow_mut();
        let state = &mut *state;
        let grid = match &mut state.floating {
            None => match fresh {
                Some(grid) => grid,
                None => return FrameOutcome::NotDrawn,
            },
            Some(floating) => {
                let Some(content) = fresh else {
                    return FrameOutcome::NotDrawn;
                };
                let origin = CursorPosition {
                    col: (state.bounds.origin.x.as_f32() / CELL_WIDTH) as u16,
                    row: (state.bounds.origin.y.as_f32() / CELL_HEIGHT) as u16,
                };
                let bar = floating_bar(&content, state.outputs.canvas.get());
                let mut screen = floating.underlay.clone();
                screen.overlay_with_left_bar(&content, origin, bar);
                screen
            }
        };
        let outputs = state.outputs.clone();
        deliver_frame(&outputs.frame, grid)
    }

    fn resolve_caret(
        &self,
        mut grid: CellGrid,
        carets: Vec<CaretCandidate>,
        mode: CaretMode,
    ) -> CellGrid {
        if !carets.is_empty() {
            let focused = self.focused_caret();
            let last_caret_color = self.state.borrow().last_caret_color;
            let caret_color = resolve_carets(&mut grid, &carets, focused, last_caret_color, mode);
            self.state.borrow_mut().last_caret_color = caret_color;
        }
        grid
    }

    fn focused_caret(&self) -> Option<CursorPosition> {
        let bounds = with_taken(
            &self.state,
            |state| &mut state.input_handler,
            |input_handler| input_handler.ime_candidate_bounds(),
        )??;
        let (col, row) = caret_cell(
            bounds.origin.x.as_f32(),
            (bounds.origin.y + bounds.size.height / 2.).as_f32(),
        );
        Some(CursorPosition {
            col: u16::try_from(col).ok()?,
            row: u16::try_from(row).ok()?,
        })
    }
}

impl raw_window_handle::HasWindowHandle for TuiWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        Err(raw_window_handle::HandleError::NotSupported)
    }
}

impl raw_window_handle::HasDisplayHandle for TuiWindow {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Err(raw_window_handle::HandleError::NotSupported)
    }
}

impl PlatformWindow for TuiWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.0.state.borrow().bounds
    }

    fn is_maximized(&self) -> bool {
        true
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Windowed(self.bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.bounds().size
    }

    fn resize(&mut self, size: Size<Pixels>) {
        self.0.state.borrow_mut().bounds.size = size;
    }

    fn scale_factor(&self) -> f32 {
        1.0
    }

    fn appearance(&self) -> WindowAppearance {
        WindowAppearance::Dark
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.0.state.borrow().display.clone())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.0.state.borrow().mouse_position
    }

    fn modifiers(&self) -> Modifiers {
        self.0.state.borrow().modifiers
    }

    fn capslock(&self) -> Capslock {
        Capslock::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.0.state.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.0.state.borrow_mut().input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<futures::channel::oneshot::Receiver<usize>> {
        None
    }

    fn activate(&self) {
        if let Some(registry) = self.0.registry.upgrade() {
            registry.activate(self.0.handle);
        }
    }

    fn is_active(&self) -> bool {
        self.0
            .registry
            .upgrade()
            .is_some_and(|registry| registry.active() == Some(self.0.handle))
    }

    fn visibility(&self) -> WindowVisibility {
        WindowVisibility::Visible
    }

    fn is_hovered(&self) -> bool {
        self.is_active()
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }

    fn set_title(&mut self, title: &str) {
        let outputs = {
            let mut state = self.0.state.borrow_mut();
            state.title = title.to_owned();
            state.outputs.clone()
        };
        with_taken(&outputs.title, |sink| sink, |sink| sink(title));
    }

    fn get_title(&self) -> String {
        self.0.state.borrow().title.clone()
    }

    fn set_background_appearance(&self, _background: WindowBackgroundAppearance) {}

    fn minimize(&self) {}

    fn zoom(&self) {}

    fn toggle_fullscreen(&self) {
        let mut state = self.0.state.borrow_mut();
        state.is_fullscreen = !state.is_fullscreen;
    }

    fn is_fullscreen(&self) -> bool {
        self.0.state.borrow().is_fullscreen
    }

    fn frame_waker(&self) -> Option<Rc<dyn Fn()>> {
        let frame_requested = self.0.frame_requested.clone();
        Some(Rc::new(move || frame_requested.set(true)))
    }

    fn schedule_frame(&self) {
        self.0.schedule_frame();
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.0.callbacks.borrow_mut().request_frame = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.0.callbacks.borrow_mut().input = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.callbacks.borrow_mut().active_status_change = Some(callback);
    }

    fn on_visibility_change(&self, _callback: Box<dyn FnMut(WindowVisibility)>) {}

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.callbacks.borrow_mut().hover_status_change = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.0.callbacks.borrow_mut().resize = Some(callback);
    }

    fn on_moved(&self, _callback: Box<dyn FnMut()>) {}

    fn on_should_close(&self, _callback: Box<dyn FnMut() -> bool>) {}

    fn on_close(&self, _callback: Box<dyn FnOnce()>) {}

    fn on_hit_test_window_control(&self, _callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
    }

    fn on_appearance_changed(&self, _callback: Box<dyn FnMut()>) {}

    fn draw(&self, scene: &Scene) {
        let mut state = self.0.state.borrow_mut();
        let state = &mut *state;
        let (cols, rows) = crate::cells_for_size(state.bounds.size);
        let outputs = state.outputs.clone();
        let icon_glyphs = outputs.icon_glyphs.borrow();
        let icon_glyph = |path: &str| icon_glyphs.as_ref().and_then(|glyphs| glyphs(path));
        let frame = rasterize_scene(
            scene,
            &state.atlas,
            &icon_glyph,
            cols,
            rows,
            outputs.canvas.get(),
        );
        state.pending_frame = Some(frame);
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        Arc::new(self.0.state.borrow().atlas.clone())
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        None
    }
}

impl Drop for TuiWindow {
    fn drop(&mut self) {
        if let Some(registry) = self.0.registry.upgrade() {
            registry.remove(self.0.handle);
        }
    }
}

fn deliver_frame(frame_sink: &RefCell<FrameOutput>, grid: CellGrid) -> FrameOutcome {
    {
        let mut output = frame_sink.borrow_mut();
        match &mut output.last_delivered {
            Some(last) if *last == grid => return FrameOutcome::Unchanged,
            Some(last) => last.clone_from(&grid),
            None => output.last_delivered = Some(grid.clone()),
        }
    }
    with_taken(frame_sink, |output| &mut output.sink, |sink| sink(grid));
    FrameOutcome::Changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(bounds: Bounds<Pixels>) -> (f32, f32, f32, f32) {
        (
            bounds.origin.x.as_f32() / CELL_WIDTH,
            bounds.origin.y.as_f32() / CELL_HEIGHT,
            bounds.size.width.as_f32() / CELL_WIDTH,
            bounds.size.height.as_f32() / CELL_HEIGHT,
        )
    }

    #[test]
    fn floating_windows_keep_their_requested_size_and_position() {
        let requested = Bounds::new(
            Point::new(px(20. * CELL_WIDTH), px(5. * CELL_HEIGHT)),
            size_for_cells(40, 10),
        );
        let bounds = floating_bounds(requested, size_for_cells(80, 24));
        assert_eq!(cells(bounds), (20., 5., 40., 10.));
    }

    #[test]
    fn floating_windows_larger_than_the_terminal_leave_room_only_for_the_left_bar() {
        let requested = Bounds::new(Point::default(), size_for_cells(200, 60));
        let bounds = floating_bounds(requested, size_for_cells(80, 24));
        assert_eq!(cells(bounds), (1., 0., 79., 24.));
    }

    #[test]
    fn floating_windows_reach_the_bottom_and_right_edges() {
        let requested = Bounds::new(
            Point::new(px(70. * CELL_WIDTH), px(20. * CELL_HEIGHT)),
            size_for_cells(10, 4),
        );
        let bounds = floating_bounds(requested, size_for_cells(80, 24));
        assert_eq!(cells(bounds), (70., 20., 10., 4.));
    }

    #[test]
    fn floating_bars_contrast_with_dark_and_light_backgrounds() {
        for background in [
            Rgb::new(40, 44, 52),
            Rgb::new(110, 110, 110),
            Rgb::new(150, 150, 150),
            Rgb::new(250, 250, 250),
        ] {
            let bar = floating_bar_color(background);
            assert!(bar.distance(background) >= 90, "{bar:?} on {background:?}");
        }
        let dark = Rgb::new(40, 44, 52);
        assert!(floating_bar_color(dark).r > dark.r);
        let light = Rgb::new(250, 250, 250);
        assert!(floating_bar_color(light).r < light.r);
    }

    #[test]
    fn floating_bars_over_an_unpainted_window_contrast_with_the_canvas() {
        let canvas = Rgb::new(40, 44, 52);
        let unpainted = CellGrid::new(4, 2, Rgb::default());
        assert_eq!(floating_bar(&unpainted, canvas), floating_bar_color(canvas));
        let painted = CellGrid::new(4, 2, Rgb::new(250, 250, 250));
        assert_eq!(
            floating_bar(&painted, canvas),
            floating_bar_color(Rgb::new(250, 250, 250))
        );
    }
}
