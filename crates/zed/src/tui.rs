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
mod title_bar;

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
    use std::{
        cell::RefCell,
        ffi::OsString,
        io::Write as _,
        path::{Path, PathBuf},
        rc::Rc,
    };

    use anyhow::{Context as _, Result};
    use clap::{Parser, Subcommand};
    use command_palette_hooks::CommandPaletteFilter;
    use gpui::{App, AppContext as _, Application, UpdateGlobal as _};
    use gpui_tui::{CellGrid, Rgb, TuiPlatform};
    use settings::{
        ActiveSettingsProfileName, MergeFromTrait as _, RootUserSettings as _, SettingsAssets,
        SettingsContent, SettingsStore,
    };
    use theme::{ActiveTheme as _, GlobalTheme, ThemeRegistry};
    use util::{ResultExt as _, asset_str};
    use workspace::Workspace;

    use super::{client, server, title_bar::TerminalTitleBar};

    const DEFAULT_COLS: u16 = 120;
    const DEFAULT_ROWS: u16 = 40;

    const TUI_DEFAULTS_PATH: &str = "settings/tui_defaults.json";
    const TUI_CONSTRAINTS_PATH: &str = "settings/tui_constraints.json";
    const SETTINGS_PROFILE: &str = "tui";

    #[derive(Parser)]
    #[command(name = "zed --tui", about = "Zed in the terminal")]
    struct Cli {
        #[arg(
            long,
            global = true,
            help = "Session name; by default the session whose root covers the path"
        )]
        session: Option<String>,
        #[arg(long, global = true)]
        user_data_dir: Option<PathBuf>,
        #[command(subcommand)]
        command: Option<Command>,
        paths: Vec<PathBuf>,
    }

    #[derive(Subcommand)]
    enum Command {
        #[command(about = "Attach to a running session")]
        Attach { path: Option<PathBuf> },
        #[command(about = "Run the session server in the foreground")]
        Server {
            #[arg(long, hide = true)]
            root: Option<PathBuf>,
            paths: Vec<PathBuf>,
        },
        #[command(about = "Stop a session")]
        Kill { path: Option<PathBuf> },
        #[command(about = "List running sessions")]
        Ls,
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

    struct Resolved {
        paths: server::SessionPaths,
        root: PathBuf,
    }

    fn resolve(session: Option<&str>, target: &Path) -> Result<Resolved> {
        let root = server::session_root_for(target);
        let name = match session {
            Some(session) => session.to_owned(),
            None => {
                let sessions = server::list()?;
                match server::deepest_covering(&sessions, target) {
                    Some(covering) => covering.name.clone(),
                    None => {
                        let name = server::session_name_for_root(&root);
                        if let Some(other) = sessions.iter().find(|session| session.name == name) {
                            anyhow::bail!(
                                "session {name:?} already serves {}; pass --session to pick another name",
                                other
                                    .root
                                    .as_deref()
                                    .map_or("another root".into(), |root| root
                                        .display()
                                        .to_string())
                            );
                        }
                        name
                    }
                }
            }
        };
        let paths = server::SessionPaths::new(&name)?;
        Ok(Resolved { paths, root })
    }

    fn find_running(
        session: Option<&str>,
        path: Option<PathBuf>,
        allow_only: bool,
    ) -> Result<String> {
        if let Some(session) = session {
            return Ok(session.to_owned());
        }
        let sessions = server::list()?;
        if allow_only && let [only] = sessions.as_slice() {
            return Ok(only.name.clone());
        }
        let current_dir = std::env::current_dir().context("reading the current directory")?;
        let target = server::canonical_target(&path.unwrap_or_default(), &current_dir);
        if let Some(covering) = server::deepest_covering(&sessions, &target) {
            return Ok(covering.name.clone());
        }
        let names = sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect::<Vec<_>>();
        if names.is_empty() {
            anyhow::bail!("no session is running");
        }
        anyhow::bail!(
            "no running session covers {}; running sessions: {}",
            target.display(),
            names.join(", ")
        )
    }

    fn run(cli: Cli) -> Result<Outcome> {
        if let Some(user_data_dir) = &cli.user_data_dir {
            paths::set_custom_data_dir(&user_data_dir.to_string_lossy());
        }
        match cli.command {
            None => open(cli),
            Some(Command::Attach { path }) => {
                let name = find_running(cli.session.as_deref(), path, true)?;
                attach(&server::SessionPaths::new(&name)?)
            }
            Some(Command::Kill { path }) => {
                let name = find_running(cli.session.as_deref(), path, false)?;
                server::kill(&server::SessionPaths::new(&name)?)?;
                Ok(Outcome::Exit(0))
            }
            Some(Command::Ls) => {
                for session in server::list()? {
                    let root = session
                        .root
                        .map_or("-".into(), |root| root.display().to_string());
                    println!("{}\t{root}", session.name);
                }
                Ok(Outcome::Exit(0))
            }
            Some(Command::Server { root, paths }) => {
                let name = cli.session.unwrap_or_else(|| "default".to_owned());
                let session_paths = server::SessionPaths::new(&name)?;
                let platform = TuiPlatform::new(DEFAULT_COLS, DEFAULT_ROWS);
                let root = root.map(|root| root.canonicalize().unwrap_or(root));
                let (session, started) =
                    server::start_session(session_paths, root.as_deref(), platform.clone())?;
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

    fn open(cli: Cli) -> Result<Outcome> {
        let current_dir = std::env::current_dir().context("reading the current directory")?;
        let targets = cli
            .paths
            .iter()
            .map(|path| server::canonical_target(path, &current_dir))
            .collect::<Vec<_>>();
        let first_target = targets.first().cloned().unwrap_or(current_dir);
        let session = resolve(cli.session.as_deref(), &first_target)?;

        if !server::is_running(&session.paths) {
            let extra_targets = targets
                .iter()
                .filter(|target| **target != session.root)
                .cloned()
                .collect::<Vec<_>>();
            server::spawn_daemon(
                &session.paths,
                &session.root,
                &extra_targets,
                cli.user_data_dir.as_deref(),
            )?;
        }
        attach(&session.paths)
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

    #[derive(Default)]
    struct DefaultColors {
        canvas: Rgb,
        backgrounds: [Rgb; 2],
        foregrounds: [Rgb; 2],
    }

    impl DefaultColors {
        fn of_theme(cx: &App) -> Self {
            let theme = cx.theme();
            let canvas = ThemeRegistry::global(cx)
                .get(&theme.name)
                .log_err()
                .map_or(Rgb::default(), |original| {
                    Rgb::default().blend(original.colors().editor_background)
                });
            let colors = theme.colors();
            let painted = |color: gpui::Hsla| {
                if color.a > 0. {
                    canvas.blend(color)
                } else {
                    Rgb::default()
                }
            };
            Self {
                canvas,
                backgrounds: [colors.editor_background, colors.panel_background].map(painted),
                foregrounds: [colors.editor_foreground, colors.text]
                    .map(|color| Rgb::default().blend(color)),
            }
        }
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
                        mut on_frame,
                        after_start,
                    },
            } = self;

            CommandPaletteFilter::update_global(cx, |filter, _| {
                filter.hide_action_types(&crate::zed::font_size_actions());
            });

            let default_colors: Rc<RefCell<DefaultColors>> = Rc::default();
            platform.set_frame_sink({
                let default_colors = default_colors.clone();
                move |mut grid: CellGrid| {
                    let default_colors = default_colors.borrow();
                    grid.mark_default_colors(
                        &default_colors.backgrounds,
                        &default_colors.foregrounds,
                    );
                    on_frame(grid)
                }
            });
            let update_default_colors = move |cx: &mut App| {
                let colors = DefaultColors::of_theme(cx);
                platform.set_canvas(colors.canvas);
                *default_colors.borrow_mut() = colors;
            };
            update_default_colors(cx);
            cx.observe_global::<GlobalTheme>(update_default_colors)
                .detach();

            cx.observe_new(|workspace: &mut Workspace, window, cx| {
                let Some(window) = window else {
                    return;
                };
                let workspace_handle = cx.entity();
                let title_bar = cx.new(|cx| TerminalTitleBar::new(workspace, workspace_handle, cx));
                workspace.set_titlebar_item(title_bar.into(), window, cx);
            })
            .detach();

            after_start(cx);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use settings::{
            FontSize, ReduceMotionMode, SaturatingBool, ThemeColor, ThemeName, ThemeSelection,
        };

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
        fn base_backgrounds_are_transparent_by_default_and_overridable(
            cx: &mut gpui::TestAppContext,
        ) {
            cx.update(|cx| {
                let mut store = SettingsStore::test(cx);
                apply_terminal_settings(&mut store, cx).unwrap();
                let transparent = Some(ThemeColor::from("#00000000"));
                let colors = |store: &SettingsStore| {
                    store
                        .merged_settings()
                        .theme
                        .experimental_theme_overrides
                        .clone()
                        .unwrap()
                        .colors
                };

                let defaults = colors(&store);
                assert_eq!(defaults.background, transparent);
                assert_eq!(defaults.editor_background, transparent);
                assert_eq!(defaults.surface_background, transparent);
                assert_eq!(defaults.tab_active_background, None);

                store
                    .set_user_settings(
                        r##"{ "experimental.theme_overrides": { "editor.background": "#282c33ff" } }"##,
                        cx,
                    )
                    .result()
                    .unwrap();
                let overridden = colors(&store);
                assert_eq!(
                    overridden.editor_background,
                    Some(ThemeColor::from("#282c33ff"))
                );
                assert_eq!(overridden.background, transparent);
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
