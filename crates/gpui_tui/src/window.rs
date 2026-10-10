use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::Arc,
};

use gpui::{
    AnyWindowHandle, Bounds, Capslock, DispatchEventResult, GpuSpecs, Modifiers,
    ModifiersChangedEvent, Pixels, PlatformAtlas, PlatformDisplay, PlatformInput,
    PlatformInputHandler, PlatformWindow, Point, PromptButton, PromptLevel, RequestFrameOptions,
    Scene, Size, WindowAppearance, WindowBackgroundAppearance, WindowBounds, WindowControlArea,
    WindowParams, WindowVisibility,
};

use crate::{
    atlas::TuiAtlas,
    caret_cell,
    grid::{CellGrid, CursorPosition, Rgb},
    platform::{PlatformOutputs, WindowRegistry},
    rasterize::{CaretCandidate, CaretMode, rasterize_scene, resolve_carets},
    size_for_cells, with_taken,
};

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
}

#[derive(Clone)]
pub(crate) struct TuiWindowHandle {
    handle: AnyWindowHandle,
    registry: Weak<WindowRegistry>,
    state: Rc<RefCell<WindowState>>,
    callbacks: Rc<RefCell<Callbacks>>,
}

pub(crate) struct TuiWindow(TuiWindowHandle);

impl TuiWindow {
    pub(crate) fn new(
        handle: AnyWindowHandle,
        params: WindowParams,
        display: Rc<dyn PlatformDisplay>,
        outputs: Rc<PlatformOutputs>,
        registry: Weak<WindowRegistry>,
    ) -> Self {
        Self(TuiWindowHandle {
            handle,
            registry,
            state: Rc::new(RefCell::new(WindowState {
                bounds: params.bounds,
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
            })),
            callbacks: Rc::default(),
        })
    }

    pub(crate) fn handle(&self) -> TuiWindowHandle {
        self.0.clone()
    }
}

impl TuiWindowHandle {
    pub(crate) fn handle_input(&self, input: PlatformInput) {
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

    pub(crate) fn resize(&self, cols: u16, rows: u16) {
        let size = size_for_cells(cols, rows);
        {
            let mut state = self.state.borrow_mut();
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

    pub(crate) fn request_frame(&self) {
        self.draw_frame();
        self.deliver_pending_frame();
    }

    fn draw_frame(&self) {
        with_taken(
            &self.callbacks,
            |callbacks| &mut callbacks.request_frame,
            |callback| callback(RequestFrameOptions::default()),
        );
    }

    fn deliver_pending_frame(&self) {
        let Some((grid, carets)) = self.state.borrow_mut().pending_frame.take() else {
            return;
        };
        let grid = self.resolve_caret(grid, carets, CaretMode::TerminalCursor);
        let outputs = self.state.borrow().outputs.clone();
        deliver_frame(&outputs.frame_sink, grid);
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
        self.0.state.borrow_mut().title = title.to_owned();
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

fn deliver_frame(frame_sink: &RefCell<Option<Box<dyn FnMut(CellGrid)>>>, grid: CellGrid) {
    with_taken(frame_sink, |sink| sink, |sink| sink(grid));
}
