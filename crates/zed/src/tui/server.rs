use std::{
    fs,
    io::{BufReader, ErrorKind, Write as _},
    net::Shutdown,
    os::unix::{
        fs::{DirBuilderExt as _, PermissionsExt as _},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use collections::HashMap;
use futures::{
    StreamExt as _,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
};
use gpui::App;
use gpui_tui::{CellGrid, TuiPlatform};
use parking_lot::Mutex;
use util::ResultExt as _;

use crate::tui::{
    input::InputTranslator,
    protocol::{
        ClientMessage, FrameEncoder, PROTOCOL_VERSION, ServerMessage, TermEvent, read_message,
        write_message,
    },
};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(30);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub struct SessionPaths {
    pub name: String,
    pub directory: PathBuf,
    pub socket: PathBuf,
    pub pid: PathBuf,
    pub log: PathBuf,
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
            directory,
        }
    }
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
}

struct ClientHandle {
    id: u64,
    sender: mpsc::Sender<Outgoing>,
}

#[derive(Default)]
struct HubState {
    clients: Vec<ClientHandle>,
    last_frame: Option<Arc<CellGrid>>,
}

impl HubState {
    fn client(&self, id: u64) -> Option<&ClientHandle> {
        self.clients.iter().find(|client| client.id == id)
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
        state
            .clients
            .retain(|client| client.sender.send(Outgoing::Frame(grid.clone())).is_ok());
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

    fn remove(&self, id: u64) {
        self.state.lock().clients.retain(|client| client.id != id);
    }
}

enum ServerEvent {
    Resized { id: u64, cols: u16, rows: u16 },
    Disconnected { id: u64 },
    Input(TermEvent),
}

pub struct Started {
    pub on_frame: Box<dyn FnMut(CellGrid)>,
    pub after_start: Box<dyn FnOnce(&mut App)>,
}

pub struct SessionGuard {
    session_paths: SessionPaths,
    _pid_lock: fs::File,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        fs::remove_file(&self.session_paths.socket).log_err();
        fs::remove_file(&self.session_paths.pid).log_err();
    }
}

fn lock_session(session_paths: &SessionPaths) -> Result<fs::File> {
    create_private_dir(&session_paths.directory)?;
    if is_running(session_paths) {
        bail!("session {:?} is already running", session_paths.name);
    }
    let mut pid_lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&session_paths.pid)
        .with_context(|| format!("opening {}", session_paths.pid.display()))?;
    pid_lock.set_len(0)?;
    write!(pid_lock, "{}", std::process::id())?;
    Ok(pid_lock)
}

pub fn start_session(
    session_paths: SessionPaths,
    platform: Rc<TuiPlatform>,
) -> Result<(SessionGuard, Started)> {
    let pid_lock = lock_session(&session_paths)?;
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

    let started = Started {
        on_frame: Box::new({
            let hub = hub.clone();
            move |grid| hub.broadcast_frame(Arc::new(grid))
        }),
        after_start: Box::new(move |cx| {
            cx.spawn(async move |_| handle_events(platform, hub, event_receiver).await)
                .detach();
        }),
    };
    let session = SessionGuard {
        session_paths,
        _pid_lock: pid_lock,
    };
    Ok((session, started))
}

async fn handle_events(
    platform: Rc<TuiPlatform>,
    hub: Arc<ClientHub>,
    mut events: UnboundedReceiver<ServerEvent>,
) {
    let mut sizes: HashMap<u64, (u16, u16)> = HashMap::default();
    let mut translator = InputTranslator::default();

    while let Some(event) = events.next().await {
        handle_event(event, &platform, &hub, &mut sizes, &mut translator);
    }
}

fn handle_event(
    event: ServerEvent,
    platform: &TuiPlatform,
    hub: &Arc<ClientHub>,
    sizes: &mut HashMap<u64, (u16, u16)>,
    translator: &mut InputTranslator,
) {
    match event {
        ServerEvent::Resized { id, cols, rows } => {
            let attached = sizes.insert(id, (cols, rows)).is_none();
            if let Some(shared) = apply_shared_size(platform, sizes)
                && attached
            {
                hub.send_last_frame_to(id, shared);
            }
        }
        ServerEvent::Disconnected { id } => {
            sizes.remove(&id);
            apply_shared_size(platform, sizes);
        }
        ServerEvent::Input(event) => {
            for input in translator.translate(event) {
                platform.handle_input(input);
            }
        }
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
                     stop the session server and start it again"
                );
                write_message(&mut &write_stream, &ServerMessage::Error(error)).log_err();
                bail!("client speaks protocol {version}, expected {PROTOCOL_VERSION}");
            }
            (cols, rows)
        }
        other => bail!("expected a hello message, got {other:?}"),
    };

    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name(format!("ClientWriter-{id}"))
        .spawn(move || write_to_client(write_stream, receiver))?;
    hub.state.lock().clients.push(ClientHandle { id, sender });

    events
        .unbounded_send(ServerEvent::Resized { id, cols, rows })
        .log_err();
    log::info!("client {id} attached at {cols}x{rows}");

    let hub = hub.clone();
    let events = events.clone();
    thread::Builder::new()
        .name(format!("ClientReader-{id}"))
        .spawn(move || {
            let detached = read_from_client(id, reader, &events);
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
    events: &UnboundedSender<ServerEvent>,
) -> bool {
    loop {
        let event = match read_message(&mut reader) {
            Ok(ClientMessage::Input(event)) => ServerEvent::Input(event),
            Ok(ClientMessage::Resize { cols, rows }) => ServerEvent::Resized { id, cols, rows },
            Ok(ClientMessage::Detach) => return true,
            Ok(ClientMessage::Hello { .. }) => continue,
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
    send_to_client(&mut stream, receiver);
    stream.shutdown(Shutdown::Both).ok();
}

fn send_to_client(stream: &mut UnixStream, receiver: mpsc::Receiver<Outgoing>) {
    let encoder = FrameEncoder;
    while let Ok(outgoing) = receiver.recv() {
        let Outgoing::Frame(grid) = outgoing;
        if write_message(stream, &encoder.full_frame(&grid)).is_err() {
            return;
        }
    }
}

pub fn spawn_daemon(session_paths: &SessionPaths, paths: &[PathBuf]) -> Result<()> {
    create_private_dir(&session_paths.directory)?;
    let log = fs::File::create(&session_paths.log)
        .with_context(|| format!("creating {}", session_paths.log.display()))?;
    let mut command = util::command::new_std_command(std::env::current_exe()?);
    command.arg("--tui").arg("server").args(paths);
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
            return Ok(());
        }
        if let Some(status) = child.try_status()? {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::protocol::{FrameDecoder, KeyCode};
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
        assert!(matches!(reply, ServerMessage::Error(error) if error.contains("protocol")));
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
        assert!(matches!(message, ServerMessage::FullFrame(..)));
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
}
