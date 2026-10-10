use std::{ffi::OsString, path::PathBuf};

use anyhow::Result;

#[derive(Debug, PartialEq)]
pub struct Launch {
    zed: Option<PathBuf>,
    arguments: Vec<OsString>,
}

pub fn launch(mut args: impl Iterator<Item = OsString>) -> Option<Launch> {
    args.next()?;
    let mut zed = None;
    let mut first = args.next()?;
    if first == "--zed" {
        zed = Some(PathBuf::from(args.next()?));
        first = args.next()?;
    } else if let Some(path) = first.to_str().and_then(|arg| arg.strip_prefix("--zed=")) {
        zed = Some(PathBuf::from(path));
        first = args.next()?;
    }
    (first == "--tui").then(|| Launch {
        zed,
        arguments: std::iter::once(first).chain(args).collect(),
    })
}

#[cfg(unix)]
pub fn run(launch: Launch) -> Result<()> {
    use crate::{Detect, InstalledApp as _};
    use anyhow::Context as _;
    use std::os::unix::process::CommandExt as _;

    #[cfg(target_os = "linux")]
    let zed = launch.zed.or_else(crate::flatpak::bin_if_no_escape);
    #[cfg(not(target_os = "linux"))]
    let zed = launch.zed;
    let app = Detect::detect(zed.as_deref()).context("Bundle detection")?;
    let error = std::process::Command::new(app.path())
        .args(launch.arguments)
        .exec();
    Err(error).with_context(|| format!("running {}", app.path().display()))
}

#[cfg(not(unix))]
pub fn run(_launch: Launch) -> Result<()> {
    anyhow::bail!("--tui is only supported on Unix")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_arguments_are_forwarded_when_tui_comes_first_or_after_zed() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        let launch = |list: &[&str]| launch(args(list).into_iter());
        assert_eq!(
            launch(&["zed", "--tui", "ls"]),
            Some(Launch {
                zed: None,
                arguments: args(&["--tui", "ls"]),
            })
        );
        assert_eq!(
            launch(&["zed", "--zed", "/app/zed", "--tui", "ls"]),
            Some(Launch {
                zed: Some(PathBuf::from("/app/zed")),
                arguments: args(&["--tui", "ls"]),
            })
        );
        assert_eq!(
            launch(&["zed", "--zed=/app/zed", "--tui"]),
            Some(Launch {
                zed: Some(PathBuf::from("/app/zed")),
                arguments: args(&["--tui"]),
            })
        );
        assert_eq!(
            launch(&["zed", "--tui", "--wait", ".git/COMMIT_EDITMSG"]),
            Some(Launch {
                zed: None,
                arguments: args(&["--tui", "--wait", ".git/COMMIT_EDITMSG"]),
            })
        );
        assert_eq!(launch(&["zed", "--zed", "/app/zed", "file"]), None);
        assert_eq!(launch(&["zed", "file", "--tui"]), None);
        assert_eq!(launch(&["zed"]), None);
    }
}
