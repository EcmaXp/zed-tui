use std::sync::OnceLock;

use anyhow::{Context as _, Result};
use util::ResultExt as _;
use windows::Win32::{
    Storage::FileSystem::WriteFile,
    System::Console::{
        CONSOLE_MODE, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_PROCESSED_OUTPUT,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetConsoleOutputCP, GetStdHandle,
        STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler, SetConsoleMode,
        SetConsoleOutputCP,
    },
};

static ORIGINAL_CONSOLE: OnceLock<ConsoleState> = OnceLock::new();

struct ConsoleState {
    input_mode: CONSOLE_MODE,
    output_mode: CONSOLE_MODE,
    output_code_page: u32,
}

impl ConsoleState {
    fn restore(&self) -> Result<()> {
        unsafe {
            let input = SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE)?, self.input_mode);
            let output = SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE)?, self.output_mode);
            let code_page = SetConsoleOutputCP(self.output_code_page);
            input?;
            output?;
            code_page?;
        }
        Ok(())
    }
}

pub(super) struct ConsoleGuard;

impl ConsoleGuard {
    pub(super) fn enter() -> Result<Self> {
        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE)?;
            let output = GetStdHandle(STD_OUTPUT_HANDLE)?;
            let mut input_mode = CONSOLE_MODE::default();
            let mut output_mode = CONSOLE_MODE::default();
            GetConsoleMode(input, &mut input_mode).context("reading console input mode")?;
            GetConsoleMode(output, &mut output_mode).context("reading console output mode")?;
            ORIGINAL_CONSOLE.get_or_init(|| ConsoleState {
                input_mode,
                output_mode,
                output_code_page: GetConsoleOutputCP(),
            });
            let guard = Self;
            SetConsoleMode(
                output,
                output_mode
                    | ENABLE_PROCESSED_OUTPUT
                    | ENABLE_VIRTUAL_TERMINAL_PROCESSING
                    | DISABLE_NEWLINE_AUTO_RETURN,
            )
            .context("enabling virtual terminal output; use Windows Terminal")?;
            SetConsoleOutputCP(65001).context("enabling UTF-8 console output")?;
            SetConsoleCtrlHandler(Some(restore_console), true)
                .context("installing the console cleanup handler")?;
            Ok(guard)
        }
    }
}

impl Drop for ConsoleGuard {
    fn drop(&mut self) {
        unsafe { SetConsoleCtrlHandler(Some(restore_console), false) }
            .context("removing the console cleanup handler")
            .log_err();
        if let Some(original) = ORIGINAL_CONSOLE.get() {
            original
                .restore()
                .context("restoring the console")
                .log_err();
        }
    }
}

unsafe extern "system" fn restore_console(_event: u32) -> windows::core::BOOL {
    if let Some(original) = ORIGINAL_CONSOLE.get() {
        let written = super::client::RESTORE_ON_SIGNAL
            .get()
            .map_or(Ok(()), |restore| unsafe {
                WriteFile(
                    GetStdHandle(STD_OUTPUT_HANDLE)?,
                    Some(restore.as_slice()),
                    None,
                    None,
                )
            });
        let restored = original.restore();
        if written.is_err() || restored.is_err() {
            unsafe {
                windows::Win32::System::Diagnostics::Debug::OutputDebugStringW(windows::core::w!(
                    "Zed TUI could not restore the console during termination"
                ));
            }
        }
    }
    false.into()
}
