use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use gpui::{
    Action, ActivityGuard, AnyWindowHandle, BackgroundExecutor, Bounds, ClipboardItem, CursorStyle,
    DisplayId, DummyKeyboardMapper, ForegroundExecutor, Keymap, Menu, MenuItem, PathPromptOptions,
    Pixels, Platform, PlatformDisplay, PlatformInput, PlatformKeyboardLayout,
    PlatformKeyboardMapper, PlatformTextSystem, PlatformWindow, Point, RunnableVariant, Task,
    ThermalState, WindowAppearance, WindowKind, WindowParams,
};
use gpui_util::ResultExt as _;
use uuid::Uuid;

use crate::{
    dispatcher::{TuiDispatcher, run_runnable},
    grid::{CellGrid, Rgb},
    size_for_cells,
    text_system::TuiTextSystem,
    window::{FrameOutcome, TuiWindow, TuiWindowHandle},
    with_taken,
};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const BACKGROUND_FRAME_INTERVAL: Duration = Duration::from_millis(66);
const UNCHANGED_FRAME_INTERVAL: Duration = Duration::from_millis(100);
const UNCHANGED_FRAMES_BEFORE_SLOWDOWN: u32 = 2;
const INPUT_SETTLE: Duration = Duration::from_millis(6);
const INPUT_ACTIVITY: Duration = Duration::from_millis(250);
const MAX_RUNNABLE_BATCH: Duration = Duration::from_millis(8);

#[derive(Default)]
struct FramePacer {
    last_frame: Option<Instant>,
    last_input: Option<Instant>,
    settle_until: Option<Instant>,
    unchanged_frames: u32,
}

impl FramePacer {
    fn next_frame(&self) -> Option<Instant> {
        if let Some(settle_until) = self.settle_until {
            return Some(settle_until);
        }
        let last_frame = self.last_frame?;
        let follows_input = self
            .last_input
            .is_some_and(|input| last_frame.saturating_duration_since(input) < INPUT_ACTIVITY);
        let interval = if self.unchanged_frames >= UNCHANGED_FRAMES_BEFORE_SLOWDOWN {
            UNCHANGED_FRAME_INTERVAL
        } else if follows_input {
            FRAME_INTERVAL
        } else {
            BACKGROUND_FRAME_INTERVAL
        };
        Some(last_frame + interval)
    }

    fn is_due(&self, now: Instant) -> bool {
        self.next_frame().is_none_or(|next_frame| now >= next_frame)
    }

    fn frame_presented(&mut self, now: Instant, outcome: FrameOutcome) {
        self.unchanged_frames = match outcome {
            FrameOutcome::NotDrawn | FrameOutcome::Unchanged => {
                self.unchanged_frames.saturating_add(1)
            }
            FrameOutcome::Changed => 0,
        };
        self.last_frame = Some(now);
        self.settle_until = None;
    }

    fn input_received(&mut self, now: Instant, input: &PlatformInput) {
        self.unchanged_frames = 0;
        self.last_input = Some(now);
        if matches!(
            input,
            PlatformInput::KeyDown(_) | PlatformInput::MouseDown(_) | PlatformInput::MouseUp(_)
        ) {
            self.settle_until.get_or_insert(now + INPUT_SETTLE);
        }
    }

    fn settle_after_input(&mut self, now: Instant) {
        self.unchanged_frames = 0;
        self.last_input = Some(now);
        self.settle_until.get_or_insert(now + INPUT_SETTLE);
    }

    fn present_next_frame_immediately(&mut self) {
        self.unchanged_frames = 0;
        self.settle_until = None;
        self.last_frame = None;
    }
}

#[derive(Default)]
pub(crate) struct FrameOutput {
    pub(crate) sink: Option<Box<dyn FnMut(CellGrid)>>,
    pub(crate) last_delivered: Option<CellGrid>,
}

#[derive(Default)]
pub(crate) struct PlatformOutputs {
    pub(crate) frame: RefCell<FrameOutput>,
    pub(crate) title: RefCell<Option<Box<dyn FnMut(&str)>>>,
    pub(crate) icon_glyphs: RefCell<Option<Box<dyn Fn(&str) -> Option<char>>>>,
    pub(crate) canvas: Cell<Rgb>,
}

#[derive(Debug)]
pub(crate) struct TuiDisplay {
    bounds: Cell<Bounds<Pixels>>,
}

impl PlatformDisplay for TuiDisplay {
    fn id(&self) -> DisplayId {
        DisplayId::new(0)
    }

    fn uuid(&self) -> Result<Uuid> {
        Ok(Uuid::nil())
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds.get()
    }
}

struct TerminalKeyboardLayout;

impl PlatformKeyboardLayout for TerminalKeyboardLayout {
    fn id(&self) -> &str {
        "terminal"
    }

    fn name(&self) -> &str {
        "Terminal"
    }
}

pub(crate) struct WindowRegistry {
    windows: RefCell<Vec<TuiWindowHandle>>,
    active: Cell<Option<AnyWindowHandle>>,
    executor: ForegroundExecutor,
}

impl WindowRegistry {
    pub(crate) fn active(&self) -> Option<AnyWindowHandle> {
        self.active.get()
    }

    pub(crate) fn activate(self: &Rc<Self>, handle: AnyWindowHandle) {
        let previous = self.active.replace(Some(handle));
        if previous == Some(handle) {
            return;
        }
        let mut changes = vec![(handle, true)];
        changes.extend(previous.map(|previous| (previous, false)));
        self.notify_active_status(changes);
    }

    fn notify_active_status(self: &Rc<Self>, changes: Vec<(AnyWindowHandle, bool)>) {
        let registry = self.clone();
        self.executor
            .spawn(async move {
                for (handle, active) in changes.into_iter().rev() {
                    if let Some(window) = registry.window(handle) {
                        window.notify_active_status(active);
                    }
                }
            })
            .detach();
    }

    pub(crate) fn remove(self: &Rc<Self>, handle: AnyWindowHandle) {
        self.windows
            .borrow_mut()
            .retain(|window| window.handle() != handle);
        if self.active.get() == Some(handle) {
            self.active.set(None);
            let last = self.windows.borrow().last().map(TuiWindowHandle::handle);
            if let Some(last) = last {
                self.activate(last);
            }
        }
    }

    fn window(&self, handle: AnyWindowHandle) -> Option<TuiWindowHandle> {
        self.windows
            .borrow()
            .iter()
            .find(|window| window.handle() == handle)
            .cloned()
    }

    fn focused_window(&self) -> Option<TuiWindowHandle> {
        self.window(self.active.get()?)
    }

    fn open_windows(&self) -> Vec<TuiWindowHandle> {
        self.windows.borrow().clone()
    }

    fn window_under_floating(&self) -> Option<TuiWindowHandle> {
        let focused = self.focused_window()?;
        if !focused.is_floating() {
            return None;
        }
        self.windows
            .borrow()
            .iter()
            .rev()
            .find(|window| !window.is_floating())
            .cloned()
    }
}

#[derive(Default)]
struct PlatformCallbacks {
    quit: Option<Box<dyn FnMut() -> bool>>,
    open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    clipboard_write: Option<Box<dyn FnMut(String)>>,
    cursor_style_change: Option<Box<dyn FnMut(CursorStyle)>>,
}

pub struct TuiPlatform {
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    text_system: Arc<TuiTextSystem>,
    main_receiver: RefCell<Option<mpsc::Receiver<RunnableVariant>>>,
    display: Rc<TuiDisplay>,
    windows: Rc<WindowRegistry>,
    outputs: Rc<PlatformOutputs>,
    callbacks: RefCell<PlatformCallbacks>,
    cursor_style: Cell<CursorStyle>,
    clipboard: RefCell<Option<ClipboardItem>>,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    primary: RefCell<Option<ClipboardItem>>,
    #[cfg(target_os = "macos")]
    find_pasteboard: RefCell<Option<ClipboardItem>>,
    should_quit: Cell<bool>,
    pacer: RefCell<FramePacer>,
}

impl TuiPlatform {
    pub fn new(cols: u16, rows: u16) -> Rc<Self> {
        gpui::use_linux_key_conventions();
        let (main_sender, main_receiver) = mpsc::channel();
        let dispatcher = Arc::new(TuiDispatcher::new(main_sender));
        let foreground_executor = ForegroundExecutor::new(dispatcher.clone());
        Rc::new(Self {
            background_executor: BackgroundExecutor::new(dispatcher),
            windows: Rc::new(WindowRegistry {
                windows: RefCell::default(),
                active: Cell::new(None),
                executor: foreground_executor.clone(),
            }),
            foreground_executor,
            text_system: Arc::new(TuiTextSystem),
            main_receiver: RefCell::new(Some(main_receiver)),
            display: Rc::new(TuiDisplay {
                bounds: Cell::new(Bounds::new(Point::default(), size_for_cells(cols, rows))),
            }),
            outputs: Rc::default(),
            callbacks: RefCell::default(),
            cursor_style: Cell::new(CursorStyle::default()),
            clipboard: RefCell::default(),
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            primary: RefCell::default(),
            #[cfg(target_os = "macos")]
            find_pasteboard: RefCell::default(),
            should_quit: Cell::new(false),
            pacer: RefCell::default(),
        })
    }

    pub fn set_frame_sink(&self, sink: impl FnMut(CellGrid) + 'static) {
        *self.outputs.frame.borrow_mut() = FrameOutput {
            sink: Some(Box::new(sink)),
            last_delivered: None,
        };
    }

    pub fn set_icon_glyphs(&self, glyphs: impl Fn(&str) -> Option<char> + 'static) {
        *self.outputs.icon_glyphs.borrow_mut() = Some(Box::new(glyphs));
    }

    pub fn set_canvas(&self, canvas: Rgb) {
        self.outputs.canvas.set(canvas);
    }

    pub fn on_title_change(&self, callback: impl FnMut(&str) + 'static) {
        *self.outputs.title.borrow_mut() = Some(Box::new(callback));
    }

    pub fn on_clipboard_write(&self, callback: impl FnMut(String) + 'static) {
        self.callbacks.borrow_mut().clipboard_write = Some(Box::new(callback));
    }

    pub fn on_cursor_style_change(&self, callback: impl FnMut(CursorStyle) + 'static) {
        self.callbacks.borrow_mut().cursor_style_change = Some(Box::new(callback));
    }

    pub(crate) fn focused_window(&self) -> Option<TuiWindowHandle> {
        self.windows.focused_window()
    }

    pub fn open_urls(&self, urls: Vec<String>) {
        with_taken(
            &self.callbacks,
            |callbacks| &mut callbacks.open_urls,
            |callback| callback(urls),
        );
    }

    pub fn handle_input(&self, input: PlatformInput) {
        self.pacer
            .borrow_mut()
            .input_received(Instant::now(), &input);
        if let Some(window) = self.focused_window() {
            window.handle_input(input);
        }
    }

    pub fn insert_text(&self, text: &str) {
        self.pacer.borrow_mut().settle_after_input(Instant::now());
        if let Some(window) = self.focused_window() {
            window.insert_text(text);
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        self.pacer.borrow_mut().present_next_frame_immediately();
        self.display
            .bounds
            .set(Bounds::new(Point::default(), size_for_cells(cols, rows)));
        for window in self.windows.open_windows() {
            window.resize(cols, rows);
        }
    }

    fn present_frame(&self, now: Instant) {
        if let Some(underneath) = self.windows.window_under_floating()
            && let Some(underlay) = underneath.draw_underlay()
            && let Some(window) = self.focused_window()
        {
            window.set_underlay(underlay);
        }
        if let Some(window) = self.focused_window() {
            let outcome = window.request_frame();
            self.pacer.borrow_mut().frame_presented(now, outcome);
        }
    }

    fn frame_wait(&self) -> Duration {
        self.pacer
            .borrow()
            .next_frame()
            .map_or(Duration::ZERO, |next_frame| {
                next_frame.saturating_duration_since(Instant::now())
            })
    }
}

impl Platform for TuiPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.background_executor.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground_executor.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn 'static + FnOnce()>) {
        on_finish_launching();

        let Some(receiver) = self.main_receiver.borrow_mut().take() else {
            log::error!("the terminal platform is already running");
            return;
        };

        while !self.should_quit.get() {
            match receiver.recv_timeout(self.frame_wait()) {
                Ok(runnable) => {
                    run_runnable(runnable);
                    let batch_start = Instant::now();
                    while batch_start.elapsed() < MAX_RUNNABLE_BATCH {
                        match receiver.try_recv() {
                            Ok(runnable) => run_runnable(runnable),
                            Err(_) => break,
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            let now = Instant::now();
            if self.pacer.borrow().is_due(now) {
                self.present_frame(now);
            }
        }

        let quit = self.callbacks.borrow_mut().quit.take();
        if let Some(mut quit) = quit {
            quit();
        }
    }

    fn quit(&self) {
        self.should_quit.set(true);
    }

    fn restart(&self, _binary_path: Option<PathBuf>, _arguments: Vec<OsString>) {}

    fn activate(&self, _ignoring_other_apps: bool) {}

    fn hide(&self) {}

    fn hide_other_apps(&self) {}

    fn unhide_other_apps(&self) {}

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![self.display.clone()]
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.display.clone())
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        self.windows.active()
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        mut options: WindowParams,
    ) -> Result<Box<dyn PlatformWindow>> {
        let floating_request = matches!(options.kind, WindowKind::Floating | WindowKind::PopUp)
            .then_some(options.bounds);
        options.bounds = self.display.bounds();
        let focus = options.focus;
        let window = TuiWindow::new(
            handle,
            options,
            floating_request,
            self.display.clone(),
            self.outputs.clone(),
            Rc::downgrade(&self.windows),
        );
        self.windows.windows.borrow_mut().push(window.handle());
        if focus || self.windows.active().is_none() {
            self.windows.activate(handle);
        }
        Ok(Box::new(window))
    }

    fn window_appearance(&self) -> WindowAppearance {
        WindowAppearance::Dark
    }

    fn open_url(&self, url: &str) {
        open::that_detached(url).log_err();
    }

    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        self.callbacks.borrow_mut().open_urls = Some(callback);
    }

    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow!(
            "the terminal platform cannot register URL schemes"
        )))
    }

    fn prompt_for_paths(
        &self,
        _options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (sender, receiver) = oneshot::channel();
        sender.send(Ok(None)).ok();
        receiver
    }

    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        _suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (sender, receiver) = oneshot::channel();
        sender.send(Ok(None)).ok();
        receiver
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }

    fn reveal_path(&self, path: &Path) {
        let directory = if path.is_dir() {
            Some(path)
        } else {
            path.parent()
        };
        if let Some(directory) = directory {
            open::that_detached(directory).log_err();
        }
    }

    fn open_with_system(&self, path: &Path) {
        open::that_detached(path).log_err();
    }

    fn on_quit(&self, callback: Box<dyn FnMut() -> bool>) {
        self.callbacks.borrow_mut().quit = Some(callback);
    }

    fn on_reopen(&self, _callback: Box<dyn FnMut()>) {}

    fn on_system_sleep(&self, _callback: Box<dyn FnMut()>) {}

    fn on_system_wake(&self, _callback: Box<dyn FnMut()>) {}

    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {}

    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {}

    fn on_app_menu_action(&self, _callback: Box<dyn FnMut(&dyn Action)>) {}

    fn on_will_open_app_menu(&self, _callback: Box<dyn FnMut()>) {}

    fn on_validate_app_menu_command(&self, _callback: Box<dyn FnMut(&dyn Action) -> bool>) {}

    fn thermal_state(&self) -> ThermalState {
        ThermalState::Nominal
    }

    fn on_thermal_state_change(&self, _callback: Box<dyn FnMut()>) {}

    fn prevent_idle_sleep(&self, _reason: &str) -> Task<Result<ActivityGuard>> {
        Task::ready(Ok(ActivityGuard::noop()))
    }

    fn app_path(&self) -> Result<PathBuf> {
        Ok(std::env::current_exe()?)
    }

    fn path_for_auxiliary_executable(&self, name: &str) -> Result<PathBuf> {
        Err(anyhow!(
            "no auxiliary executable {name} in the terminal platform"
        ))
    }

    fn set_cursor_style(&self, style: CursorStyle) {
        if self.cursor_style.replace(style) == style {
            return;
        }
        with_taken(
            &self.callbacks,
            |callbacks| &mut callbacks.cursor_style_change,
            |callback| callback(style),
        );
    }

    fn hide_cursor_until_mouse_moves(&self) {}

    fn is_cursor_visible(&self) -> bool {
        true
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        true
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        self.clipboard.borrow().clone()
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        let text = item.text();
        *self.clipboard.borrow_mut() = Some(item);
        if let Some(text) = text {
            with_taken(
                &self.callbacks,
                |callbacks| &mut callbacks.clipboard_write,
                |callback| callback(text),
            );
        }
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn read_from_primary(&self) -> Option<ClipboardItem> {
        self.primary.borrow().clone()
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn write_to_primary(&self, item: ClipboardItem) {
        *self.primary.borrow_mut() = Some(item);
    }

    #[cfg(target_os = "macos")]
    fn read_from_find_pasteboard(&self) -> Option<ClipboardItem> {
        self.find_pasteboard.borrow().clone()
    }

    #[cfg(target_os = "macos")]
    fn write_to_find_pasteboard(&self, item: ClipboardItem) {
        *self.find_pasteboard.borrow_mut() = Some(item);
    }

    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Task<Result<()>> {
        Task::ready(Err(anyhow!(
            "the terminal platform has no credential store"
        )))
    }

    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(Ok(None))
    }

    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(TerminalKeyboardLayout)
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, _callback: Box<dyn FnMut()>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_frames_slow_down_until_input_arrives() {
        let (start, mut pacer) = pacer_after_frame();
        for _ in 0..UNCHANGED_FRAMES_BEFORE_SLOWDOWN {
            pacer.frame_presented(start, FrameOutcome::Unchanged);
        }
        assert_eq!(pacer.next_frame(), Some(start + UNCHANGED_FRAME_INTERVAL));

        pacer.input_received(start, &PlatformInput::MouseMove(Default::default()));
        assert_eq!(pacer.next_frame(), Some(start + FRAME_INTERVAL));

        pacer.frame_presented(start, FrameOutcome::Unchanged);
        pacer.frame_presented(start, FrameOutcome::Changed);
        assert_eq!(pacer.next_frame(), Some(start + FRAME_INTERVAL));
    }

    fn pacer_after_frame() -> (Instant, FramePacer) {
        let start = Instant::now();
        let mut pacer = FramePacer::default();
        pacer.frame_presented(start, FrameOutcome::Changed);
        (start, pacer)
    }

    fn key_down() -> PlatformInput {
        PlatformInput::KeyDown(gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("a").unwrap(),
            is_held: false,
            prefer_character_input: false,
        })
    }

    #[test]
    fn key_presses_are_drawn_after_a_short_settle_window() {
        let (start, mut pacer) = pacer_after_frame();
        let typed = start + Duration::from_millis(5);
        pacer.input_received(typed, &key_down());
        assert!(!pacer.is_due(typed));
        assert!(pacer.is_due(typed + INPUT_SETTLE));

        pacer.input_received(typed + INPUT_SETTLE, &key_down());
        assert_eq!(pacer.next_frame(), Some(typed + INPUT_SETTLE));

        let drawn = typed + INPUT_SETTLE;
        pacer.frame_presented(drawn, FrameOutcome::Changed);
        pacer.settle_after_input(drawn);
        assert_eq!(pacer.next_frame(), Some(drawn + INPUT_SETTLE));
    }

    #[test]
    fn frames_long_after_input_use_the_background_interval() {
        let (start, mut pacer) = pacer_after_frame();
        assert_eq!(pacer.next_frame(), Some(start + BACKGROUND_FRAME_INTERVAL));

        let typed = start + Duration::from_secs(1);
        pacer.input_received(typed, &key_down());
        pacer.frame_presented(typed + INPUT_SETTLE, FrameOutcome::Changed);
        assert_eq!(
            pacer.next_frame(),
            Some(typed + INPUT_SETTLE + FRAME_INTERVAL)
        );

        let later = typed + INPUT_ACTIVITY;
        pacer.frame_presented(later, FrameOutcome::Changed);
        assert_eq!(pacer.next_frame(), Some(later + BACKGROUND_FRAME_INTERVAL));
    }

    #[test]
    fn resizes_are_drawn_immediately() {
        let (start, mut pacer) = pacer_after_frame();
        pacer.input_received(start, &key_down());
        pacer.present_next_frame_immediately();
        assert!(pacer.is_due(start));
    }

    #[test]
    fn cursor_styles_reach_the_callback_once_per_change() {
        let platform = TuiPlatform::new(10, 2);
        let reported: Rc<RefCell<Vec<CursorStyle>>> = Rc::default();
        platform.on_cursor_style_change({
            let reported = reported.clone();
            move |style| reported.borrow_mut().push(style)
        });
        for style in [
            CursorStyle::Arrow,
            CursorStyle::IBeam,
            CursorStyle::IBeam,
            CursorStyle::PointingHand,
            CursorStyle::PointingHand,
            CursorStyle::Arrow,
        ] {
            platform.set_cursor_style(style);
        }
        assert_eq!(
            *reported.borrow(),
            [
                CursorStyle::IBeam,
                CursorStyle::PointingHand,
                CursorStyle::Arrow
            ]
        );
    }
}
