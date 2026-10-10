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
    use command_palette_hooks::CommandPaletteFilter;
    use gpui::{App, Application, UpdateGlobal as _};
    use gpui_tui::TuiPlatform;
    use settings::{
        ActiveSettingsProfileName, MergeFromTrait as _, RootUserSettings as _, SettingsAssets,
        SettingsContent, SettingsStore,
    };
    use util::{ResultExt as _, asset_str};

    use super::{client, server};

    const DEFAULT_COLS: u16 = 120;
    const DEFAULT_ROWS: u16 = 40;
    const SESSION: &str = "default";

    const TUI_DEFAULTS_PATH: &str = "settings/tui_defaults.json";
    const TUI_CONSTRAINTS_PATH: &str = "settings/tui_constraints.json";
    const SETTINGS_PROFILE: &str = "tui";

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
            client::Exit::Detached => format!("detached from session {session:?}"),
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

    fn apply_terminal_settings(store: &mut SettingsStore, cx: &mut App) -> Result<()> {
        let defaults = SettingsContent::parse_json_with_comments(&asset_str::<SettingsAssets>(
            TUI_DEFAULTS_PATH,
        ))
        .context("parsing the terminal defaults")?;
        store.update_default_settings(cx, |content| content.merge_from(&defaults));
        store
            .set_server_settings(&asset_str::<SettingsAssets>(TUI_CONSTRAINTS_PATH), cx)
            .context("applying the terminal constraints")
    }

    fn activate_settings_profile(cx: &mut App) {
        cx.set_global(ActiveSettingsProfileName(SETTINGS_PROFILE.into()));
    }

    impl Startup {
        pub fn application(&self) -> Application {
            Application::with_platform(self.platform.clone())
        }

        pub fn init_settings(&self, cx: &mut App) {
            SettingsStore::update_global(cx, apply_terminal_settings).log_err();
            activate_settings_profile(cx);
        }

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

            CommandPaletteFilter::update_global(cx, |filter, _| {
                filter.hide_action_types(&crate::zed::font_size_actions());
            });

            platform.set_frame_sink(on_frame);

            after_start(cx);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use settings::{FontSize, ReduceMotionMode, SaturatingBool, ThemeName, ThemeSelection};

        #[gpui::test]
        fn user_settings_override_defaults_but_not_constraints(cx: &mut gpui::TestAppContext) {
            cx.update(|cx| {
                let mut store = SettingsStore::test(cx);
                apply_terminal_settings(&mut store, cx).unwrap();
                assert_eq!(
                    store.merged_settings().reduce_motion,
                    Some(ReduceMotionMode::On)
                );
                store
                    .set_user_settings(
                        r#"{
                            "theme": "Gruvbox Dark",
                            "show_edit_predictions": true,
                            "buffer_font_size": 20,
                            "disable_ai": false
                        }"#,
                        cx,
                    )
                    .result()
                    .unwrap();
                let merged = store.merged_settings();
                assert_eq!(
                    merged.theme.theme,
                    Some(ThemeSelection::Static(ThemeName("Gruvbox Dark".into())))
                );
                assert_eq!(
                    merged.project.all_languages.defaults.show_edit_predictions,
                    Some(true)
                );
                assert_eq!(merged.theme.buffer_font_size, Some(FontSize(16.)));
                assert_eq!(merged.project.disable_ai, Some(SaturatingBool(false)));
            });
        }

        #[gpui::test]
        fn agent_and_commit_editors_use_the_buffer_row_height(cx: &mut gpui::TestAppContext) {
            cx.update(|cx| {
                let mut store = SettingsStore::test(cx);
                apply_terminal_settings(&mut store, cx).unwrap();
                let merged = store.merged_settings();
                assert_eq!(
                    merged.theme.agent_buffer_font_size,
                    merged.theme.buffer_font_size
                );
                assert_eq!(
                    merged.theme.git_commit_buffer_font_size,
                    merged.theme.buffer_font_size
                );
            });
        }

        #[gpui::test]
        fn user_settings_cannot_resize_fonts_or_enable_wheel_zoom(cx: &mut gpui::TestAppContext) {
            cx.update(|cx| {
                let mut store = SettingsStore::test(cx);
                apply_terminal_settings(&mut store, cx).unwrap();
                store
                    .set_user_settings(
                        r#"{
                            "agent_ui_font_size": 20,
                            "agent_buffer_font_size": 20,
                            "git_commit_buffer_font_size": 20,
                            "markdown_preview": { "font_size": 20 },
                            "mouse_wheel_zoom": true
                        }"#,
                        cx,
                    )
                    .result()
                    .unwrap();
                let merged = store.merged_settings();
                assert_eq!(merged.theme.agent_ui_font_size, Some(FontSize(10.)));
                assert_eq!(merged.theme.agent_buffer_font_size, Some(FontSize(16.)));
                assert_eq!(
                    merged.theme.git_commit_buffer_font_size,
                    Some(FontSize(16.))
                );
                assert_eq!(
                    merged
                        .markdown_preview
                        .as_ref()
                        .and_then(|markdown_preview| markdown_preview.font_size),
                    Some(FontSize(10.))
                );
                assert_eq!(merged.editor.mouse_wheel_zoom, Some(false));
            });
        }

        #[gpui::test]
        fn the_tui_profile_applies_only_when_active(cx: &mut gpui::TestAppContext) {
            cx.update(|cx| {
                let user_settings = r#"{
                    "profiles": {
                        "tui": { "settings": { "theme": "Gruvbox Dark", "buffer_font_size": 20 } }
                    }
                }"#;
                let terminal_store = |cx: &mut App| {
                    let mut store = SettingsStore::test(cx);
                    apply_terminal_settings(&mut store, cx).unwrap();
                    store.set_user_settings(user_settings, cx).result().unwrap();
                    store
                };
                let gruvbox = Some(ThemeSelection::Static(ThemeName("Gruvbox Dark".into())));

                let store = terminal_store(cx);
                assert_ne!(store.merged_settings().theme.theme, gruvbox);

                activate_settings_profile(cx);
                let store = terminal_store(cx);
                let merged = store.merged_settings();
                assert_eq!(merged.theme.theme, gruvbox);
                assert_eq!(merged.theme.buffer_font_size, Some(FontSize(16.)));
            });
        }
    }
}
