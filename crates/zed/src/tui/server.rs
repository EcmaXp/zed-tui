use std::{
    cell::RefCell,
    ffi::OsString,
    fs,
    io::{BufReader, ErrorKind, Write as _},
    net::Shutdown,
    os::unix::{
        ffi::{OsStrExt as _, OsStringExt as _},
        fs::{DirBuilderExt as _, PermissionsExt as _},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
use cli::{CliRequest, CliResponse, CliResponseSink};
use collections::HashMap;
use futures::{
    StreamExt as _,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
};
use gpui::{App, AsyncApp, CursorStyle};
use gpui_tui::{CellGrid, TuiPlatform};
use parking_lot::Mutex;
use util::ResultExt as _;
use workspace::{AppState, MultiWorkspace};

use crate::tui::{
    input::{InputTranslator, Translated},
    protocol::{
        ClientMessage, FrameEncoder, MessageReader, MessageWriter, PROTOCOL_VERSION, ServerMessage,
        TermEvent, WAIT_ONLY_SIZE, read_message, write_message,
    },
};

const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(30);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_FRAMES_IN_FLIGHT: u32 = 2;
const MAX_SESSION_BASENAME: usize = 32;
const FIRST_WINDOW_POLL_INTERVAL: Duration = Duration::from_millis(50);
pub const SESSION_ENV: &str = "ZED_TUI_SESSION";
pub const ALREADY_RUNNING_EXIT_CODE: i32 = 3;

#[derive(Debug)]
pub struct AlreadyRunning(String);

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "session {:?} is already running", self.0)
    }
}

impl std::error::Error for AlreadyRunning {}

pub struct SessionPaths {
    pub name: String,
    pub directory: PathBuf,
    pub socket: PathBuf,
    pub pid: PathBuf,
    pub log: PathBuf,
    pub root: PathBuf,
}

fn is_session_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')
}

impl SessionPaths {
    pub fn new(session: &str) -> Result<Self> {
        let is_valid = !session.is_empty()
            && !session.starts_with('.')
            && session.chars().all(is_session_name_char);
        if !is_valid {
            bail!("invalid session name {session:?}: use letters, digits, '-', '_' and '.'");
        }
        Ok(Self::in_directory(session, sessions_dir().join(session)))
    }

    fn in_directory(name: &str, directory: PathBuf) -> Self {
        Self {
            name: name.to_owned(),
            socket: directory.join("server.sock"),
            pid: directory.join("server.pid"),
            log: directory.join("server.log"),
            root: directory.join("root"),
            directory,
        }
    }
}

pub struct RunningSession {
    pub name: String,
    pub root: Option<PathBuf>,
}

pub fn canonical_target(path: &Path, current_dir: &Path) -> PathBuf {
    let path = current_dir.join(path);
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => match parent.canonicalize() {
            Ok(parent) => parent.join(name),
            Err(_) => path,
        },
        _ => path,
    }
}

pub fn session_root_for(target: &Path) -> PathBuf {
    let start = if target.is_dir() {
        target
    } else {
        target.parent().unwrap_or(target)
    };
    start
        .ancestors()
        .find(|directory| directory.join(".git").symlink_metadata().is_ok())
        .unwrap_or(start)
        .to_path_buf()
}

pub fn session_name_for_root(root: &Path) -> String {
    let basename = root
        .file_name()
        .map(|name| {
            name.to_string_lossy()
                .chars()
                .map(|ch| if is_session_name_char(ch) { ch } else { '_' })
                .skip_while(|ch| *ch == '.')
                .take(MAX_SESSION_BASENAME)
                .collect::<String>()
        })
        .unwrap_or_default();
    let basename = if basename.is_empty() {
        "root"
    } else {
        &basename
    };
    format!(
        "{basename}-{:08x}",
        fnv1a(root.as_os_str().as_bytes()) as u32
    )
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn write_root(session_paths: &SessionPaths, root: &Path) -> Result<()> {
    fs::write(&session_paths.root, root.as_os_str().as_bytes())
        .with_context(|| format!("writing {}", session_paths.root.display()))
}

pub fn read_root(session_paths: &SessionPaths) -> Option<PathBuf> {
    let bytes = fs::read(&session_paths.root).ok()?;
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

pub fn deepest_covering<'a>(
    sessions: &'a [RunningSession],
    target: &Path,
) -> Option<&'a RunningSession> {
    sessions
        .iter()
        .filter_map(|session| {
            let root = session.root.as_deref()?;
            target
                .starts_with(root)
                .then(|| (root.components().count(), session))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, session)| session)
}

fn sessions_dir() -> PathBuf {
    paths::data_dir()
        .join("tui")
        .join(release_channel::RELEASE_CHANNEL.dev_name())
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("creating {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", path.display()))
}

pub fn is_running(paths: &SessionPaths) -> bool {
    UnixStream::connect(&paths.socket).is_ok()
}

enum Outgoing {
    Frame(Arc<CellGrid>),
    Message(ServerMessage),
    Rendered(u32),
}

struct ClientHandle {
    id: u64,
    receives_frames: bool,
    sender: mpsc::Sender<Outgoing>,
    writer: thread::JoinHandle<()>,
}

#[derive(Default)]
struct HubState {
    clients: Vec<ClientHandle>,
    last_frame: Option<Arc<CellGrid>>,
    last_title: Option<String>,
    pointer: CursorStyle,
    pointer_owner: Option<u64>,
}

impl HubState {
    fn client(&self, id: u64) -> Option<&ClientHandle> {
        self.clients.iter().find(|client| client.id == id)
    }

    fn send_pointer_to_owner(&self) {
        let Some(owner) = self.pointer_owner else {
            return;
        };
        if let Some(client) = self.client(owner) {
            client
                .sender
                .send(Outgoing::Message(ServerMessage::Pointer(self.pointer)))
                .log_err();
        }
    }
}

#[derive(Default)]
struct ClientHub {
    state: Mutex<HubState>,
    next_id: AtomicU64,
}

impl ClientHub {
    fn broadcast_frame(&self, grid: Arc<CellGrid>) {
        let mut state = self.state.lock();
        state.last_frame = Some(grid.clone());
        state.clients.retain(|client| {
            !client.receives_frames || client.sender.send(Outgoing::Frame(grid.clone())).is_ok()
        });
    }

    fn broadcast_message(&self, message: ServerMessage) {
        self.state.lock().clients.retain(|client| {
            client
                .sender
                .send(Outgoing::Message(message.clone()))
                .is_ok()
        });
    }

    fn shut_down(&self) {
        let clients = std::mem::take(&mut self.state.lock().clients);
        let writers = clients
            .into_iter()
            .filter(|client| {
                client
                    .sender
                    .send(Outgoing::Message(ServerMessage::Shutdown))
                    .is_ok()
            })
            .map(|client| client.writer)
            .collect::<Vec<_>>();
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        for writer in writers {
            while !writer.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn send_last_frame_to(&self, id: u64, size: (u16, u16)) {
        let state = self.state.lock();
        let Some(frame) = state
            .last_frame
            .clone()
            .filter(|frame| (frame.cols, frame.rows) == size)
        else {
            return;
        };
        if let Some(client) = state.client(id) {
            client.sender.send(Outgoing::Frame(frame)).log_err();
        }
    }

    fn send_message_to(&self, id: u64, message: ServerMessage) -> bool {
        self.state
            .lock()
            .client(id)
            .is_some_and(|client| client.sender.send(Outgoing::Message(message)).is_ok())
    }

    fn has_client(&self, id: u64) -> bool {
        self.state.lock().client(id).is_some()
    }

    fn set_pointer(&self, style: CursorStyle) {
        let mut state = self.state.lock();
        state.pointer = style;
        state.send_pointer_to_owner();
    }

    fn claim_pointer(&self, id: u64) {
        let mut state = self.state.lock();
        if state.pointer_owner.replace(id) != Some(id) {
            state.send_pointer_to_owner();
        }
    }

    fn remove(&self, id: u64) {
        let mut state = self.state.lock();
        state.clients.retain(|client| client.id != id);
        if state.pointer_owner == Some(id) {
            state.pointer_owner = None;
        }
    }
}

enum ServerEvent {
    Resized {
        id: u64,
        cols: u16,
        rows: u16,
    },
    Disconnected {
        id: u64,
    },
    Input(TermEvent),
    Open(Vec<PathBuf>),
    OpenAndWait {
        id: u64,
        paths: Vec<PathBuf>,
        quit_session: bool,
    },
    Kill,
}

pub struct Started {
    pub on_frame: Box<dyn FnMut(CellGrid)>,
    pub after_start: Box<dyn FnOnce(&mut App)>,
}

pub struct SessionGuard {
    hub: Arc<ClientHub>,
    session_paths: SessionPaths,
    _pid_lock: fs::File,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.hub.shut_down();
        fs::remove_file(&self.session_paths.socket).log_err();
        fs::remove_file(&self.session_paths.pid).log_err();
    }
}

fn lock_session(session_paths: &SessionPaths) -> Result<fs::File> {
    create_private_dir(&session_paths.directory)?;
    let mut pid_lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&session_paths.pid)
        .with_context(|| format!("opening {}", session_paths.pid.display()))?;
    if pid_lock.try_lock().is_err() || is_running(session_paths) {
        return Err(AlreadyRunning(session_paths.name.clone()).into());
    }
    pid_lock.set_len(0)?;
    write!(pid_lock, "{}", std::process::id())?;
    Ok(pid_lock)
}

pub fn start_session(
    session_paths: SessionPaths,
    root: Option<&Path>,
    platform: Rc<TuiPlatform>,
) -> Result<(SessionGuard, Started)> {
    let pid_lock = lock_session(&session_paths)?;
    if let Some(root) = root {
        write_root(&session_paths, root)?;
    }
    match fs::remove_file(&session_paths.socket) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("removing a stale socket"),
    }
    let listener = UnixListener::bind(&session_paths.socket)
        .with_context(|| format!("binding {}", session_paths.socket.display()))?;
    log::info!(
        "serving session {:?} on {}",
        session_paths.name,
        session_paths.socket.display()
    );

    let hub = Arc::new(ClientHub::default());
    let (event_sender, event_receiver) = unbounded();
    thread::Builder::new().name("Accept".to_owned()).spawn({
        let hub = hub.clone();
        move || accept_clients(listener, hub, event_sender)
    })?;

    platform.on_title_change({
        let hub = hub.clone();
        move |title| {
            hub.state.lock().last_title = Some(title.to_string());
            hub.broadcast_message(ServerMessage::Title(title.to_string()));
        }
    });
    platform.on_clipboard_write({
        let hub = hub.clone();
        move |text| hub.broadcast_message(ServerMessage::Clipboard(text))
    });
    platform.on_cursor_style_change({
        let hub = hub.clone();
        move |style| hub.set_pointer(style)
    });

    let started = Started {
        on_frame: Box::new({
            let hub = hub.clone();
            move |grid| hub.broadcast_frame(Arc::new(grid))
        }),
        after_start: Box::new({
            let hub = hub.clone();
            move |cx| {
                cx.spawn(async move |cx| handle_events(platform, hub, event_receiver, cx).await)
                    .detach();
            }
        }),
    };
    let session = SessionGuard {
        hub,
        session_paths,
        _pid_lock: pid_lock,
    };
    Ok((session, started))
}

fn print_snapshot(grid: Option<CellGrid>, ansi: bool) {
    let Some(grid) = grid else {
        eprintln!("zed --tui: no frame was rendered");
        return;
    };
    if !ansi {
        println!("{}", grid.text());
        return;
    }
    match crate::tui::client::ansi_text(&grid) {
        Ok(text) => print!("{text}"),
        Err(error) => eprintln!("zed --tui: {error:#}"),
    }
}

pub fn start_snapshot(
    platform: Rc<TuiPlatform>,
    wait: Duration,
    keys: Vec<String>,
    ansi: bool,
) -> Result<Started> {
    let keystrokes = keys
        .iter()
        .map(|key| {
            gpui::Keystroke::parse(key)
                .map(gpui::Keystroke::with_simulated_ime)
                .with_context(|| format!("parsing key {key:?}"))
        })
        .collect::<Result<Vec<_>>>()?;
    let last_frame: Rc<RefCell<Option<CellGrid>>> = Rc::default();
    Ok(Started {
        on_frame: Box::new({
            let last_frame = last_frame.clone();
            move |grid| *last_frame.borrow_mut() = Some(grid)
        }),
        after_start: Box::new(move |cx| {
            cx.spawn(async move |cx| {
                cx.background_executor().timer(wait / 2).await;
                for keystroke in keystrokes {
                    platform.handle_input(gpui::PlatformInput::KeyDown(gpui::KeyDownEvent {
                        keystroke,
                        is_held: false,
                        prefer_character_input: false,
                    }));
                    cx.background_executor()
                        .timer(Duration::from_millis(100))
                        .await;
                }
                cx.background_executor().timer(wait / 2).await;
                print_snapshot(last_frame.borrow_mut().take(), ansi);
                cx.update(|cx| cx.quit());
            })
            .detach();
        }),
    })
}

fn file_urls(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .filter_map(|path| match url::Url::from_file_path(path) {
            Ok(url) => Some(url.to_string()),
            Err(()) => {
                log::error!("cannot open {}: not an absolute path", path.display());
                None
            }
        })
        .collect()
}

async fn handle_events(
    platform: Rc<TuiPlatform>,
    hub: Arc<ClientHub>,
    mut events: UnboundedReceiver<ServerEvent>,
    cx: &mut AsyncApp,
) {
    let mut sizes: HashMap<u64, (u16, u16)> = HashMap::default();
    let mut translator = InputTranslator::default();

    while let Some(event) = events.next().await {
        handle_event(event, &platform, &hub, &mut sizes, &mut translator, cx);
    }
}

fn handle_event(
    event: ServerEvent,
    platform: &TuiPlatform,
    hub: &Arc<ClientHub>,
    sizes: &mut HashMap<u64, (u16, u16)>,
    translator: &mut InputTranslator,
    cx: &mut AsyncApp,
) {
    match event {
        ServerEvent::Resized { id, cols, rows } => {
            let attached = sizes.insert(id, (cols, rows)).is_none();
            if let Some(shared) = apply_shared_size(platform, sizes)
                && attached
            {
                hub.send_last_frame_to(id, shared);
            }
            platform.set_presenting(true);
        }
        ServerEvent::Disconnected { id } => {
            sizes.remove(&id);
            if apply_shared_size(platform, sizes).is_none() {
                platform.set_presenting(false);
            }
        }
        ServerEvent::Input(event) => {
            for translated in translator.translate(event) {
                match translated {
                    Translated::Input(input) => platform.handle_input(input),
                    Translated::Text(text) => platform.insert_text(&text),
                }
            }
        }
        ServerEvent::Open(paths) => platform.open_urls(file_urls(&paths)),
        ServerEvent::OpenAndWait {
            id,
            paths,
            quit_session,
        } => {
            let hub = hub.clone();
            cx.spawn(async move |cx| open_and_wait(hub, id, paths, quit_session, cx).await)
                .detach();
        }
        ServerEvent::Kill => {
            cx.update(|cx| cx.quit());
        }
    }
}

struct WaitSink {
    hub: Arc<ClientHub>,
    id: u64,
    errors: Mutex<Vec<String>>,
    delivered: Arc<AtomicBool>,
}

impl CliResponseSink for WaitSink {
    fn send(&self, response: CliResponse) -> Result<()> {
        match response {
            CliResponse::Ping if self.hub.has_client(self.id) => Ok(()),
            CliResponse::Ping => Err(anyhow!("client {} stopped waiting", self.id)),
            CliResponse::Stdout { message } | CliResponse::Stderr { message } => {
                log::info!("waiting client {}: {message}", self.id);
                self.errors.lock().push(message);
                Ok(())
            }
            CliResponse::Exit { status } => {
                let errors = std::mem::take(&mut *self.errors.lock());
                let delivered = self
                    .hub
                    .send_message_to(self.id, ServerMessage::WaitFinished { status, errors });
                self.delivered.store(delivered, Ordering::SeqCst);
                if delivered {
                    Ok(())
                } else {
                    Err(anyhow!("client {} left before its wait finished", self.id))
                }
            }
            CliResponse::PromptOpenBehavior => Err(anyhow!("cannot prompt a waiting client")),
        }
    }
}

async fn open_and_wait(
    hub: Arc<ClientHub>,
    id: u64,
    paths: Vec<PathBuf>,
    quit_session: bool,
    cx: &mut AsyncApp,
) {
    let Some(app_state) = cx.update(|cx| AppState::try_global(cx)) else {
        hub.send_message_to(
            id,
            ServerMessage::WaitFinished {
                status: 1,
                errors: vec!["the session has not started".to_owned()],
            },
        );
        return;
    };
    let deadline = Instant::now() + DAEMON_START_TIMEOUT;
    while !cx.update(|cx| {
        cx.windows()
            .iter()
            .any(|window| window.downcast::<MultiWorkspace>().is_some())
    }) && Instant::now() < deadline
    {
        cx.background_executor()
            .timer(FIRST_WINDOW_POLL_INTERVAL)
            .await;
    }

    let delivered = Arc::new(AtomicBool::new(false));
    let sink = WaitSink {
        hub,
        id,
        errors: Mutex::default(),
        delivered: delivered.clone(),
    };
    let (requests, receiver) = unbounded();
    requests
        .unbounded_send(CliRequest::Open {
            paths: paths
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            urls: Vec::new(),
            diff_paths: Vec::new(),
            diff_all: false,
            wsl: None,
            wait: true,
            open_behavior: cli::OpenBehavior::Add,
            env: None,
            user_data_dir: None,
            dev_container: false,
            cwd: None,
        })
        .log_err();
    drop(requests);
    crate::zed::handle_cli_connection((receiver, Box::new(sink)), app_state, cx).await;
    if quit_session && delivered.load(Ordering::SeqCst) {
        cx.update(|cx| cx.quit());
    }
}

fn apply_shared_size(
    platform: &TuiPlatform,
    sizes: &HashMap<u64, (u16, u16)>,
) -> Option<(u16, u16)> {
    let cols = sizes.values().map(|(cols, _)| *cols).min()?.max(1);
    let rows = sizes.values().map(|(_, rows)| *rows).min()?.max(1);
    platform.resize(cols, rows);
    Some((cols, rows))
}

fn accept_clients(
    listener: UnixListener,
    hub: Arc<ClientHub>,
    events: UnboundedSender<ServerEvent>,
) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let hub = hub.clone();
                let events = events.clone();
                let spawn_result = thread::Builder::new()
                    .name("ClientHandshake".to_owned())
                    .spawn(move || {
                        if let Err(error) = serve_client(stream, &hub, &events) {
                            log::error!("failed to serve client: {error:#}");
                        }
                    });
                if let Err(error) = spawn_result {
                    log::error!("failed to spawn a client thread: {error}");
                }
            }
            Err(error) => log::error!("failed to accept client: {error}"),
        }
    }
}

fn serve_client(
    stream: UnixStream,
    hub: &Arc<ClientHub>,
    events: &UnboundedSender<ServerEvent>,
) -> Result<()> {
    let id = hub.next_id.fetch_add(1, Ordering::SeqCst);
    if let Err(error) = stream.set_read_timeout(Some(HELLO_TIMEOUT)) {
        log::debug!("client {id} closed before the hello timeout was set: {error}");
    }
    let write_stream = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    let hello = match read_message(&mut reader) {
        Ok(message) => message,
        Err(error) if is_closed_connection(&error) => {
            log::debug!("connection closed before sending a hello");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let (cols, rows) = match hello {
        ClientMessage::Hello {
            version,
            cols,
            rows,
        } => {
            if version != PROTOCOL_VERSION {
                let error = format!(
                    "the session runs protocol {PROTOCOL_VERSION} but this zed speaks {version}; \
                     restart the session with `zed --tui kill`"
                );
                write_message(&mut &write_stream, &ServerMessage::Error(error)).log_err();
                bail!("client speaks protocol {version}, expected {PROTOCOL_VERSION}");
            }
            (cols, rows)
        }
        ClientMessage::Kill => {
            events.unbounded_send(ServerEvent::Kill).log_err();
            return Ok(());
        }
        ClientMessage::Open { paths } => {
            events.unbounded_send(ServerEvent::Open(paths)).log_err();
            return Ok(());
        }
        other => bail!("expected a hello message, got {other:?}"),
    };
    if let Err(error) = reader.get_ref().set_read_timeout(None) {
        log::debug!("client {id} closed right after its hello: {error}");
    }

    let receives_frames = (cols, rows) != WAIT_ONLY_SIZE;
    let (sender, receiver) = mpsc::channel();
    let render_acks = sender.clone();
    let writer = thread::Builder::new()
        .name(format!("ClientWriter-{id}"))
        .spawn(move || write_to_client(write_stream, receiver))?;
    {
        let mut state = hub.state.lock();
        if let Some(title) = state.last_title.clone().filter(|_| receives_frames) {
            sender
                .send(Outgoing::Message(ServerMessage::Title(title)))
                .log_err();
        }
        state.clients.push(ClientHandle {
            id,
            receives_frames,
            sender,
            writer,
        });
    }

    if receives_frames {
        events
            .unbounded_send(ServerEvent::Resized { id, cols, rows })
            .log_err();
        log::info!("client {id} attached at {cols}x{rows}");
    } else {
        log::info!("client {id} attached to wait without frames");
    }

    let hub = hub.clone();
    let events = events.clone();
    thread::Builder::new()
        .name(format!("ClientReader-{id}"))
        .spawn(move || {
            let detached = read_from_client(id, reader, &hub, &events, &render_acks);
            hub.remove(id);
            events
                .unbounded_send(ServerEvent::Disconnected { id })
                .log_err();
            log::info!("client {id} disconnected (detached: {detached})");
        })?;
    Ok(())
}

fn is_closed_connection(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == ErrorKind::UnexpectedEof)
}

fn read_from_client(
    id: u64,
    mut reader: BufReader<UnixStream>,
    hub: &ClientHub,
    events: &UnboundedSender<ServerEvent>,
    render_acks: &mpsc::Sender<Outgoing>,
) -> bool {
    let mut message_reader = MessageReader::default();
    loop {
        let event = match message_reader.read(&mut reader) {
            Ok(ClientMessage::Input(event)) => {
                if matches!(event, TermEvent::Mouse { .. }) {
                    hub.claim_pointer(id);
                }
                ServerEvent::Input(event)
            }
            Ok(ClientMessage::Resize { cols, rows }) => ServerEvent::Resized { id, cols, rows },
            Ok(ClientMessage::Kill) => ServerEvent::Kill,
            Ok(ClientMessage::Open { paths }) => ServerEvent::Open(paths),
            Ok(ClientMessage::OpenAndWait {
                paths,
                quit_session,
            }) => ServerEvent::OpenAndWait {
                id,
                paths,
                quit_session,
            },
            Ok(ClientMessage::Detach) => return true,
            Ok(ClientMessage::Hello { .. }) => continue,
            Ok(ClientMessage::Rendered(frames)) => {
                render_acks.send(Outgoing::Rendered(frames)).ok();
                continue;
            }
            Err(error) => {
                log::debug!("client {id} disconnected: {error:#}");
                return false;
            }
        };
        if events.unbounded_send(event).is_err() {
            return false;
        }
    }
}

fn write_to_client(mut stream: UnixStream, receiver: mpsc::Receiver<Outgoing>) {
    stream.set_write_timeout(Some(WRITE_TIMEOUT)).log_err();
    send_to_client(&mut stream, receiver);
    stream.shutdown(Shutdown::Both).ok();
}

fn send_to_client(stream: &mut UnixStream, receiver: mpsc::Receiver<Outgoing>) {
    let mut writer = MessageWriter::default();
    let mut encoder = FrameEncoder;
    let mut last_sent: Option<Arc<CellGrid>> = None;
    let mut newest_frame: Option<Arc<CellGrid>> = None;
    let mut frames_in_flight: u32 = 0;
    while let Ok(first) = receiver.recv() {
        for outgoing in std::iter::once(first).chain(receiver.try_iter()) {
            match outgoing {
                Outgoing::Frame(grid) => newest_frame = Some(grid),
                Outgoing::Rendered(frames) => {
                    frames_in_flight = frames_in_flight.saturating_sub(frames)
                }
                Outgoing::Message(message) => {
                    let is_shutdown = matches!(message, ServerMessage::Shutdown);
                    writer.push(&message).log_err();
                    if is_shutdown {
                        writer.flush(stream).ok();
                        return;
                    }
                }
            }
        }
        if frames_in_flight < MAX_FRAMES_IN_FLIGHT
            && let Some(grid) = newest_frame.take()
        {
            if let Some(update) = encoder.update(last_sent.as_deref(), &grid)
                && writer.push(&update).log_err().is_some()
            {
                frames_in_flight += 1;
            }
            last_sent = Some(grid);
        }
        if writer.flush(stream).is_err() {
            return;
        }
    }
}

pub fn spawn_daemon(
    session_paths: &SessionPaths,
    root: &Path,
    paths_to_open: &[PathBuf],
    user_data_dir: Option<&Path>,
) -> Result<bool> {
    create_private_dir(&session_paths.directory)?;
    let log = fs::File::create(&session_paths.log)
        .with_context(|| format!("creating {}", session_paths.log.display()))?;
    let mut command = util::command::new_std_command(std::env::current_exe()?);
    command.arg("--tui");
    if let Some(user_data_dir) = user_data_dir {
        command.arg("--user-data-dir").arg(user_data_dir);
    }
    command
        .arg("--session")
        .arg(&session_paths.name)
        .arg("server")
        .arg("--root")
        .arg(root)
        .arg(root)
        .args(paths_to_open)
        .env(SESSION_ENV, &session_paths.name);
    util::set_pre_exec_to_start_new_session(&mut command);
    let mut child = smol::process::Command::from(command)
        .stdin(smol::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .context("spawning the session server")?;

    let deadline = Instant::now() + DAEMON_START_TIMEOUT;
    while Instant::now() < deadline {
        if is_running(session_paths) {
            let owner = fs::read_to_string(&session_paths.pid).unwrap_or_default();
            return Ok(owner.trim() == child.id().to_string());
        }
        if let Some(status) = child.try_status()?
            && status.code() != Some(ALREADY_RUNNING_EXIT_CODE)
        {
            bail!(
                "the session server exited ({status}); see {}",
                session_paths.log.display()
            );
        }
        thread::sleep(DAEMON_POLL_INTERVAL);
    }
    bail!(
        "the session server did not start; see {}",
        session_paths.log.display()
    )
}

pub fn kill(session_paths: &SessionPaths) -> Result<()> {
    send(session_paths, &ClientMessage::Kill)
}

pub fn open(session_paths: &SessionPaths, paths: Vec<PathBuf>) -> Result<()> {
    send(session_paths, &ClientMessage::Open { paths })
}

fn send(session_paths: &SessionPaths, message: &ClientMessage) -> Result<()> {
    let mut stream = UnixStream::connect(&session_paths.socket)
        .with_context(|| format!("session {:?} is not running", session_paths.name))?;
    write_message(&mut stream, message)
}

pub fn list() -> Result<Vec<RunningSession>> {
    let mut sessions = Vec::new();
    let entries = match fs::read_dir(sessions_dir()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(sessions),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(session_paths) = SessionPaths::new(&name) else {
            continue;
        };
        if is_running(&session_paths) {
            let root = read_root(&session_paths);
            sessions.push(RunningSession { name, root });
        }
    }
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::protocol::{FrameDecoder, KeyCode, MouseAction};
    use gpui::Modifiers;
    use gpui_tui::Rgb;

    fn send_hello(stream: &mut UnixStream, version: u32) {
        write_message(
            stream,
            &ClientMessage::Hello {
                version,
                cols: 4,
                rows: 2,
            },
        )
        .unwrap();
    }

    fn next_event(events: &mut UnboundedReceiver<ServerEvent>) -> ServerEvent {
        for _ in 0..200 {
            if let Ok(event) = events.try_recv() {
                return event;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("no server event arrived");
    }

    #[test]
    fn a_silent_client_does_not_block_later_attaches() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("server.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let hub = Arc::new(ClientHub::default());
        let (event_sender, mut events) = unbounded();
        thread::spawn(move || accept_clients(listener, hub, event_sender));

        let _silent = UnixStream::connect(&socket).unwrap();
        let mut attaching = UnixStream::connect(&socket).unwrap();
        send_hello(&mut attaching, PROTOCOL_VERSION);
        assert!(matches!(
            next_event(&mut events),
            ServerEvent::Resized {
                cols: 4,
                rows: 2,
                ..
            }
        ));
    }

    #[test]
    fn a_second_server_for_a_session_reports_that_it_is_already_running() {
        let directory = tempfile::tempdir().unwrap();
        let session_paths = SessionPaths::in_directory("race", directory.path().join("session"));
        let _first = lock_session(&session_paths).unwrap();
        let second = lock_session(&session_paths).unwrap_err();
        assert!(second.is::<AlreadyRunning>(), "{second:#}");
    }

    #[test]
    fn session_names_cannot_leave_the_sessions_directory() {
        for name in ["", ".", "..", "../x", "/tmp/x", "a/b", ".hidden"] {
            assert!(SessionPaths::new(name).is_err(), "{name:?} was accepted");
        }
        for name in ["default", "work-1.2", "my_session"] {
            assert!(SessionPaths::new(name).is_ok(), "{name:?} was rejected");
        }
    }

    #[test]
    fn a_client_with_another_protocol_is_told_why() {
        let hub = Arc::new(ClientHub::default());
        let (event_sender, _events) = unbounded();
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        send_hello(&mut client_side, PROTOCOL_VERSION - 1);
        assert!(serve_client(server_side, &hub, &event_sender).is_err());
        let reply: ServerMessage = read_message(&mut client_side).unwrap();
        assert!(matches!(reply, ServerMessage::Error(error) if error.contains("zed --tui kill")));
    }

    #[test]
    fn session_directories_are_private_even_if_they_existed() {
        let directory = tempfile::tempdir().unwrap();
        let session = directory.path().join("tui").join("default");
        fs::create_dir_all(&session).unwrap();
        fs::set_permissions(&session, fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&session).unwrap();
        let mode = fs::metadata(&session).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn a_kill_from_a_client_that_already_closed_still_arrives() {
        let hub = Arc::new(ClientHub::default());
        let (event_sender, mut events) = unbounded();
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        write_message(&mut client_side, &ClientMessage::Kill).unwrap();
        drop(client_side);
        serve_client(server_side, &hub, &event_sender).unwrap();
        assert!(matches!(next_event(&mut events), ServerEvent::Kill));
    }

    #[test]
    fn paths_sent_to_a_running_session_are_opened() {
        let hub = Arc::new(ClientHub::default());
        let (event_sender, mut events) = unbounded();
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        let paths = vec![PathBuf::from("/tmp/a.rs")];
        write_message(
            &mut client_side,
            &ClientMessage::Open {
                paths: paths.clone(),
            },
        )
        .unwrap();
        serve_client(server_side, &hub, &event_sender).unwrap();
        assert!(matches!(next_event(&mut events), ServerEvent::Open(opened) if opened == paths));
    }

    #[test]
    fn closing_a_client_without_detaching_is_reported() {
        let hub = Arc::new(ClientHub::default());
        let (_, client_side, mut events) = attach(&hub);
        drop(client_side);
        assert!(matches!(
            next_event(&mut events),
            ServerEvent::Disconnected { .. }
        ));
    }

    #[test]
    fn probe_connections_are_not_errors() {
        let hub = Arc::new(ClientHub::default());
        let (event_sender, mut events) = unbounded();
        let (server_side, client_side) = UnixStream::pair().unwrap();
        drop(client_side);
        serve_client(server_side, &hub, &event_sender).unwrap();
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn clients_receive_frames_and_report_input() {
        let hub = Arc::new(ClientHub::default());
        let first = CellGrid::new(4, 2, Rgb::new(0, 0, 0));
        hub.broadcast_frame(Arc::new(first.clone()));

        let (id, client_side, mut events) = attach(&hub);
        let mut client_writer = client_side.try_clone().unwrap();
        let mut client_reader = BufReader::new(client_side);
        hub.send_last_frame_to(id, (4, 2));
        let mut mirrored = None;
        let mut decoder = FrameDecoder;
        let message: ServerMessage = read_message(&mut client_reader).unwrap();
        assert!(matches!(message, ServerMessage::FullFrame(..)));
        decoder.apply(&mut mirrored, &message);

        let mut second = first;
        if let Some(cell) = second.cell_mut(1, 1) {
            cell.glyph = 'x'.into();
        }
        hub.broadcast_frame(Arc::new(second.clone()));
        let message: ServerMessage = read_message(&mut client_reader).unwrap();
        assert!(matches!(message, ServerMessage::Diff(..)));
        decoder.apply(&mut mirrored, &message);
        assert_eq!(mirrored.as_ref(), Some(&second));

        write_message(
            &mut client_writer,
            &ClientMessage::Resize { cols: 3, rows: 1 },
        )
        .unwrap();
        assert!(matches!(
            next_event(&mut events),
            ServerEvent::Resized {
                cols: 3,
                rows: 1,
                ..
            }
        ));

        let key = TermEvent::Key {
            code: KeyCode::Function(1),
            modifiers: Modifiers::default(),
        };
        write_message(&mut client_writer, &ClientMessage::Input(key.clone())).unwrap();
        assert!(matches!(next_event(&mut events), ServerEvent::Input(event) if event == key));

        write_message(&mut client_writer, &ClientMessage::Detach).unwrap();
        assert!(matches!(
            next_event(&mut events),
            ServerEvent::Disconnected { .. }
        ));
    }

    fn attach(hub: &Arc<ClientHub>) -> (u64, UnixStream, UnboundedReceiver<ServerEvent>) {
        let (event_sender, mut events) = unbounded();
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        send_hello(&mut client_side, PROTOCOL_VERSION);
        serve_client(server_side, hub, &event_sender).unwrap();
        let ServerEvent::Resized { id, .. } = next_event(&mut events) else {
            panic!("the client did not report its size");
        };
        (id, client_side, events)
    }

    fn frame_size(message: &ServerMessage) -> Option<(u16, u16)> {
        match message {
            ServerMessage::FullFrame(cols, rows, ..) => Some((*cols, *rows)),
            _ => None,
        }
    }

    #[test]
    fn frames_of_another_size_are_not_sent_on_attach() {
        let hub = Arc::new(ClientHub::default());
        hub.broadcast_frame(Arc::new(CellGrid::new(120, 40, Rgb::new(0, 0, 0))));
        let (id, client_side, _events) = attach(&hub);
        let mut client_reader = BufReader::new(client_side);

        hub.send_last_frame_to(id, (4, 2));
        hub.broadcast_frame(Arc::new(CellGrid::new(4, 2, Rgb::new(0, 0, 0))));
        let message: ServerMessage = read_message(&mut client_reader).unwrap();
        assert_eq!(frame_size(&message), Some((4, 2)));
    }

    #[test]
    fn last_frames_reach_a_client_once_the_shared_size_matches() {
        let hub = Arc::new(ClientHub::default());
        hub.broadcast_frame(Arc::new(CellGrid::new(3, 1, Rgb::new(0, 0, 0))));
        let (id, client_side, _events) = attach(&hub);
        let mut client_reader = BufReader::new(client_side);

        hub.send_last_frame_to(id, (3, 1));
        let message: ServerMessage = read_message(&mut client_reader).unwrap();
        assert_eq!(frame_size(&message), Some((3, 1)));
    }

    #[test]
    fn frames_wait_for_the_client_to_render_but_other_messages_do_not() {
        let hub = Arc::new(ClientHub::default());
        let (_, client_side, _events) = attach(&hub);
        let mut client_writer = client_side.try_clone().unwrap();
        let mut client_reader = BufReader::new(client_side);

        let frame = |ch: char| {
            let mut grid = CellGrid::new(4, 2, Rgb::new(0, 0, 0));
            if let Some(cell) = grid.cell_mut(0, 0) {
                cell.glyph = ch.into();
            }
            Arc::new(grid)
        };
        let mut decoder = FrameDecoder;
        let mut mirrored = None;
        for ch in ['a', 'b'] {
            hub.broadcast_frame(frame(ch));
            let message: ServerMessage = read_message(&mut client_reader).unwrap();
            decoder.apply(&mut mirrored, &message);
        }
        hub.broadcast_frame(frame('c'));
        hub.broadcast_frame(frame('d'));
        hub.broadcast_message(ServerMessage::Title("title".into()));
        let message: ServerMessage = read_message(&mut client_reader).unwrap();
        assert_eq!(message, ServerMessage::Title("title".into()));

        write_message(&mut client_writer, &ClientMessage::Rendered(2)).unwrap();
        let message: ServerMessage = read_message(&mut client_reader).unwrap();
        decoder.apply(&mut mirrored, &message);
        assert_eq!(mirrored.as_ref(), Some(&*frame('d')));
    }

    #[test]
    fn shutting_down_tells_attached_clients_before_returning() {
        let hub = Arc::new(ClientHub::default());
        let (_, client_side, _events) = attach(&hub);

        hub.shut_down();
        assert!(hub.state.lock().clients.is_empty());
        client_side.set_nonblocking(true).unwrap();
        let message: ServerMessage = read_message(&mut BufReader::new(client_side)).unwrap();
        assert_eq!(message, ServerMessage::Shutdown);
    }

    #[gpui::test]
    fn opened_paths_keep_url_metacharacters(cx: &mut gpui::TestAppContext) {
        let paths = [
            "/tmp/x/percent%41.txt",
            "/tmp/x/100%.txt",
            "/tmp/x/with space.txt",
            "/tmp/x/hash#1.txt",
            "/tmp/x/what?.txt",
            "/tmp/x/한글.txt",
        ];
        let urls = file_urls(&paths.map(PathBuf::from));
        let request = cx.update(|cx| {
            crate::zed::OpenRequest::parse(
                crate::zed::RawOpenRequest {
                    urls,
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });
        assert_eq!(request.open_paths, paths);
    }

    fn canonical(path: &Path) -> PathBuf {
        path.canonicalize().unwrap()
    }

    #[test]
    fn session_roots_are_the_enclosing_git_repository() {
        let directory = tempfile::tempdir().unwrap();
        let repo = canonical(directory.path()).join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("src/nested")).unwrap();
        fs::write(repo.join("src/nested/a.rs"), "").unwrap();
        let worktree = canonical(directory.path()).join("worktree");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(worktree.join(".git"), "gitdir: elsewhere").unwrap();
        let plain = canonical(directory.path()).join("plain");
        fs::create_dir_all(&plain).unwrap();
        fs::write(plain.join("notes.txt"), "").unwrap();

        assert_eq!(session_root_for(&repo.join("src/nested/a.rs")), repo);
        assert_eq!(session_root_for(&repo.join("src/nested")), repo);
        assert_eq!(session_root_for(&repo.join(".git/COMMIT_EDITMSG")), repo);
        assert_eq!(session_root_for(&worktree), worktree);
        assert_eq!(session_root_for(&plain.join("notes.txt")), plain);
        assert_eq!(session_root_for(&plain), plain);
    }

    #[test]
    fn targets_resolve_symlinks_and_missing_files_through_their_parent() {
        let directory = tempfile::tempdir().unwrap();
        let real = canonical(directory.path()).join("real");
        fs::create_dir_all(&real).unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(canonical_target(Path::new("link"), directory.path()), real);
        assert_eq!(
            canonical_target(Path::new("link/new.txt"), directory.path()),
            real.join("new.txt")
        );
        assert_eq!(
            canonical_target(&link.join("missing/new.txt"), Path::new("/")),
            link.join("missing/new.txt")
        );
    }

    #[test]
    fn session_names_from_roots_are_valid_stable_and_distinct() {
        assert_eq!(
            session_name_for_root(Path::new("/work/zed")),
            "zed-a5a7578b"
        );
        assert_ne!(
            session_name_for_root(Path::new("/work/zed")),
            session_name_for_root(Path::new("/home/zed"))
        );
        for root in ["/", "/work/.hidden", "/work/한글 dir", "/work/a b/c:d"] {
            let name = session_name_for_root(Path::new(root));
            assert!(SessionPaths::new(&name).is_ok(), "{root:?} gave {name:?}");
        }
        assert!(session_name_for_root(Path::new("/")).starts_with("root-"));
        assert!(session_name_for_root(Path::new("/work/.hidden")).starts_with("hidden-"));
    }

    #[test]
    fn the_deepest_running_root_covers_a_path() {
        let session = |name: &str, root: Option<&str>| RunningSession {
            name: name.to_owned(),
            root: root.map(PathBuf::from),
        };
        let sessions = [
            session("unknown", None),
            session("work", Some("/work")),
            session("repo", Some("/work/repo")),
        ];
        let covering = |target: &str| {
            deepest_covering(&sessions, Path::new(target)).map(|session| session.name.as_str())
        };
        assert_eq!(covering("/work/repo/src/a.rs"), Some("repo"));
        assert_eq!(covering("/work/repo"), Some("repo"));
        assert_eq!(covering("/work/repo2/a.rs"), Some("work"));
        assert_eq!(covering("/elsewhere"), None);
    }

    #[test]
    fn session_roots_round_trip_through_the_session_directory() {
        let directory = tempfile::tempdir().unwrap();
        let session_paths = SessionPaths::in_directory("roots", directory.path().to_path_buf());
        assert_eq!(read_root(&session_paths), None);
        let root = Path::new("/work/한글 repo");
        write_root(&session_paths, root).unwrap();
        assert_eq!(read_root(&session_paths).as_deref(), Some(root));
    }

    #[test]
    fn an_open_and_wait_after_hello_reports_the_client_id() {
        let hub = Arc::new(ClientHub::default());
        let (id, mut client_side, mut events) = attach(&hub);
        let paths = vec![PathBuf::from("/repo/a.txt")];
        write_message(
            &mut client_side,
            &ClientMessage::OpenAndWait {
                paths: paths.clone(),
                quit_session: true,
            },
        )
        .unwrap();
        assert!(matches!(
            next_event(&mut events),
            ServerEvent::OpenAndWait { id: waiting, paths: opened, quit_session: true }
                if waiting == id && opened == paths
        ));
    }

    fn wait_sink(hub: &Arc<ClientHub>, id: u64) -> (WaitSink, Arc<AtomicBool>) {
        let delivered = Arc::new(AtomicBool::new(false));
        let sink = WaitSink {
            hub: hub.clone(),
            id,
            errors: Mutex::default(),
            delivered: delivered.clone(),
        };
        (sink, delivered)
    }

    #[test]
    fn a_finished_wait_reaches_only_its_client() {
        let hub = Arc::new(ClientHub::default());
        let (waiting, waiting_side, _waiting_events) = attach(&hub);
        let (_, other_side, _other_events) = attach(&hub);
        let (sink, delivered) = wait_sink(&hub, waiting);

        sink.send(CliResponse::Stderr {
            message: "oops".into(),
        })
        .unwrap();
        sink.send(CliResponse::Exit { status: 0 }).unwrap();
        hub.broadcast_message(ServerMessage::Title("title".into()));

        assert!(delivered.load(Ordering::SeqCst));
        let message: ServerMessage = read_message(&mut BufReader::new(waiting_side)).unwrap();
        assert_eq!(
            message,
            ServerMessage::WaitFinished {
                status: 0,
                errors: vec!["oops".into()],
            }
        );
        let message: ServerMessage = read_message(&mut BufReader::new(other_side)).unwrap();
        assert_eq!(message, ServerMessage::Title("title".into()));
    }

    #[test]
    fn pings_fail_once_the_waiting_client_is_gone() {
        let hub = Arc::new(ClientHub::default());
        let (id, mut client_side, mut events) = attach(&hub);
        let (sink, delivered) = wait_sink(&hub, id);
        sink.send(CliResponse::Ping).unwrap();

        write_message(&mut client_side, &ClientMessage::Detach).unwrap();
        assert!(matches!(
            next_event(&mut events),
            ServerEvent::Disconnected { .. }
        ));
        assert!(sink.send(CliResponse::Ping).is_err());
        assert!(sink.send(CliResponse::Exit { status: 0 }).is_err());
        assert!(!delivered.load(Ordering::SeqCst));
    }

    #[test]
    fn wait_only_clients_receive_no_frames_or_titles() {
        let hub = Arc::new(ClientHub::default());
        hub.state.lock().last_title = Some("title".into());
        let (event_sender, mut events) = unbounded();
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        let (cols, rows) = WAIT_ONLY_SIZE;
        write_message(
            &mut client_side,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                cols,
                rows,
            },
        )
        .unwrap();
        serve_client(server_side, &hub, &event_sender).unwrap();
        write_message(
            &mut client_side,
            &ClientMessage::OpenAndWait {
                paths: Vec::new(),
                quit_session: false,
            },
        )
        .unwrap();
        let ServerEvent::OpenAndWait { id, .. } = next_event(&mut events) else {
            panic!("a wait-only client must not report a size");
        };

        hub.broadcast_frame(Arc::new(CellGrid::new(4, 2, Rgb::default())));
        assert!(hub.send_message_to(id, ServerMessage::Shutdown));
        let message: ServerMessage = read_message(&mut BufReader::new(client_side)).unwrap();
        assert_eq!(message, ServerMessage::Shutdown);
    }

    fn move_mouse(writer: &mut UnixStream, events: &mut UnboundedReceiver<ServerEvent>) {
        let mouse = TermEvent::Mouse {
            action: MouseAction::Moved,
            col: 1,
            row: 1,
            modifiers: Modifiers::default(),
        };
        write_message(writer, &ClientMessage::Input(mouse)).unwrap();
        assert!(matches!(
            next_event(events),
            ServerEvent::Input(TermEvent::Mouse { .. })
        ));
    }

    #[test]
    fn pointer_shapes_reach_only_the_client_that_moved_the_mouse() {
        let hub = Arc::new(ClientHub::default());
        let (first, first_side, mut first_events) = attach(&hub);
        let (second, second_side, mut second_events) = attach(&hub);
        let mut first_writer = first_side.try_clone().unwrap();
        let mut second_writer = second_side.try_clone().unwrap();
        let mut first_reader = BufReader::new(first_side);
        let mut second_reader = BufReader::new(second_side);
        let next =
            |reader: &mut BufReader<UnixStream>| -> ServerMessage { read_message(reader).unwrap() };
        let marker = |text: &str| {
            hub.broadcast_message(ServerMessage::Title(text.into()));
            ServerMessage::Title(text.into())
        };

        hub.set_pointer(CursorStyle::IBeam);
        let unowned = marker("unowned");
        assert_eq!(next(&mut first_reader), unowned);
        assert_eq!(next(&mut second_reader), unowned);

        move_mouse(&mut first_writer, &mut first_events);
        assert_eq!(hub.state.lock().pointer_owner, Some(first));
        assert_eq!(
            next(&mut first_reader),
            ServerMessage::Pointer(CursorStyle::IBeam)
        );
        hub.set_pointer(CursorStyle::PointingHand);
        assert_eq!(
            next(&mut first_reader),
            ServerMessage::Pointer(CursorStyle::PointingHand)
        );
        move_mouse(&mut first_writer, &mut first_events);
        let owned_by_first = marker("owned by first");
        assert_eq!(next(&mut first_reader), owned_by_first);
        assert_eq!(next(&mut second_reader), owned_by_first);

        move_mouse(&mut second_writer, &mut second_events);
        assert_eq!(hub.state.lock().pointer_owner, Some(second));
        assert_eq!(
            next(&mut second_reader),
            ServerMessage::Pointer(CursorStyle::PointingHand)
        );
        hub.set_pointer(CursorStyle::Arrow);
        assert_eq!(
            next(&mut second_reader),
            ServerMessage::Pointer(CursorStyle::Arrow)
        );

        write_message(&mut second_writer, &ClientMessage::Detach).unwrap();
        assert!(matches!(
            next_event(&mut second_events),
            ServerEvent::Disconnected { .. }
        ));
        assert_eq!(hub.state.lock().pointer_owner, None);
        hub.set_pointer(CursorStyle::IBeam);
        let detached = marker("detached");
        assert_eq!(next(&mut first_reader), detached);

        move_mouse(&mut first_writer, &mut first_events);
        assert_eq!(
            next(&mut first_reader),
            ServerMessage::Pointer(CursorStyle::IBeam)
        );
    }
}
