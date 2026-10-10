#[cfg(unix)]
mod client;
#[cfg(unix)]
mod input;
#[cfg(unix)]
mod protocol;
#[cfg(unix)]
mod server;
#[cfg(all(unix, test))]
mod test_support;

#[cfg(unix)]
pub use unix::*;

#[cfg(not(unix))]
pub use stubs::*;

#[cfg(not(unix))]
mod stubs {
    use gpui::{App, Application};

    pub enum TuiServer {}

    pub enum Startup {}

    impl TuiServer {
        pub fn zed_args(&self) -> Vec<std::ffi::OsString> {
            match *self {}
        }
    }

    impl Startup {
        pub fn application(&self) -> Application {
            match *self {}
        }

        pub fn init_settings(&self, _: &mut App) {
            match *self {}
        }

        pub fn database(&self) -> db::AppDatabase {
            match *self {}
        }

        pub fn init(self, _: &mut App) {
            match self {}
        }
    }

    pub fn start() -> Option<(TuiServer, Startup)> {
        None
    }
}

#[cfg(unix)]
mod unix {
    use std::{ffi::OsString, io::Write as _, path::PathBuf, rc::Rc};

    use anyhow::{Context as _, Result};
    use clap::{Parser, Subcommand};
    use gpui::{App, Application};
    use gpui_tui::TuiPlatform;

    use super::{client, server};

    const DEFAULT_COLS: u16 = 120;
    const DEFAULT_ROWS: u16 = 40;
    const SESSION: &str = "default";

    #[derive(Parser)]
    #[command(name = "zed --tui", about = "Zed in the terminal")]
    struct Cli {
        #[command(subcommand)]
        command: Option<Command>,
        paths: Vec<PathBuf>,
    }

    #[derive(Subcommand)]
    enum Command {
        #[command(about = "Attach to a running session")]
        Attach,
        #[command(about = "Run the session server in the foreground")]
        Server { paths: Vec<PathBuf> },
    }

    pub struct TuiServer {
        _session: server::SessionGuard,
        paths: Vec<PathBuf>,
    }

    impl TuiServer {
        pub fn zed_args(&self) -> Vec<OsString> {
            std::iter::once(OsString::from("zed"))
                .chain(self.paths.iter().map(|path| path.clone().into_os_string()))
                .collect()
        }
    }

    pub struct Startup {
        platform: Rc<TuiPlatform>,
        started: server::Started,
    }

    pub fn start() -> Option<(TuiServer, Startup)> {
        let mut args = std::env::args_os();
        let program = args.next()?;
        if args.next()? != "--tui" {
            return None;
        }
        match run(Cli::parse_from(std::iter::once(program).chain(args))) {
            Ok(Outcome::Serve(server, startup)) => Some((server, startup)),
            Ok(Outcome::Exit(code)) => std::process::exit(code),
            Err(error) => {
                eprintln!("zed --tui: {error:#}");
                std::process::exit(1);
            }
        }
    }

    enum Outcome {
        Exit(i32),
        Serve(TuiServer, Startup),
    }

    fn run(cli: Cli) -> Result<Outcome> {
        let session_paths = server::SessionPaths::new(SESSION)?;
        match cli.command {
            None => open(cli.paths, &session_paths),
            Some(Command::Attach) => attach(&session_paths),
            Some(Command::Server { paths }) => {
                let platform = TuiPlatform::new(DEFAULT_COLS, DEFAULT_ROWS);
                let (session, started) = server::start_session(session_paths, platform.clone())?;
                Ok(Outcome::Serve(
                    TuiServer {
                        _session: session,
                        paths: absolute_paths(paths)?,
                    },
                    Startup { platform, started },
                ))
            }
        }
    }

    fn open(paths: Vec<PathBuf>, session_paths: &server::SessionPaths) -> Result<Outcome> {
        if !server::is_running(session_paths) {
            server::spawn_daemon(session_paths, &absolute_paths(paths)?)?;
        }
        attach(session_paths)
    }

    fn describe(session: &str, exit: client::Exit) -> Result<String> {
        Ok(match exit {
            client::Exit::ServerShutdown => format!("session {session:?} has ended"),
            client::Exit::Disconnected => format!("lost connection to session {session:?}"),
            client::Exit::Rejected(error) => anyhow::bail!(error),
        })
    }

    fn attach(session_paths: &server::SessionPaths) -> Result<Outcome> {
        let exit = client::attach(&session_paths.socket)?;
        let message = describe(&session_paths.name, exit)?;
        writeln!(std::io::stdout(), "{message}").ok();
        Ok(Outcome::Exit(0))
    }

    fn absolute_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
        let current_dir = std::env::current_dir().context("reading the current directory")?;
        if paths.is_empty() {
            return Ok(vec![current_dir]);
        }
        Ok(paths
            .into_iter()
            .map(|path| {
                let path = current_dir.join(path);
                path.canonicalize().unwrap_or(path)
            })
            .collect())
    }

    impl Startup {
        pub fn application(&self) -> Application {
            Application::with_platform(self.platform.clone())
        }

        pub fn init_settings(&self, _: &mut App) {}

        pub fn database(&self) -> db::AppDatabase {
            db::AppDatabase::new()
        }

        pub fn init(self, cx: &mut App) {
            let Self {
                platform,
                started:
                    server::Started {
                        on_frame,
                        after_start,
                    },
            } = self;

            platform.set_frame_sink(on_frame);

            after_start(cx);
        }
    }
}
