use std::{
    fs::File,
    io::{self, BufReader, Write},
    ops::Range,
    os::{
        fd::{AsFd as _, RawFd},
        unix::net::UnixStream,
    },
    path::Path,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU8, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use collections::HashMap;
use crossterm::{cursor, event, style, terminal};
use gpui::{CursorStyle, Modifiers};
use gpui_tui::{Cell, CellAttrs, CellGrid, CursorShape, Glyph, Rgb};
use parking_lot::Mutex;

use crate::tui::frame_diff::{GridScroll, RowShift, changed_ranges, find_moves, find_row_shift};
use crate::tui::protocol::{
    ClientMessage, FrameDecoder, KeyCode, MessageReader, MessageWriter, MouseAction,
    MouseButtonKind, PROTOCOL_VERSION, ServerMessage, TermEvent, WAIT_ONLY_SIZE,
    drop_superseded_moves, write_message,
};
const PUSH_TITLE: &str = "\x1b[22;0t";
const POP_TITLE: &str = "\x1b[23;0t";
const REDRAW_MERGE_GAP: usize = 4;
const MIN_ERASE_RUN: usize = 8;
const MIN_ERASE_RUN_BEFORE_MOVE: usize = 5;
const MIN_ERASE_TAIL: usize = 4;
const MARGIN_QUERY: &str = "\x1b[?69$p";
const MARGIN_MODE_ON: &[u8] = b"\x1b[?69h";
const MARGIN_MODE_OFF: &[u8] = b"\x1b[?69l";
const POINTER_RESET: &[u8] = b"\x1b]22;text\x1b\\";
const CURSOR_SHAPE_RESET: &[u8] = b"\x1b[0 q";
const KEYBOARD_FLAGS_QUERY: &str = "\x1b[?u";
const VERSION_QUERY: &str = "\x1b[>0q";
const DEVICE_ATTRIBUTES_QUERY: &str = "\x1b[c";
const FIRST_PALETTE_SLOT: u8 = 16;
const GRAY_SLOTS_BY_LIGHTNESS: [u8; 4] = [0, 8, 7, 15];
const COLORED_ANSI_SLOTS: [u8; 12] = [1, 2, 3, 4, 5, 6, 9, 10, 11, 12, 13, 14];
const MAX_GRAY_SPREAD: u8 = 24;
const MAX_ANSI_MATCH_DISTANCE: f32 = 0.08;
const MAX_ANSI_MATCH_HUE_DEGREES: f32 = 20.0;
const ANSI_SWITCH_GAIN_DIVISOR: u32 = 4;
const MIN_ANSI_SWITCH_USES: u32 = 32;
const PALETTE_SLOTS: u8 = u8::MAX - FIRST_PALETTE_SLOT + 1;
const MAX_OSC_PARAMS: usize = 16;
const MAX_VERSION_REPLY: usize = 64;
const GHOSTTY_VERSION_PREFIXES: [&[u8]; 2] = [b"ghostty", b"libghostty"];
const OSC_PALETTE_PAIRS: usize = (MAX_OSC_PARAMS - 1) / 2;
const OSC_RESET_SLOTS: usize = MAX_OSC_PARAMS - 1;
const BRIGHT_ANSI_OFFSET: u8 = 60;
const SYNCHRONIZED_FRAME_BYTES: usize = 512;
const QUERY_TIMEOUT: Duration = Duration::from_millis(500);
const HANGUP_POLL_INTERVAL: Duration = Duration::from_millis(50);
const RESIZE_FRAME_WAIT: Duration = Duration::from_millis(100);
const MOUSE_MOVE_INTERVAL: Duration = Duration::from_millis(8);
const ATTRIBUTE_CODES: [(CellAttrs, &str, &str); 2] =
    [(CellAttrs::BOLD, "1", "22"), (CellAttrs::ITALIC, "3", "23")];
const UNDERLINE_OFF: &str = "24";

pub enum Exit {
    Detached,
    ServerShutdown,
    Disconnected,
    Rejected(String),
    WaitFinished { status: i32, errors: Vec<String> },
}

#[derive(Clone, Copy, Default)]
struct TerminalFeatures {
    left_right_margins: bool,
    rectangle_copy: bool,
    ghostty: bool,
    margin_mode: bool,
}

#[derive(Default)]
struct TerminalSetup {
    entered: bool,
    keyboard_enhanced: bool,
    features: TerminalFeatures,
    original_palette: HashMap<u8, String>,
    palette_slots_used: Arc<AtomicU8>,
}

impl TerminalSetup {
    fn run<W: Write>(
        &mut self,
        output: &mut W,
        query: impl FnOnce(&mut W) -> io::Result<QueryReplies>,
    ) -> io::Result<()> {
        write!(output, "{PUSH_TITLE}")?;
        self.entered = true;
        crossterm::execute!(
            output,
            terminal::EnterAlternateScreen,
            cursor::Hide,
            event::EnableMouseCapture,
            event::EnableBracketedPaste,
        )?;
        let replies = query(output)?;
        self.features = replies.features;
        self.original_palette = replies.palette;
        if replies.features.ghostty && replies.features.left_right_margins {
            self.features.margin_mode = true;
            output.write_all(MARGIN_MODE_ON)?;
            output.flush()?;
        }
        if replies.keyboard_flags {
            crossterm::execute!(
                output,
                event::PushKeyboardEnhancementFlags(
                    event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                )
            )?;
            self.keyboard_enhanced = true;
        }
        Ok(())
    }

    fn undo(&self, output: &mut impl Write) {
        let used = self.palette_slots_used.load(Ordering::Relaxed);
        write!(output, "{}", palette_restore(used, &self.original_palette)).ok();
        if self.keyboard_enhanced {
            crossterm::execute!(output, event::PopKeyboardEnhancementFlags).ok();
        }
        if self.features.margin_mode {
            output.write_all(MARGIN_MODE_OFF).ok();
        }
        if self.features.ghostty {
            output.write_all(POINTER_RESET).ok();
        }
        if self.entered {
            output.write_all(CURSOR_SHAPE_RESET).ok();
            crossterm::execute!(
                output,
                style::ResetColor,
                event::DisableBracketedPaste,
                event::DisableMouseCapture,
                cursor::Show,
                terminal::LeaveAlternateScreen,
            )
            .ok();
            write!(output, "{POP_TITLE}").ok();
        }
        output.flush().ok();
    }
}

struct TerminalGuard(TerminalSetup);

impl TerminalGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode().context("enabling raw mode")?;
        let mut guard = Self(TerminalSetup::default());
        guard.0.run(&mut io::stdout(), query_terminal)?;
        restore_on_signal(&guard.0);
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.0.undo(&mut io::stdout());
        terminal::disable_raw_mode().ok();
    }
}

fn query_terminal(stdout: &mut impl Write) -> io::Result<QueryReplies> {
    let mut query = MARGIN_QUERY.to_owned();
    for slot in 0..=u8::MAX {
        query.push_str(&format!("\x1b]4;{slot};?\x07"));
    }
    query.push_str(KEYBOARD_FLAGS_QUERY);
    query.push_str(VERSION_QUERY);
    query.push_str(DEVICE_ATTRIBUTES_QUERY);
    stdout.write_all(query.as_bytes())?;
    stdout.flush()?;
    Ok(read_query_reply(QUERY_TIMEOUT))
}

#[derive(Default)]
struct QueryReplies {
    palette: HashMap<u8, String>,
    device_attributes: bool,
    keyboard_flags: bool,
    features: TerminalFeatures,
    version: Option<Vec<u8>>,
}

impl vte::Perform for QueryReplies {
    fn hook(&mut self, _params: &vte::Params, intermediates: &[u8], ignore: bool, action: char) {
        self.version = (!ignore && intermediates == b">" && action == '|').then(Vec::new);
    }

    fn put(&mut self, byte: u8) {
        if let Some(version) = &mut self.version
            && version.len() < MAX_VERSION_REPLY
        {
            version.push(byte);
        }
    }

    fn unhook(&mut self) {
        if let Some(version) = self.version.take() {
            self.features.ghostty |= GHOSTTY_VERSION_PREFIXES
                .iter()
                .any(|prefix| version.starts_with(prefix));
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if let [b"4", slot, spec] = params
            && spec.starts_with(b"rgb:")
            && let Some(slot) = std::str::from_utf8(slot)
                .ok()
                .and_then(|slot| slot.parse::<u8>().ok())
        {
            self.palette
                .insert(slot, String::from_utf8_lossy(spec).into_owned());
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if ignore {
            return;
        }
        let mut values = params.iter().map(|param| param.first().copied());
        match (intermediates, action) {
            (b"?", 'c') => {
                self.device_attributes = true;
                self.features.rectangle_copy |= values.skip(1).any(|value| value == Some(28));
            }
            (b"?", 'u') => self.keyboard_flags = true,
            (b"?$", 'y') => {
                self.features.left_right_margins |=
                    values.next() == Some(Some(69)) && matches!(values.next(), Some(Some(1..=3)));
            }
            _ => {}
        }
    }
}

static RESTORE_ON_SIGNAL: OnceLock<Vec<u8>> = OnceLock::new();

fn restore_on_signal(setup: &TerminalSetup) {
    let restore = signal_restore(
        &setup.original_palette,
        setup.features.margin_mode,
        setup.features.ghostty,
    );
    if restore.is_empty() || RESTORE_ON_SIGNAL.set(restore).is_err() {
        return;
    }
    for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
        unsafe {
            libc::signal(
                signal,
                write_restore_and_reraise as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
    }
}

extern "C" fn write_restore_and_reraise(signal: libc::c_int) {
    if let Some(restore) = RESTORE_ON_SIGNAL.get() {
        unsafe {
            libc::write(libc::STDOUT_FILENO, restore.as_ptr().cast(), restore.len());
        }
    }
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

fn signal_restore(original: &HashMap<u8, String>, margin_mode: bool, ghostty: bool) -> Vec<u8> {
    let mut restore = if original.is_empty() {
        Vec::new()
    } else {
        palette_restore(PALETTE_SLOTS, original).into_bytes()
    };
    if margin_mode {
        restore.extend_from_slice(MARGIN_MODE_OFF);
    }
    if ghostty {
        restore.extend_from_slice(POINTER_RESET);
    }
    restore.extend_from_slice(CURSOR_SHAPE_RESET);
    restore
}

fn palette_restore(slots_used: u8, original: &HashMap<u8, String>) -> String {
    let slots = (0..FIRST_PALETTE_SLOT)
        .filter(|slot| original.contains_key(slot))
        .chain((FIRST_PALETTE_SLOT..=u8::MAX).take(usize::from(slots_used)));
    let (mut known, mut unknown) = (Vec::new(), Vec::new());
    for slot in slots {
        match original.get(&slot) {
            Some(spec) => known.push((slot, spec)),
            None => unknown.push(slot),
        }
    }
    let mut restore = Vec::new();
    write_osc4(&mut restore, known).ok();
    for chunk in unknown.chunks(OSC_RESET_SLOTS) {
        restore.extend_from_slice(b"\x1b]104");
        for slot in chunk {
            write!(restore, ";{slot}").ok();
        }
        restore.push(b'\x07');
    }
    String::from_utf8_lossy(&restore).into_owned()
}

fn write_osc4<T: std::fmt::Display>(
    output: &mut impl Write,
    definitions: impl IntoIterator<Item = (u8, T)>,
) -> io::Result<()> {
    let definitions: Vec<(u8, T)> = definitions.into_iter().collect();
    for chunk in definitions.chunks(OSC_PALETTE_PAIRS) {
        output.write_all(b"\x1b]4")?;
        for (slot, spec) in chunk {
            write!(output, ";{slot};{spec}")?;
        }
        output.write_all(b"\x07")?;
    }
    Ok(())
}

struct RgbSpec(Rgb);

impl std::fmt::Display for RgbSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        let Rgb { r, g, b } = self.0;
        write!(formatter, "rgb:{r:02x}/{g:02x}/{b:02x}")
    }
}

fn parse_rgb_spec(spec: &str) -> Option<Rgb> {
    let mut channels = spec.strip_prefix("rgb:")?.split('/').map(|hex| {
        let digits = u32::try_from(hex.len())
            .ok()
            .filter(|digits| (1..=4).contains(digits))?;
        let max = (1u32 << (4 * digits)) - 1;
        let value = u32::from_str_radix(hex, 16).ok()?;
        u8::try_from((value * 255 + max / 2) / max).ok()
    });
    let rgb = Rgb::new(channels.next()??, channels.next()??, channels.next()??);
    channels.next().is_none().then_some(rgb)
}

fn is_gray(color: Rgb) -> bool {
    let max = color.r.max(color.g).max(color.b);
    let min = color.r.min(color.g).min(color.b);
    max - min <= MAX_GRAY_SPREAD
}

#[derive(Clone, Copy)]
struct Perceptual {
    lab: theme::Oklab,
    lch: theme::Oklch,
}

impl Perceptual {
    fn of(color: Rgb) -> Self {
        let hsla: gpui::Hsla = gpui::rgb(u32::from(color)).into();
        Self {
            lab: theme::hsla_to_oklab(hsla),
            lch: theme::hsla_to_oklch(hsla),
        }
    }

    fn near_match_distance(&self, terminal: &Perceptual) -> Option<f32> {
        let (a, b) = (self.lab, terminal.lab);
        let distance = ((a.l - b.l).powi(2) + (a.a - b.a).powi(2) + (a.b - b.b).powi(2)).sqrt();
        let hue_difference = (self.lch.hue - terminal.lch.hue).abs();
        let hue_difference = hue_difference.min(360.0 - hue_difference);
        (distance < MAX_ANSI_MATCH_DISTANCE && hue_difference < MAX_ANSI_MATCH_HUE_DEGREES)
            .then_some(distance)
    }
}

struct ColorUse {
    count: u32,
    lightness: f32,
    nearest_ansi_slots: Vec<u8>,
}

#[derive(Default)]
struct Palette {
    enabled: bool,
    slots: HashMap<Rgb, u8>,
    slots_used: Arc<AtomicU8>,
    definitions: Vec<(u8, Rgb)>,
    reported_ansi: [Option<Perceptual>; FIRST_PALETTE_SLOT as usize],
    defined_ansi: [Option<Rgb>; FIRST_PALETTE_SLOT as usize],
    ansi: HashMap<Rgb, u8>,
    uses: HashMap<Rgb, ColorUse>,
    assignable_uses: u32,
    uses_changed: bool,
}

impl Palette {
    fn new(original: &HashMap<u8, String>, slots_used: Arc<AtomicU8>) -> Self {
        Self {
            enabled: !original.is_empty(),
            slots_used,
            reported_ansi: std::array::from_fn(|slot| {
                let spec = original.get(&u8::try_from(slot).ok()?)?;
                Some(Perceptual::of(parse_rgb_spec(spec)?))
            }),
            ..Self::default()
        }
    }

    fn count_use(&mut self, color: PenColor) {
        let PenColor::Color(color) = color else {
            return;
        };
        if !self.enabled {
            return;
        }
        self.uses_changed = true;
        let reported_ansi = &self.reported_ansi;
        let usage = self.uses.entry(color).or_insert_with(|| {
            let perceptual = Perceptual::of(color);
            let mut nearest: Vec<(f32, u8)> = if is_gray(color) {
                Vec::new()
            } else {
                COLORED_ANSI_SLOTS
                    .into_iter()
                    .filter_map(|slot| {
                        let reported = reported_ansi[usize::from(slot)].as_ref()?;
                        Some((perceptual.near_match_distance(reported)?, slot))
                    })
                    .collect()
            };
            nearest.sort_by(|a, b| a.0.total_cmp(&b.0));
            ColorUse {
                count: 0,
                lightness: perceptual.lab.l,
                nearest_ansi_slots: nearest.into_iter().map(|(_, slot)| slot).collect(),
            }
        });
        usage.count += 1;
        if is_gray(color) || !usage.nearest_ansi_slots.is_empty() {
            self.assignable_uses += 1;
        }
    }

    fn best_ansi(&self) -> HashMap<Rgb, u8> {
        let mut by_use: Vec<(&Rgb, &ColorUse)> = self.uses.iter().collect();
        by_use.sort_by_key(|(color, usage)| {
            (std::cmp::Reverse(usage.count), color.r, color.g, color.b)
        });
        let is_reported = |slot: &u8| self.reported_ansi[usize::from(*slot)].is_some();

        let gray_slots: Vec<u8> = GRAY_SLOTS_BY_LIGHTNESS
            .into_iter()
            .filter(is_reported)
            .collect();
        let mut grays: Vec<(Rgb, f32)> = by_use
            .iter()
            .filter(|(color, _)| is_gray(**color))
            .take(gray_slots.len())
            .map(|(color, usage)| (**color, usage.lightness))
            .collect();
        grays.sort_by(|a, b| a.1.total_cmp(&b.1));
        let mut best: HashMap<Rgb, u8> = grays
            .into_iter()
            .map(|(color, _)| color)
            .zip(gray_slots)
            .collect();

        let mut taken = [false; FIRST_PALETTE_SLOT as usize];
        for (color, usage) in &by_use {
            if let Some(slot) = usage
                .nearest_ansi_slots
                .iter()
                .find(|slot| !taken[usize::from(**slot)])
            {
                taken[usize::from(*slot)] = true;
                best.insert(**color, *slot);
            }
        }
        best
    }

    fn rebalance_ansi(&mut self) -> Vec<Rgb> {
        if !std::mem::take(&mut self.uses_changed) {
            return Vec::new();
        }
        let uses_of = |assignment: &HashMap<Rgb, u8>| -> u32 {
            assignment
                .keys()
                .filter_map(|color| self.uses.get(color))
                .map(|usage| usage.count)
                .sum()
        };
        let current_uses = uses_of(&self.ansi);
        let required_uses =
            current_uses + current_uses / ANSI_SWITCH_GAIN_DIVISOR + MIN_ANSI_SWITCH_USES;
        if self.assignable_uses < required_uses {
            return Vec::new();
        }
        let best = self.best_ansi();
        if uses_of(&best) < required_uses {
            return Vec::new();
        }
        let moved: Vec<Rgb> = self
            .ansi
            .iter()
            .filter(|(color, slot)| best.get(color) != Some(slot))
            .map(|(color, _)| *color)
            .collect();
        for (color, slot) in &best {
            let defined = &mut self.defined_ansi[usize::from(*slot)];
            if *defined != Some(*color) {
                *defined = Some(*color);
                self.definitions.push((*slot, *color));
            }
        }
        self.ansi = best;
        moved
    }

    fn slot_for(&mut self, color: Rgb) -> Option<u8> {
        if !self.enabled {
            return None;
        }
        if let Some(slot) = self.ansi.get(&color).or_else(|| self.slots.get(&color)) {
            return Some(*slot);
        }
        let slot = u8::try_from(FIRST_PALETTE_SLOT as usize + self.slots.len()).ok()?;
        self.definitions.push((slot, color));
        self.slots.insert(color, slot);
        self.slots_used
            .fetch_max(slot - FIRST_PALETTE_SLOT + 1, Ordering::Relaxed);
        Some(slot)
    }

    fn write_definitions(&mut self, output: &mut impl Write) -> io::Result<()> {
        write_osc4(
            output,
            self.definitions
                .drain(..)
                .map(|(slot, color)| (slot, RgbSpec(color))),
        )
    }

    fn forget_if_full(&mut self) {
        if self.slots.len() >= usize::from(PALETTE_SLOTS) {
            self.slots.clear();
        }
    }

    fn param(&mut self, layer: Layer, color: PenColor) -> SgrParam {
        let PenColor::Color(rgb) = color else {
            return SgrParam::Code(layer.default_code());
        };
        match (self.slot_for(rgb), layer.ansi_base()) {
            (Some(slot), Some(base)) if slot < 8 => SgrParam::Ansi(base + slot),
            (Some(slot), Some(base)) if slot < FIRST_PALETTE_SLOT => {
                SgrParam::Ansi(base + BRIGHT_ANSI_OFFSET + slot - 8)
            }
            (Some(slot), _) => SgrParam::Indexed(layer.extended_code(), slot),
            (None, _) => SgrParam::Color(layer.extended_code(), rgb),
        }
    }
}

#[derive(Clone, Copy)]
enum Layer {
    Foreground,
    Background,
    Underline,
}

impl Layer {
    fn ansi_base(self) -> Option<u8> {
        match self {
            Self::Foreground => Some(30),
            Self::Background => Some(40),
            Self::Underline => None,
        }
    }

    fn extended_code(self) -> &'static str {
        match self {
            Self::Foreground => "38",
            Self::Background => "48",
            Self::Underline => "58",
        }
    }

    fn default_code(self) -> &'static str {
        match self {
            Self::Foreground => "39",
            Self::Background => "49",
            Self::Underline => "59",
        }
    }
}

fn read_query_reply(timeout: Duration) -> QueryReplies {
    let deadline = Instant::now() + timeout;
    let mut parser = vte::Parser::new();
    let mut replies = QueryReplies::default();
    while !replies.device_attributes {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut poll_fd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
        if unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) } <= 0 {
            break;
        }
        let mut buffer = [0u8; 4096];
        let count =
            unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
        match usize::try_from(count) {
            Ok(count) if count > 0 => {
                parser.advance(&mut replies, buffer.get(..count).unwrap_or_default())
            }
            _ => break,
        }
    }
    replies
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum PenColor {
    #[default]
    Default,
    Color(Rgb),
}

impl PenColor {
    fn background(cell: &Cell) -> Self {
        if cell.attrs.contains(CellAttrs::DEFAULT_BACKGROUND) {
            Self::Default
        } else {
            Self::Color(cell.bg)
        }
    }

    fn foreground(cell: &Cell) -> Self {
        if cell.attrs.contains(CellAttrs::DEFAULT_FOREGROUND) {
            Self::Default
        } else {
            Self::Color(cell.fg)
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Pen {
    fg: Option<PenColor>,
    bg: PenColor,
    underline: PenColor,
    attrs: CellAttrs,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Style {
    fg: Option<PenColor>,
    bg: PenColor,
    underline: Option<PenColor>,
    attrs: CellAttrs,
    relevant: CellAttrs,
}

impl Style {
    fn blank(bg: PenColor) -> Self {
        Self {
            fg: None,
            bg,
            underline: None,
            attrs: CellAttrs::empty(),
            relevant: CellAttrs::UNDERLINE | CellAttrs::CURLY_UNDERLINE,
        }
    }

    fn of_cell(cell: &Cell, styled_underlines: bool) -> Self {
        if cell.is_plain_blank() {
            return Self::blank(PenColor::background(cell));
        }
        let mut attrs = cell.attrs
            - (CellAttrs::WIDE_CONTINUATION
                | CellAttrs::DEFAULT_BACKGROUND
                | CellAttrs::DEFAULT_FOREGROUND);
        if !styled_underlines {
            attrs.remove(CellAttrs::CURLY_UNDERLINE);
        }
        let underline = (styled_underlines && attrs.contains(CellAttrs::UNDERLINE)).then(|| {
            match cell.underline.rgb() {
                Some(color) if color != cell.fg => PenColor::Color(color),
                _ => PenColor::Default,
            }
        });
        Self {
            fg: Some(PenColor::foreground(cell)),
            bg: PenColor::background(cell),
            underline,
            attrs,
            relevant: CellAttrs::all(),
        }
    }
}

impl Pen {
    fn shows(&self, style: &Style) -> bool {
        self.plan(style, &mut SgrParam::of).is_none()
    }

    fn after(&self, style: &Style, wanted: CellAttrs, resets: bool) -> Pen {
        let mut pen = *self;
        if resets {
            pen.fg = Some(style.fg.unwrap_or(PenColor::Default));
            pen.underline = style.underline.unwrap_or(PenColor::Default);
        } else {
            if style.fg.is_some() {
                pen.fg = style.fg;
            }
            if let Some(underline) = style.underline {
                pen.underline = underline;
            }
        }
        pen.bg = style.bg;
        pen.attrs = wanted;
        pen
    }

    fn push_changes(
        &self,
        style: &Style,
        wanted: CellAttrs,
        sink: &mut impl SgrSink,
        color: &mut impl FnMut(Layer, PenColor) -> SgrParam,
    ) {
        for (flag, on, off) in ATTRIBUTE_CODES {
            if self.attrs.contains(flag) && !wanted.contains(flag) {
                sink.push(SgrParam::Code(off));
            } else if wanted.contains(flag) && !self.attrs.contains(flag) {
                sink.push(SgrParam::Code(on));
            }
        }
        match (underline_code(self.attrs), underline_code(wanted)) {
            (Some(_), None) => sink.push(SgrParam::Code(UNDERLINE_OFF)),
            (shown, Some(code)) if shown != Some(code) => sink.push(SgrParam::Code(code)),
            _ => {}
        }
        if let Some(fg) = style.fg.filter(|fg| self.fg != Some(*fg)) {
            sink.push(color(Layer::Foreground, fg));
        }
        if self.bg != style.bg {
            sink.push(color(Layer::Background, style.bg));
        }
        if let Some(underline) = style
            .underline
            .filter(|underline| self.underline != *underline)
        {
            sink.push(color(Layer::Underline, underline));
        }
    }

    fn plan(
        &self,
        style: &Style,
        color: &mut impl FnMut(Layer, PenColor) -> SgrParam,
    ) -> Option<SgrPlan> {
        let wanted = (self.attrs - style.relevant) | (style.attrs & style.relevant);
        let mut changes = ParamLength::default();
        self.push_changes(style, wanted, &mut changes, color);
        if changes.count == 0 {
            return None;
        }
        let mut reset = ParamLength::default();
        push_reset(style, wanted, &mut reset, color);
        let reset_len = usize::from(reset.count > 0) + reset.len();
        let resets = reset_len < changes.len();
        Some(SgrPlan {
            len: 3 + if resets { reset_len } else { changes.len() },
            resets,
            wanted,
        })
    }

    fn write_plan(
        &mut self,
        output: &mut impl Write,
        style: &Style,
        plan: &SgrPlan,
        color: &mut impl FnMut(Layer, PenColor) -> SgrParam,
    ) -> io::Result<()> {
        output.write_all(b"\x1b[")?;
        let mut writer = ParamWriter {
            output: &mut *output,
            count: usize::from(plan.resets),
            result: Ok(()),
        };
        if plan.resets {
            push_reset(style, plan.wanted, &mut writer, color);
        } else {
            self.push_changes(style, plan.wanted, &mut writer, color);
        }
        writer.result?;
        output.write_all(b"m")?;
        *self = self.after(style, plan.wanted, plan.resets);
        Ok(())
    }

    fn write_style(
        &mut self,
        output: &mut impl Write,
        palette: &mut Palette,
        style: Style,
    ) -> io::Result<()> {
        let mut color = |layer, color| palette.param(layer, color);
        let Some(plan) = self.plan(&style, &mut color) else {
            return Ok(());
        };
        let counted_fg = if plan.resets {
            style.fg
        } else {
            style.fg.filter(|fg| self.fg != Some(*fg))
        };
        let counts_bg = plan.resets || self.bg != style.bg;
        self.write_plan(output, &style, &plan, &mut color)?;
        if let Some(fg) = counted_fg {
            palette.count_use(fg);
        }
        if counts_bg {
            palette.count_use(style.bg);
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum SgrParam {
    Code(&'static str),
    Ansi(u8),
    Indexed(&'static str, u8),
    Color(&'static str, Rgb),
}

impl SgrParam {
    fn of(layer: Layer, color: PenColor) -> Self {
        match color {
            PenColor::Default => Self::Code(layer.default_code()),
            PenColor::Color(rgb) => Self::Color(layer.extended_code(), rgb),
        }
    }

    fn len(self) -> usize {
        match self {
            Self::Code(code) => code.len(),
            Self::Ansi(code) => decimal_len(usize::from(code)),
            Self::Indexed(code, slot) => code.len() + 3 + decimal_len(usize::from(slot)),
            Self::Color(code, Rgb { r, g, b }) => {
                code.len()
                    + 5
                    + decimal_len(usize::from(r))
                    + decimal_len(usize::from(g))
                    + decimal_len(usize::from(b))
            }
        }
    }

    fn write(self, output: &mut impl Write) -> io::Result<()> {
        match self {
            Self::Code(code) => output.write_all(code.as_bytes()),
            Self::Ansi(code) => write_decimal(output, usize::from(code)),
            Self::Indexed(code, slot) => {
                output.write_all(code.as_bytes())?;
                output.write_all(b";5;")?;
                write_decimal(output, usize::from(slot))
            }
            Self::Color(code, Rgb { r, g, b }) => {
                output.write_all(code.as_bytes())?;
                output.write_all(b";2;")?;
                write_decimal(output, usize::from(r))?;
                output.write_all(b";")?;
                write_decimal(output, usize::from(g))?;
                output.write_all(b";")?;
                write_decimal(output, usize::from(b))
            }
        }
    }
}

trait SgrSink {
    fn push(&mut self, param: SgrParam);
}

#[derive(Default)]
struct ParamLength {
    bytes: usize,
    count: usize,
}

impl ParamLength {
    fn len(&self) -> usize {
        self.bytes + self.count.saturating_sub(1)
    }
}

impl SgrSink for ParamLength {
    fn push(&mut self, param: SgrParam) {
        self.bytes += param.len();
        self.count += 1;
    }
}

struct ParamWriter<'a, W: Write> {
    output: &'a mut W,
    count: usize,
    result: io::Result<()>,
}

impl<W: Write> SgrSink for ParamWriter<'_, W> {
    fn push(&mut self, param: SgrParam) {
        if self.result.is_err() {
            return;
        }
        if self.count > 0
            && let Err(error) = self.output.write_all(b";")
        {
            self.result = Err(error);
            return;
        }
        self.count += 1;
        self.result = param.write(self.output);
    }
}

#[derive(Clone, Copy)]
struct SgrPlan {
    len: usize,
    resets: bool,
    wanted: CellAttrs,
}

fn push_reset(
    style: &Style,
    wanted: CellAttrs,
    sink: &mut impl SgrSink,
    color: &mut impl FnMut(Layer, PenColor) -> SgrParam,
) {
    for (flag, on, _) in ATTRIBUTE_CODES {
        if wanted.contains(flag) {
            sink.push(SgrParam::Code(on));
        }
    }
    if let Some(code) = underline_code(wanted) {
        sink.push(SgrParam::Code(code));
    }
    for (layer, pen_color) in [
        (Layer::Foreground, style.fg),
        (Layer::Background, Some(style.bg)),
        (Layer::Underline, style.underline),
    ] {
        if let Some(pen_color @ PenColor::Color(_)) = pen_color {
            sink.push(color(layer, pen_color));
        }
    }
}

fn decimal_len(value: usize) -> usize {
    value
        .checked_ilog10()
        .map_or(1, |exponent| exponent as usize + 1)
}

fn write_csi(output: &mut impl Write, value: usize, final_byte: char) -> io::Result<()> {
    output.write_all(b"\x1b[")?;
    write_decimal(output, value)?;
    output.write_all(final_byte.encode_utf8(&mut [0; 4]).as_bytes())
}

fn write_decimal(output: &mut impl Write, value: usize) -> io::Result<()> {
    let mut digits = [0u8; 20];
    let mut start = digits.len();
    let mut rest = value;
    loop {
        start -= 1;
        if let Some(digit) = digits.get_mut(start) {
            *digit = b'0' + (rest % 10) as u8;
        }
        rest /= 10;
        if rest == 0 || start == 0 {
            break;
        }
    }
    output.write_all(digits.get(start..).unwrap_or_default())
}

fn underline_code(attrs: CellAttrs) -> Option<&'static str> {
    if !attrs.contains(CellAttrs::UNDERLINE) {
        None
    } else if attrs.contains(CellAttrs::CURLY_UNDERLINE) {
        Some("4:3")
    } else {
        Some("4")
    }
}

fn assume_erased(shown: &mut [Cell], wanted: &[Cell]) {
    for (shown, wanted) in shown.iter_mut().zip(wanted) {
        if wanted.is_plain_blank() && wanted.attrs.contains(CellAttrs::DEFAULT_BACKGROUND) {
            *shown = *wanted;
        }
    }
}

fn forget_cells_drawn_with(screen: &mut CellGrid, colors: &[Rgb]) {
    let is_recolored =
        |color: PenColor| matches!(color, PenColor::Color(color) if colors.contains(&color));
    for cell in &mut screen.cells {
        let underline_recolored = cell.attrs.contains(CellAttrs::UNDERLINE)
            && cell
                .underline
                .rgb()
                .is_some_and(|color| colors.contains(&color));
        if is_recolored(PenColor::background(cell))
            || (!cell.is_plain_blank() && is_recolored(PenColor::foreground(cell)))
            || underline_recolored
        {
            *cell = unknown_cell();
        }
    }
}

fn starts_its_own_cell(ch: char) -> bool {
    matches!(ch, ' '..='~' | '\u{2500}'..='\u{259f}')
}

fn is_blank_on(cell: &Cell, background: PenColor) -> bool {
    cell.is_plain_blank() && PenColor::background(cell) == background
}

fn uniform_blank_background(cells: &[Cell]) -> Option<PenColor> {
    let background = PenColor::background(cells.first()?);
    cells
        .iter()
        .all(|cell| is_blank_on(cell, background))
        .then_some(background)
}

fn blank_tail_start(cells: &[Cell], cols: usize) -> Option<usize> {
    let cells = cells.get(..cols)?;
    let background = PenColor::background(cells.last()?);
    Some(
        cells
            .iter()
            .rposition(|cell| !is_blank_on(cell, background))
            .map_or(0, |last_drawn| last_drawn + 1),
    )
}

fn unknown_cell() -> Cell {
    Cell {
        glyph: '\0'.into(),
        ..Cell::blank(Rgb::default())
    }
}

#[derive(Clone, Copy)]
enum CursorStep {
    Stay,
    Absolute { line: usize, column: usize },
    Newlines(usize),
    Relative(char, usize),
    Line(usize),
    Column(usize),
    CarriageReturn,
    Backspace,
    CarriageReturnThenRight(usize),
}

impl CursorStep {
    fn len(self) -> usize {
        match self {
            Self::Stay => 0,
            Self::Absolute { line: 1, column: 1 } => 3,
            Self::Absolute { line: 1, column } => 4 + decimal_len(column),
            Self::Absolute { line, column: 1 } => 3 + decimal_len(line),
            Self::Absolute { line, column } => 4 + decimal_len(line) + decimal_len(column),
            Self::Newlines(count) => count,
            Self::Relative(final_byte, 1) => 2 + final_byte.len_utf8(),
            Self::Relative(final_byte, distance) => {
                2 + decimal_len(distance) + final_byte.len_utf8()
            }
            Self::Line(number) | Self::Column(number) => 3 + decimal_len(number),
            Self::CarriageReturn | Self::Backspace => 1,
            Self::CarriageReturnThenRight(distance) => 1 + Self::Relative('C', distance).len(),
        }
    }

    fn write(self, output: &mut impl Write) -> io::Result<()> {
        match self {
            Self::Stay => Ok(()),
            Self::Absolute { line: 1, column: 1 } => output.write_all(b"\x1b[H"),
            Self::Absolute { line: 1, column } => {
                output.write_all(b"\x1b[;")?;
                write_decimal(output, column)?;
                output.write_all(b"H")
            }
            Self::Absolute { line, column: 1 } => write_csi(output, line, 'H'),
            Self::Absolute { line, column } => {
                output.write_all(b"\x1b[")?;
                write_decimal(output, line)?;
                output.write_all(b";")?;
                write_decimal(output, column)?;
                output.write_all(b"H")
            }
            Self::Newlines(count) => (0..count).try_for_each(|_| output.write_all(b"\n")),
            Self::Relative(final_byte, 1) => {
                output.write_all(b"\x1b[")?;
                output.write_all(final_byte.encode_utf8(&mut [0; 4]).as_bytes())
            }
            Self::Relative(final_byte, distance) => write_csi(output, distance, final_byte),
            Self::Line(number) => write_csi(output, number, 'd'),
            Self::Column(number) => write_csi(output, number, 'G'),
            Self::CarriageReturn => output.write_all(b"\r"),
            Self::Backspace => output.write_all(b"\x08"),
            Self::CarriageReturnThenRight(distance) => {
                output.write_all(b"\r")?;
                Self::Relative('C', distance).write(output)
            }
        }
    }

    fn shortest<const N: usize>(steps: [Self; N]) -> Self {
        steps
            .into_iter()
            .min_by_key(|step| step.len())
            .unwrap_or(Self::Stay)
    }
}

fn write_shortest_move(
    output: &mut impl Write,
    from: Option<(usize, u16)>,
    col: usize,
    row: u16,
) -> io::Result<()> {
    shortest_move_steps(from, col, row)
        .into_iter()
        .try_for_each(|step| step.write(output))
}

fn shortest_move_steps(from: Option<(usize, u16)>, col: usize, row: u16) -> [CursorStep; 2] {
    let line = row as usize + 1;
    let absolute = CursorStep::Absolute {
        line,
        column: col + 1,
    };
    let Some((current_col, current_row)) = from else {
        return [absolute, CursorStep::Stay];
    };
    let (vertical, line_start) = match row.cmp(&current_row) {
        std::cmp::Ordering::Equal => (CursorStep::Stay, None),
        std::cmp::Ordering::Greater => {
            let distance = (row - current_row) as usize;
            let vertical = CursorStep::shortest([
                CursorStep::Newlines(distance),
                CursorStep::Relative('B', distance),
                CursorStep::Line(line),
            ]);
            (
                vertical,
                (col == 0).then_some(CursorStep::Relative('E', distance)),
            )
        }
        std::cmp::Ordering::Less => {
            let distance = (current_row - row) as usize;
            let vertical =
                CursorStep::shortest([CursorStep::Relative('A', distance), CursorStep::Line(line)]);
            (
                vertical,
                (col == 0).then_some(CursorStep::Relative('F', distance)),
            )
        }
    };
    let horizontal = if col == current_col {
        CursorStep::Stay
    } else if col == 0 {
        CursorStep::CarriageReturn
    } else if col + 1 == current_col {
        CursorStep::Backspace
    } else {
        let relative = if col > current_col {
            CursorStep::Relative('C', col - current_col)
        } else {
            CursorStep::Relative('D', current_col - col)
        };
        CursorStep::shortest([
            CursorStep::Column(col + 1),
            CursorStep::CarriageReturnThenRight(col),
            relative,
        ])
    };
    let single = line_start.map_or(absolute, |line_start| {
        CursorStep::shortest([absolute, line_start])
    });
    if vertical.len() + horizontal.len() <= single.len() {
        [vertical, horizontal]
    } else {
        [single, CursorStep::Stay]
    }
}

struct Terminal<W: Write> {
    output: W,
    body: Vec<u8>,
    cols: u16,
    rows: u16,
    pen: Pen,
    cursor: Option<(usize, u16)>,
    cursor_visible: bool,
    cursor_shape: Option<CursorShape>,
    features: TerminalFeatures,
    palette: Palette,
    pointer: Option<&'static str>,
    draw_scratch: DrawScratch,
}

impl<W: Write> Terminal<W> {
    fn clear(&mut self) -> io::Result<()> {
        self.body.write_all(b"\x1b[m\x1b[2J")?;
        self.pen = Pen::default();
        self.palette.forget_if_full();
        self.cursor = None;
        Ok(())
    }

    fn move_cells(&mut self, scroll: &GridScroll) -> io::Result<()> {
        if !self.features.rectangle_copy {
            return self.scroll(scroll);
        }
        let columns = scroll.columns.clone().unwrap_or(0..self.cols as usize);
        let distance = scroll.shift.unsigned_abs();
        let (source_top, source_bottom, destination_top) = if scroll.shift > 0 {
            (scroll.top + distance, scroll.bottom, scroll.top)
        } else {
            (scroll.top, scroll.bottom - distance, scroll.top + distance)
        };
        write!(
            self.body,
            "\x1b[{};{};{};{};1;{};{};1$v",
            source_top + 1,
            columns.start + 1,
            source_bottom,
            columns.end,
            destination_top + 1,
            columns.start + 1,
        )
    }

    fn scroll(&mut self, scroll: &GridScroll) -> io::Result<()> {
        self.pen.write_style(
            &mut self.body,
            &mut self.palette,
            Style::blank(PenColor::Default),
        )?;
        let whole_screen =
            scroll.top == 0 && scroll.bottom == self.rows as usize && scroll.columns.is_none();
        if scroll.columns.is_some() && !self.features.margin_mode {
            self.body.write_all(MARGIN_MODE_ON)?;
        }
        if !whole_screen {
            write!(self.body, "\x1b[{};{}r", scroll.top + 1, scroll.bottom)?;
        }
        if let Some(columns) = &scroll.columns {
            write!(self.body, "\x1b[{};{}s", columns.start + 1, columns.end)?;
        }
        let final_byte = if scroll.shift > 0 { 'S' } else { 'T' };
        CursorStep::Relative(final_byte, scroll.shift.unsigned_abs()).write(&mut self.body)?;
        if scroll.columns.is_some() {
            self.body.write_all(b"\x1b[s")?;
            if !self.features.margin_mode {
                self.body.write_all(MARGIN_MODE_OFF)?;
            }
        }
        if !whole_screen {
            self.body.write_all(b"\x1b[r")?;
            self.cursor = Some((0, 0));
        }
        Ok(())
    }

    fn shift_cells(
        &mut self,
        row: u16,
        shift: &RowShift,
        erase: Option<PenColor>,
    ) -> io::Result<()> {
        if let Some(erase) = erase {
            self.pen
                .write_style(&mut self.body, &mut self.palette, Style::blank(erase))?;
        }
        self.move_to(shift.start, row)?;
        let final_byte = if shift.shift > 0 { '@' } else { 'P' };
        CursorStep::Relative(final_byte, shift.shift.unsigned_abs()).write(&mut self.body)
    }

    fn reset_pen(&mut self) -> io::Result<()> {
        self.body.write_all(b"\x1b[m")?;
        self.pen = Pen::default();
        Ok(())
    }

    fn flush_frame(&mut self, force_synchronize: bool) -> io::Result<()> {
        if self.body.is_empty() && self.palette.definitions.is_empty() {
            return Ok(());
        }
        let synchronize = force_synchronize || self.body.len() >= SYNCHRONIZED_FRAME_BYTES;
        if synchronize {
            crossterm::queue!(self.output, terminal::BeginSynchronizedUpdate)?;
        }
        self.palette.write_definitions(&mut self.output)?;
        self.output.write_all(&self.body)?;
        self.body.clear();
        if synchronize {
            crossterm::queue!(self.output, terminal::EndSynchronizedUpdate)?;
        }
        self.output.flush()
    }

    fn move_to(&mut self, col: usize, row: u16) -> io::Result<()> {
        if self.cursor == Some((col, row)) {
            return Ok(());
        }
        write_shortest_move(&mut self.body, self.cursor, col, row)?;
        self.cursor = Some((col, row));
        Ok(())
    }

    fn draw(
        &mut self,
        row: u16,
        cells: &[Cell],
        range: Range<usize>,
        blank_tail_start: Option<usize>,
    ) -> io::Result<usize> {
        let mut scratch = std::mem::take(&mut self.draw_scratch);
        let drawn = self
            .plan_draw(cells, range, blank_tail_start, &mut scratch)
            .and_then(|drawn_to| {
                self.draw_planned(row, &mut scratch)?;
                Ok(drawn_to)
            });
        self.draw_scratch = scratch;
        drawn
    }

    fn draw_planned(&mut self, row: u16, scratch: &mut DrawScratch) -> io::Result<()> {
        match self.plan_grouped_order(row, scratch) {
            None => scratch
                .segments
                .iter()
                .try_for_each(|segment| self.draw_segment(row, segment, &scratch.text)),
            Some(PlannedOrder::Grouped) => scratch
                .order
                .iter()
                .zip(&scratch.plans)
                .filter_map(|(index, plan)| Some((scratch.segments.get(*index)?, plan)))
                .try_for_each(|(segment, plan)| {
                    self.draw_planned_segment(row, segment, plan, &scratch.text)
                }),
            Some(PlannedOrder::InOrder) => scratch
                .segments
                .iter()
                .zip(&scratch.in_order_plans)
                .try_for_each(|(segment, plan)| {
                    self.draw_planned_segment(row, segment, plan, &scratch.text)
                }),
        }
    }

    fn plan_grouped_order(&self, row: u16, scratch: &mut DrawScratch) -> Option<PlannedOrder> {
        let DrawScratch {
            segments,
            styles,
            style_counts,
            shown,
            order,
            plans,
            in_order_plans,
            pending,
            deferred,
            steps,
            has_clusters,
            ..
        } = scratch;
        if self.palette.enabled || *steps <= 2 || *has_clusters || styles.len() < 2 {
            return None;
        }
        let start = DrawState {
            pen: self.pen,
            cursor: self.cursor,
        };
        style_counts.clear();
        style_counts.resize(styles.len(), 0);
        for segment in segments.iter() {
            if let Some(count) = style_counts.get_mut(segment.style_id) {
                *count += 1;
            }
        }
        let mut state = start;
        let mut grouped = 0usize;
        order.clear();
        plans.clear();
        pending.clear();
        pending.extend(0..segments.len());
        let mut leader = None;
        loop {
            if let Some(index) = leader
                && let Some(segment) = segments.get(index)
            {
                let (bytes, plan) = state.plan_segment(row, segment, false);
                grouped = grouped.saturating_add(bytes);
                order.push(index);
                plans.push(plan);
                if let Some(count) = style_counts.get_mut(segment.style_id) {
                    *count = count.saturating_sub(1);
                }
            }
            shown.clear();
            shown.resize(styles.len(), None);
            let any_shown = styles
                .iter()
                .zip(style_counts.iter())
                .zip(shown.iter_mut())
                .any(|((style, count), known)| {
                    *count > 0 && *known.get_or_insert_with(|| state.pen.shows(style))
                });
            if any_shown {
                deferred.clear();
                for &index in pending.iter() {
                    let Some(segment) = segments.get(index) else {
                        continue;
                    };
                    let style_shown = shown.get_mut(segment.style_id).is_some_and(|known| {
                        *known.get_or_insert_with(|| state.pen.shows(&segment.style))
                    });
                    if style_shown && state.reaches(row, segment) {
                        let (bytes, plan) = state.plan_segment(row, segment, true);
                        grouped = grouped.saturating_add(bytes);
                        order.push(index);
                        plans.push(plan);
                        if let Some(count) = style_counts.get_mut(segment.style_id) {
                            *count = count.saturating_sub(1);
                        }
                    } else {
                        deferred.push(index);
                    }
                }
                std::mem::swap(pending, deferred);
            }
            if pending.is_empty() {
                break;
            }
            leader = Some(pending.remove(0));
        }
        if order
            .iter()
            .enumerate()
            .all(|(position, index)| position == *index)
        {
            return Some(PlannedOrder::Grouped);
        }
        let mut state = start;
        in_order_plans.clear();
        let mut in_order = 0usize;
        for segment in segments.iter() {
            let (bytes, plan) = state.plan_segment(row, segment, false);
            in_order = in_order.saturating_add(bytes);
            if in_order > grouped {
                return Some(PlannedOrder::Grouped);
            }
            in_order_plans.push(plan);
        }
        Some(PlannedOrder::InOrder)
    }

    fn plan_draw(
        &self,
        cells: &[Cell],
        range: Range<usize>,
        blank_tail_start: Option<usize>,
        scratch: &mut DrawScratch,
    ) -> io::Result<usize> {
        let cols = self.cols as usize;
        let visible_cols = cells.len().min(cols);
        let identifies_styles = !self.palette.enabled;
        scratch.segments.clear();
        scratch.text.clear();
        scratch.styles.clear();
        scratch.steps = 0;
        scratch.has_clusters = false;
        let mut col = range.start;
        while col < range.end {
            let Some(cell) = cells.get(col) else {
                break;
            };
            if cell.is_wide_continuation() {
                col += 1;
                continue;
            }
            scratch.steps += 1;
            let background = PenColor::background(cell);
            let blank_run = cells
                .get(col..range.end)
                .unwrap_or_default()
                .iter()
                .take_while(|blank| is_blank_on(blank, background))
                .count();
            let erases_to_edge =
                blank_tail_start.is_some_and(|start| col >= start) && cols - col >= MIN_ERASE_TAIL;
            let min_run = if col + blank_run == range.end {
                MIN_ERASE_RUN_BEFORE_MOVE
            } else {
                MIN_ERASE_RUN
            };
            if erases_to_edge || blank_run >= min_run {
                let (kind, content) = if erases_to_edge {
                    (SegmentKind::EraseToEdge, 3)
                } else {
                    (SegmentKind::Erase(blank_run), 3 + decimal_len(blank_run))
                };
                let style = Style::blank(background);
                let style_id = style_id_of(&mut scratch.styles, style, identifies_styles);
                scratch.segments.push(DrawSegment {
                    col,
                    style,
                    style_id,
                    kind,
                    is_blank_glyph: false,
                    content,
                    cursor_after: Some(col),
                });
                if erases_to_edge {
                    return Ok(cols);
                }
                col += blank_run;
                continue;
            }
            let is_wide = cells
                .get(col + 1)
                .is_some_and(|next| next.is_wide_continuation());
            let (glyph, width) = match (is_wide, col + 2 <= visible_cols) {
                (true, true) => (cell.glyph, 2),
                (true, false) => (Glyph::from_char(' '), 1),
                (false, _) => (cell.glyph, 1),
            };
            let repeats = if self.features.ghostty && !is_wide {
                repeat_count(cells, col, range.end)
            } else {
                0
            };
            let next_col = col + width + repeats;
            let character = glyph.as_char();
            scratch.has_clusters |= character.is_none();
            let text_start = scratch.text.len();
            match character {
                Some(character) => scratch
                    .text
                    .extend_from_slice(character.encode_utf8(&mut [0; 4]).as_bytes()),
                None => glyph.write_to(&mut scratch.text)?,
            }
            if repeats > 0 {
                CursorStep::Relative('b', repeats).write(&mut scratch.text)?;
            }
            let text_end = scratch.text.len();
            let content = text_end - text_start;
            let cursor_after = (next_col < cols && character.is_some()).then_some(next_col);
            let style = Style::of_cell(cell, self.features.ghostty);
            let is_blank_glyph = style.fg.is_none();
            match scratch.segments.last_mut() {
                Some(DrawSegment {
                    style: last_style,
                    kind: SegmentKind::Glyphs { end, .. },
                    is_blank_glyph: last_is_blank,
                    content: last_content,
                    cursor_after: last_cursor_after,
                    ..
                }) if *last_cursor_after == Some(col)
                    && (*last_style == style
                        || (is_blank_glyph
                            && !*last_is_blank
                            && last_style.bg == style.bg
                            && !last_style.attrs.contains(CellAttrs::UNDERLINE))) =>
                {
                    *end = text_end;
                    *last_content += content;
                    *last_cursor_after = cursor_after;
                }
                _ => {
                    let style_id = style_id_of(&mut scratch.styles, style, identifies_styles);
                    scratch.segments.push(DrawSegment {
                        col,
                        style,
                        style_id,
                        kind: SegmentKind::Glyphs {
                            start: text_start,
                            end: text_end,
                        },
                        is_blank_glyph,
                        content,
                        cursor_after,
                    });
                }
            }
            col += 1 + repeats;
        }
        Ok(range.end)
    }

    fn draw_segment(&mut self, row: u16, segment: &DrawSegment, text: &[u8]) -> io::Result<()> {
        self.move_to(segment.col, row)?;
        self.pen
            .write_style(&mut self.body, &mut self.palette, segment.style)?;
        self.write_segment_content(row, segment, text)
    }

    fn draw_planned_segment(
        &mut self,
        row: u16,
        segment: &DrawSegment,
        plan: &SegmentPlan,
        text: &[u8],
    ) -> io::Result<()> {
        for step in plan.moves {
            step.write(&mut self.body)?;
        }
        self.cursor = Some((segment.col, row));
        if let Some(sgr) = &plan.sgr {
            self.pen
                .write_plan(&mut self.body, &segment.style, sgr, &mut SgrParam::of)?;
        }
        self.write_segment_content(row, segment, text)
    }

    fn write_segment_content(
        &mut self,
        row: u16,
        segment: &DrawSegment,
        text: &[u8],
    ) -> io::Result<()> {
        match segment.kind {
            SegmentKind::Erase(count) => write_csi(&mut self.body, count, 'X'),
            SegmentKind::EraseToEdge => self.body.write_all(b"\x1b[K"),
            SegmentKind::Glyphs { start, end } => {
                self.body
                    .extend_from_slice(text.get(start..end).unwrap_or_default());
                self.cursor = segment.cursor_after.map(|col| (col, row));
                Ok(())
            }
        }
    }
}

#[derive(Clone, Copy)]
enum PlannedOrder {
    Grouped,
    InOrder,
}

#[derive(Clone, Copy)]
struct SegmentPlan {
    moves: [CursorStep; 2],
    sgr: Option<SgrPlan>,
}

#[derive(Default)]
struct DrawScratch {
    segments: Vec<DrawSegment>,
    text: Vec<u8>,
    steps: usize,
    has_clusters: bool,
    styles: Vec<Style>,
    style_counts: Vec<usize>,
    shown: Vec<Option<bool>>,
    order: Vec<usize>,
    plans: Vec<SegmentPlan>,
    in_order_plans: Vec<SegmentPlan>,
    pending: Vec<usize>,
    deferred: Vec<usize>,
}

fn style_id_of(styles: &mut Vec<Style>, style: Style, identifies_styles: bool) -> usize {
    if !identifies_styles {
        return 0;
    }
    match styles.iter().position(|known| *known == style) {
        Some(style_id) => style_id,
        None => {
            styles.push(style);
            styles.len() - 1
        }
    }
}

#[derive(Clone, Copy)]
enum SegmentKind {
    Erase(usize),
    EraseToEdge,
    Glyphs { start: usize, end: usize },
}

#[derive(Clone, Copy)]
struct DrawSegment {
    col: usize,
    style: Style,
    style_id: usize,
    kind: SegmentKind,
    is_blank_glyph: bool,
    content: usize,
    cursor_after: Option<usize>,
}

#[derive(Clone, Copy)]
struct DrawState {
    pen: Pen,
    cursor: Option<(usize, u16)>,
}

impl DrawState {
    fn reaches(&self, row: u16, segment: &DrawSegment) -> bool {
        !segment.is_blank_glyph || self.cursor == Some((segment.col, row))
    }

    fn plan_segment(
        &mut self,
        row: u16,
        segment: &DrawSegment,
        shown: bool,
    ) -> (usize, SegmentPlan) {
        let moves = if self.cursor == Some((segment.col, row)) {
            [CursorStep::Stay, CursorStep::Stay]
        } else {
            shortest_move_steps(self.cursor, segment.col, row)
        };
        let sgr = if shown {
            None
        } else {
            self.pen.plan(&segment.style, &mut SgrParam::of)
        };
        if let Some(plan) = &sgr {
            self.pen = self.pen.after(&segment.style, plan.wanted, plan.resets);
        }
        self.cursor = segment.cursor_after.map(|col| (col, row));
        let bytes = moves.into_iter().map(CursorStep::len).sum::<usize>()
            + sgr.as_ref().map_or(0, |plan| plan.len)
            + segment.content;
        (bytes, SegmentPlan { moves, sgr })
    }
}

fn repeat_count(cells: &[Cell], col: usize, end: usize) -> usize {
    let Some(cell) = cells.get(col) else {
        return 0;
    };
    let Some(ch) = cell.glyph.as_char().filter(|ch| starts_its_own_cell(*ch)) else {
        return 0;
    };
    if cell.is_plain_blank() {
        return 0;
    }
    let repeats = cells
        .get(col + 1..end)
        .unwrap_or_default()
        .iter()
        .take_while(|next| *next == cell)
        .count();
    if repeats * ch.len_utf8() <= CursorStep::Relative('b', repeats).len() {
        return 0;
    }
    repeats
}

struct Renderer<W: Write> {
    terminal: Terminal<W>,
    grid: Option<CellGrid>,
    screen: Option<CellGrid>,
    decoder: FrameDecoder,
    frames_since_render: usize,
    server_moves: Vec<GridScroll>,
    moved: Option<CellGrid>,
}

impl<W: Write> Renderer<W> {
    fn new(output: W, cols: u16, rows: u16) -> Self {
        Self {
            terminal: Terminal {
                output,
                body: Vec::new(),
                cols,
                rows,
                pen: Pen::default(),
                cursor: None,
                cursor_visible: false,
                cursor_shape: None,
                features: TerminalFeatures::default(),
                palette: Palette::default(),
                pointer: None,
                draw_scratch: DrawScratch::default(),
            },
            grid: None,
            screen: None,
            decoder: FrameDecoder::default(),
            frames_since_render: 0,
            server_moves: Vec::new(),
            moved: None,
        }
    }

    fn apply(&mut self, message: &ServerMessage) -> io::Result<bool> {
        match message {
            ServerMessage::FullFrame(..) | ServerMessage::Diff(..) => {
                self.server_moves = self.decoder.apply(&mut self.grid, message);
                self.frames_since_render += 1;
                return Ok(true);
            }
            ServerMessage::Clipboard(text) => self.copy_to_clipboard(text)?,
            ServerMessage::Title(title) => self.set_title(title)?,
            ServerMessage::Pointer(style) => self.set_pointer(*style)?,
            ServerMessage::Shutdown
            | ServerMessage::Error(_)
            | ServerMessage::WaitFinished { .. } => {}
        }
        Ok(false)
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.terminal.cols = cols;
        self.terminal.rows = rows;
        self.screen = None;
    }

    fn grid_fills_terminal(&self) -> bool {
        self.grid
            .as_ref()
            .is_some_and(|grid| grid.cols == self.terminal.cols && grid.rows == self.terminal.rows)
    }

    fn render(&mut self) -> io::Result<()> {
        let frames_since_render = std::mem::take(&mut self.frames_since_render);
        let mut server_moves = std::mem::take(&mut self.server_moves);
        let Some(grid) = &self.grid else {
            return Ok(());
        };
        let terminal = &mut self.terminal;
        let (mut screen, was_in_sync) = match self.screen.take() {
            Some(screen) if screen.cols == grid.cols && screen.rows == grid.rows => (screen, true),
            _ => {
                terminal.clear()?;
                let mut cleared = CellGrid::new(grid.cols, grid.rows, Rgb::default());
                cleared.cells.fill(unknown_cell());
                assume_erased(&mut cleared.cells, &grid.cells);
                (cleared, false)
            }
        };
        let recolored = terminal.palette.rebalance_ansi();
        if !recolored.is_empty() {
            forget_cells_drawn_with(&mut screen, &recolored);
            terminal.reset_pen()?;
        }
        let force_synchronize = !was_in_sync || !recolored.is_empty();
        let visible_cols = (grid.cols.min(terminal.cols)) as usize;
        let visible_rows = grid.rows.min(terminal.rows) as usize;
        let shows_whole_grid =
            visible_cols == grid.cols as usize && visible_rows == grid.rows as usize;
        let trusts_server_moves = was_in_sync && shows_whole_grid && frames_since_render == 1;
        let rectangle_copy = terminal.features.rectangle_copy;
        let column_windows = rectangle_copy || terminal.features.left_right_margins;
        let move_screen = |scroll: &GridScroll, screen: &mut CellGrid| {
            if rectangle_copy {
                scroll.copy(screen);
                return;
            }
            scroll.apply(screen, unknown_cell());
            let columns = scroll.columns.clone().unwrap_or(0..screen.cols as usize);
            for row in scroll.exposed_rows() {
                let row = row as u16;
                if let (Some(shown), Some(wanted)) = (
                    screen.row_mut(row).get_mut(columns.clone()),
                    grid.row(row).get(columns.clone()),
                ) {
                    assume_erased(shown, wanted);
                }
            }
        };
        if !column_windows && let [scroll] = server_moves.as_mut_slice() {
            scroll.columns = None;
        }
        let server_moves_fit = server_moves.iter().all(|scroll| {
            scroll.fits(visible_rows, visible_cols) && (column_windows || scroll.columns.is_none())
        });
        let moves = if trusts_server_moves && server_moves_fit {
            for scroll in &server_moves {
                move_screen(scroll, &mut screen);
            }
            server_moves
        } else {
            let moves = find_moves(
                &screen,
                grid,
                visible_rows,
                visible_cols,
                column_windows,
                &mut self.moved,
                move_screen,
            );
            if !moves.is_empty()
                && let Some(moved) = &mut self.moved
            {
                std::mem::swap(&mut screen, moved);
            }
            moves
        };
        for scroll in &moves {
            terminal.move_cells(scroll)?;
        }
        if visible_cols == terminal.cols as usize && visible_cols == grid.cols as usize {
            for row in 0..visible_rows as u16 {
                let wanted = grid.row(row);
                let Some(shift) = find_row_shift(screen.row(row), wanted) else {
                    continue;
                };
                let vacated_range = shift.vacated(visible_cols);
                let vacated = wanted.get(vacated_range.clone()).unwrap_or_default();
                let erase = uniform_blank_background(vacated);
                terminal.shift_cells(row, &shift, erase)?;
                let shown = screen.row_mut(row);
                shift.apply(shown, unknown_cell());
                if erase.is_some()
                    && let Some(shown) = shown.get_mut(vacated_range)
                {
                    shown.copy_from_slice(vacated);
                }
            }
        }
        for row in 0..visible_rows as u16 {
            let cells = grid.row(row);
            let visible = cells.get(..visible_cols).unwrap_or(cells);
            let shown = screen.row(row).get(..visible_cols).unwrap_or_default();
            let ranges = changed_ranges(shown, visible, REDRAW_MERGE_GAP);
            if ranges.is_empty() {
                continue;
            }
            let tail_start = blank_tail_start(cells, terminal.cols as usize);
            let mut drawn_to = 0;
            for range in ranges {
                if range.end <= drawn_to {
                    continue;
                }
                drawn_to = terminal.draw(row, cells, range.clone(), tail_start)?;
                let drawn = range.start..drawn_to;
                if let (Some(target), Some(source)) = (
                    screen.row_mut(row).get_mut(drawn.clone()),
                    visible.get(drawn),
                ) {
                    target.copy_from_slice(source);
                }
            }
        }
        let caret = grid.cursor.filter(|cursor| {
            (cursor.col as usize) < visible_cols && (cursor.row as usize) < visible_rows
        });
        match caret {
            Some(caret) => {
                let target = (caret.col as usize, caret.row);
                if terminal.cursor != Some(target) || !terminal.cursor_visible {
                    terminal.move_to(target.0, target.1)?;
                }
                if terminal.cursor_shape != Some(grid.cursor_shape) {
                    terminal
                        .body
                        .write_all(cursor_shape_sequence(grid.cursor_shape))?;
                    terminal.cursor_shape = Some(grid.cursor_shape);
                }
                if !terminal.cursor_visible {
                    crossterm::queue!(terminal.body, cursor::Show)?;
                    terminal.cursor_visible = true;
                }
            }
            None if terminal.cursor_visible => {
                crossterm::queue!(terminal.body, cursor::Hide)?;
                terminal.cursor_visible = false;
            }
            None => {}
        }
        self.screen = Some(screen);
        terminal.flush_frame(force_synchronize)
    }

    fn set_title(&mut self, title: &str) -> io::Result<()> {
        let title: String = title.chars().filter(|ch| !ch.is_control()).collect();
        write!(self.terminal.output, "\x1b]2;{title}\x07")?;
        self.terminal.output.flush()
    }

    fn copy_to_clipboard(&mut self, text: &str) -> io::Result<()> {
        crossterm::queue!(
            self.terminal.output,
            crossterm::clipboard::CopyToClipboard::to_clipboard_from(text)
        )?;
        self.terminal.output.flush()
    }

    fn set_pointer(&mut self, style: CursorStyle) -> io::Result<()> {
        let name = pointer_name(style);
        if !self.terminal.features.ghostty || self.terminal.pointer == Some(name) {
            return Ok(());
        }
        self.terminal.pointer = Some(name);
        write!(self.terminal.output, "\x1b]22;{name}\x1b\\")?;
        self.terminal.output.flush()
    }
}

fn cursor_shape_sequence(shape: CursorShape) -> &'static [u8] {
    match shape {
        CursorShape::Block => b"\x1b[2 q",
        CursorShape::Underline => b"\x1b[4 q",
        CursorShape::Bar => b"\x1b[6 q",
    }
}

fn pointer_name(style: CursorStyle) -> &'static str {
    match style {
        CursorStyle::Arrow => "default",
        CursorStyle::IBeam => "text",
        CursorStyle::Crosshair => "crosshair",
        CursorStyle::ClosedHand => "grabbing",
        CursorStyle::OpenHand => "grab",
        CursorStyle::PointingHand => "pointer",
        CursorStyle::ResizeLeft => "w-resize",
        CursorStyle::ResizeRight => "e-resize",
        CursorStyle::ResizeLeftRight => "ew-resize",
        CursorStyle::ResizeUp => "n-resize",
        CursorStyle::ResizeDown => "s-resize",
        CursorStyle::ResizeUpDown => "ns-resize",
        CursorStyle::ResizeUpLeftDownRight => "nwse-resize",
        CursorStyle::ResizeUpRightDownLeft => "nesw-resize",
        CursorStyle::ResizeColumn => "col-resize",
        CursorStyle::ResizeRow => "row-resize",
        CursorStyle::IBeamCursorForVerticalLayout => "vertical-text",
        CursorStyle::OperationNotAllowed => "not-allowed",
        CursorStyle::DragLink => "alias",
        CursorStyle::DragCopy => "copy",
        CursorStyle::ContextualMenu => "context-menu",
    }
}

pub fn ansi_text(grid: &CellGrid) -> io::Result<String> {
    let mut output = Vec::new();
    for row in 0..grid.rows {
        let mut pen = Pen::default();
        let mut palette = Palette::default();
        for cell in grid.row(row) {
            if cell.is_wide_continuation() {
                continue;
            }
            pen.write_style(&mut output, &mut palette, Style::of_cell(cell, false))?;
            cell.glyph.write_to(&mut output)?;
        }
        crossterm::queue!(output, style::ResetColor)?;
        output.push(b'\n');
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

enum ClientEvent {
    Terminal(event::Event),
    Exit(Exit),
}

struct TerminalOutput {
    buffer: Vec<u8>,
    terminal: File,
}

impl TerminalOutput {
    fn stdout() -> io::Result<Self> {
        Ok(Self {
            buffer: Vec::with_capacity(1 << 16),
            terminal: File::from(io::stdout().as_fd().try_clone_to_owned()?),
        })
    }
}

impl Write for TerminalOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let result = self.terminal.write_all(&self.buffer);
        self.buffer.clear();
        result
    }
}

fn connect(
    socket: &Path,
    (cols, rows): (u16, u16),
    wait: Option<ClientMessage>,
) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("connecting to {}", socket.display()))?;
    write_message(
        &mut stream,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            cols,
            rows,
        },
    )?;
    if let Some(wait) = wait {
        write_message(&mut stream, &wait)?;
    }
    Ok(stream)
}

fn exit_of(message: &ServerMessage) -> Option<Exit> {
    match message {
        ServerMessage::Shutdown => Some(Exit::ServerShutdown),
        ServerMessage::Error(error) => Some(Exit::Rejected(error.clone())),
        ServerMessage::WaitFinished { status, errors } => Some(Exit::WaitFinished {
            status: *status,
            errors: errors.clone(),
        }),
        ServerMessage::FullFrame(..)
        | ServerMessage::Diff(..)
        | ServerMessage::Clipboard(_)
        | ServerMessage::Title(_)
        | ServerMessage::Pointer(_) => None,
    }
}

pub fn attach(socket: &Path, wait: Option<ClientMessage>) -> Result<Exit> {
    let (cols, rows) = terminal::size().context("reading the terminal size")?;
    let stream = connect(socket, (cols, rows), wait)?;
    let socket_writer = Arc::new(Mutex::new(stream.try_clone()?));
    let mut reader = BufReader::new(stream);

    let guard = TerminalGuard::enter()?;
    let mut renderer = Renderer::new(TerminalOutput::stdout()?, cols, rows);
    renderer.terminal.features = guard.0.features;
    renderer.terminal.palette = Palette::new(
        &guard.0.original_palette,
        guard.0.palette_slots_used.clone(),
    );
    let (render_sender, render_receiver) = mpsc::channel();
    thread::Builder::new()
        .name("Socket reader".to_owned())
        .spawn({
            let render_sender = render_sender.clone();
            move || {
                let mut message_reader = MessageReader::default();
                while let Ok(message) = message_reader.read::<ServerMessage>(&mut reader) {
                    if render_sender.send(RenderEvent::Server(message)).is_err() {
                        break;
                    }
                }
            }
        })?;
    let (event_sender, events) = mpsc::channel();
    thread::Builder::new().name("Render".to_owned()).spawn({
        let socket_writer = socket_writer.clone();
        let event_sender = event_sender.clone();
        move || {
            let exit = render_messages(&mut renderer, &render_receiver, &socket_writer);
            event_sender.send(ClientEvent::Exit(exit)).ok();
        }
    })?;
    thread::Builder::new()
        .name("Terminal hangup".to_owned())
        .spawn({
            let event_sender = event_sender.clone();
            move || {
                if wait_for_hangup(libc::STDIN_FILENO) {
                    event_sender
                        .send(ClientEvent::Exit(Exit::Disconnected))
                        .ok();
                }
            }
        })?;
    thread::Builder::new()
        .name("Terminal input".to_owned())
        .spawn(move || {
            while let Ok(event) = event::read() {
                if event_sender.send(ClientEvent::Terminal(event)).is_err() {
                    return;
                }
            }
            event_sender
                .send(ClientEvent::Exit(Exit::Disconnected))
                .ok();
        })?;

    let mut detach_pending = false;
    let mut outgoing = MessageWriter::default();
    let mut throttle = MoveThrottle::default();
    loop {
        let received = match throttle.deadline() {
            Some(deadline) => {
                events.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            }
            None => events
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        let first = match received {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(input) = throttle.take_due(Instant::now()) {
                    outgoing.push(&ClientMessage::Input(input))?;
                    if outgoing.flush(&mut *socket_writer.lock()).is_err() {
                        return Ok(Exit::Disconnected);
                    }
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let mut inputs = Vec::new();
        let mut resize = None;
        for event in std::iter::once(first).chain(events.try_iter()) {
            let event = match event {
                ClientEvent::Exit(exit) => return Ok(exit),
                ClientEvent::Terminal(event) => event,
            };
            match event {
                event::Event::Key(key) => {
                    if key.kind == event::KeyEventKind::Release {
                        continue;
                    }
                    let was_pending = std::mem::take(&mut detach_pending);
                    if !was_pending && is_detach_prefix(&key) {
                        detach_pending = true;
                        continue;
                    }
                    if was_pending && key.code == event::KeyCode::Char('d') {
                        push_inputs(&mut outgoing, &mut throttle, inputs)?;
                        if let Some(input) = throttle.pending.take() {
                            outgoing.push(&ClientMessage::Input(input))?;
                        }
                        outgoing.push(&ClientMessage::Detach)?;
                        outgoing.flush(&mut *socket_writer.lock()).ok();
                        return Ok(Exit::Detached);
                    }
                    inputs.extend(term_key(&key));
                }
                event::Event::Mouse(mouse) => inputs.push(term_mouse(&mouse)),
                event::Event::Paste(text) => inputs.push(TermEvent::Paste(text)),
                event::Event::Resize(cols, rows) => resize = Some((cols, rows)),
                event::Event::FocusGained | event::Event::FocusLost => {}
            }
        }
        push_inputs(&mut outgoing, &mut throttle, inputs)?;
        if let Some((cols, rows)) = resize {
            render_sender.send(RenderEvent::Resized(cols, rows)).ok();
            outgoing.push(&ClientMessage::Resize { cols, rows })?;
        }
        if outgoing.flush(&mut *socket_writer.lock()).is_err() {
            return Ok(Exit::Disconnected);
        }
    }
    Ok(Exit::Disconnected)
}

pub fn wait_without_attaching(socket: &Path, wait: ClientMessage) -> Result<Exit> {
    let mut reader = BufReader::new(connect(socket, WAIT_ONLY_SIZE, Some(wait))?);
    let mut message_reader = MessageReader::default();
    while let Ok(message) = message_reader.read::<ServerMessage>(&mut reader) {
        if let Some(exit) = exit_of(&message) {
            return Ok(exit);
        }
    }
    Ok(Exit::Disconnected)
}

fn wait_for_hangup(fd: RawFd) -> bool {
    loop {
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut poll_fd, 1, -1) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        if poll_fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return true;
        }
        thread::sleep(HANGUP_POLL_INTERVAL);
    }
}

fn push_inputs(
    outgoing: &mut MessageWriter,
    throttle: &mut MoveThrottle,
    inputs: Vec<TermEvent>,
) -> Result<()> {
    for input in throttle.push(Instant::now(), inputs) {
        outgoing.push(&ClientMessage::Input(input))?;
    }
    Ok(())
}

#[derive(Default)]
struct MoveThrottle {
    pending: Option<TermEvent>,
    last_sent: Option<Instant>,
}

impl MoveThrottle {
    fn push(&mut self, now: Instant, inputs: Vec<TermEvent>) -> Vec<TermEvent> {
        let mut inputs: Vec<_> = self.pending.take().into_iter().chain(inputs).collect();
        drop_superseded_moves(&mut inputs, |input| Some(input));
        let throttled = self
            .last_sent
            .is_some_and(|sent| now.saturating_duration_since(sent) < MOUSE_MOVE_INTERVAL);
        if throttled && inputs.last().is_some_and(is_move) {
            self.pending = inputs.pop();
        }
        if inputs.iter().any(is_move) {
            self.last_sent = Some(now);
        }
        inputs
    }

    fn deadline(&self) -> Option<Instant> {
        self.pending.as_ref()?;
        self.last_sent.map(|sent| sent + MOUSE_MOVE_INTERVAL)
    }

    fn take_due(&mut self, now: Instant) -> Option<TermEvent> {
        if self.deadline().is_none_or(|deadline| now < deadline) {
            return None;
        }
        self.last_sent = Some(now);
        self.pending.take()
    }
}

fn is_move(input: &TermEvent) -> bool {
    matches!(input, TermEvent::Mouse { action, .. } if action.is_move())
}

enum RenderEvent {
    Server(ServerMessage),
    Resized(u16, u16),
}

fn render_messages<W: Write>(
    renderer: &mut Renderer<W>,
    events: &mpsc::Receiver<RenderEvent>,
    socket: &Mutex<UnixStream>,
) -> Exit {
    let mut resize_deadline: Option<Instant> = None;
    loop {
        let timeout = resize_deadline.map_or(Duration::MAX, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        });
        let first = match events.recv_timeout(timeout) {
            Ok(event) => Some(event),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Exit::Disconnected,
        };
        let mut frames = 0;
        for event in first.into_iter().chain(events.try_iter()) {
            let message = match event {
                RenderEvent::Resized(cols, rows) => {
                    renderer.resize(cols, rows);
                    resize_deadline = Some(Instant::now() + RESIZE_FRAME_WAIT);
                    continue;
                }
                RenderEvent::Server(message) => message,
            };
            if let Some(exit) = exit_of(&message) {
                return exit;
            }
            match renderer.apply(&message) {
                Ok(is_frame) => frames += u32::from(is_frame),
                Err(_) => return Exit::Disconnected,
            }
        }
        let resized_frame_arrived = frames > 0 && renderer.grid_fills_terminal();
        resize_deadline =
            resize_deadline.filter(|deadline| !resized_frame_arrived && Instant::now() < *deadline);
        if resize_deadline.is_none() && renderer.render().is_err() {
            return Exit::Disconnected;
        }
        if frames > 0
            && write_message(&mut *socket.lock(), &ClientMessage::Rendered(frames)).is_err()
        {
            return Exit::Disconnected;
        }
    }
}

fn is_detach_prefix(key: &event::KeyEvent) -> bool {
    key.modifiers.contains(event::KeyModifiers::CONTROL)
        && matches!(
            key.code,
            event::KeyCode::Char('\\') | event::KeyCode::Char('4')
        )
}

fn term_modifiers(modifiers: event::KeyModifiers) -> Modifiers {
    Modifiers {
        control: modifiers.contains(event::KeyModifiers::CONTROL),
        alt: modifiers.contains(event::KeyModifiers::ALT),
        shift: modifiers.contains(event::KeyModifiers::SHIFT),
        platform: modifiers.contains(event::KeyModifiers::SUPER),
        function: false,
    }
}

fn term_key(key: &event::KeyEvent) -> Option<TermEvent> {
    let code = match key.code {
        event::KeyCode::Char(ch) => KeyCode::Char(ch),
        event::KeyCode::Enter => KeyCode::Enter,
        event::KeyCode::Esc => KeyCode::Escape,
        event::KeyCode::Backspace => KeyCode::Backspace,
        event::KeyCode::Tab => KeyCode::Tab,
        event::KeyCode::BackTab => KeyCode::BackTab,
        event::KeyCode::Left => KeyCode::Left,
        event::KeyCode::Right => KeyCode::Right,
        event::KeyCode::Up => KeyCode::Up,
        event::KeyCode::Down => KeyCode::Down,
        event::KeyCode::Home => KeyCode::Home,
        event::KeyCode::End => KeyCode::End,
        event::KeyCode::PageUp => KeyCode::PageUp,
        event::KeyCode::PageDown => KeyCode::PageDown,
        event::KeyCode::Insert => KeyCode::Insert,
        event::KeyCode::Delete => KeyCode::Delete,
        event::KeyCode::F(number) => KeyCode::Function(number),
        _ => return None,
    };
    Some(TermEvent::Key {
        code,
        modifiers: term_modifiers(key.modifiers),
    })
}

fn term_button(button: event::MouseButton) -> MouseButtonKind {
    match button {
        event::MouseButton::Left => MouseButtonKind::Left,
        event::MouseButton::Right => MouseButtonKind::Right,
        event::MouseButton::Middle => MouseButtonKind::Middle,
    }
}

fn term_mouse(mouse: &event::MouseEvent) -> TermEvent {
    let action = match mouse.kind {
        event::MouseEventKind::Down(button) => MouseAction::Down(term_button(button)),
        event::MouseEventKind::Up(button) => MouseAction::Up(term_button(button)),
        event::MouseEventKind::Drag(button) => MouseAction::Drag(term_button(button)),
        event::MouseEventKind::Moved => MouseAction::Moved,
        event::MouseEventKind::ScrollUp => MouseAction::ScrollUp,
        event::MouseEventKind::ScrollDown => MouseAction::ScrollDown,
        event::MouseEventKind::ScrollLeft => MouseAction::ScrollLeft,
        event::MouseEventKind::ScrollRight => MouseAction::ScrollRight,
    };
    TermEvent::Mouse {
        action,
        col: mouse.column,
        row: mouse.row,
        modifiers: term_modifiers(mouse.modifiers),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::frame_diff::find_scroll;
    use crate::tui::test_support::{Random, source_lines, split_panes, text_row};
    use alacritty_terminal::{
        Term,
        event::VoidListener,
        index::{Column, Line},
        term::{Config, TermMode, cell::Flags, test::TermSize},
        vte::ansi::{self, Processor},
    };
    use gpui_tui::{CursorPosition, UnderlineColor};

    const WIDE: [char; 2] = ['한', '글'];

    fn is_wide(cell: &Cell) -> bool {
        cell.glyph.cells() == 2
    }

    struct Emulator {
        term: Term<VoidListener>,
        processor: Processor,
        styled_underlines: bool,
    }

    impl Emulator {
        fn new(cols: u16, rows: u16) -> Self {
            Self {
                term: Term::new(
                    Config::default(),
                    &TermSize::new(cols as usize, rows as usize),
                    VoidListener,
                ),
                processor: Processor::new(),
                styled_underlines: false,
            }
        }

        fn feed(&mut self, renderer: &mut Renderer<Vec<u8>>) -> usize {
            renderer.render().unwrap();
            let bytes = std::mem::take(&mut renderer.terminal.output);
            let mut rest = bytes.as_slice();
            while let Some(end) = rest.windows(2).position(|pair| pair == b"$v") {
                let start = rest[..end]
                    .windows(2)
                    .rposition(|pair| pair == b"\x1b[")
                    .unwrap();
                self.processor.advance(&mut self.term, &rest[..start]);
                let params: Vec<usize> = std::str::from_utf8(&rest[start + 2..end])
                    .unwrap()
                    .split(';')
                    .map(|param| param.parse().unwrap())
                    .collect();
                self.copy_rectangle(&params);
                rest = &rest[end + 2..];
            }
            self.processor.advance(&mut self.term, rest);
            bytes.len()
        }

        fn copy_rectangle(&mut self, params: &[usize]) {
            let [
                top,
                left,
                bottom,
                right,
                1,
                destination_top,
                destination_left,
                1,
            ] = params
            else {
                panic!("unexpected rectangle copy {params:?}");
            };
            let grid = self.term.grid_mut();
            let copied: Vec<Vec<_>> = (*top..=*bottom)
                .map(|row| {
                    (*left..=*right)
                        .map(|col| grid[Line(row as i32 - 1)][Column(col - 1)].clone())
                        .collect()
                })
                .collect();
            for (row_offset, cells) in copied.into_iter().enumerate() {
                for (col_offset, cell) in cells.into_iter().enumerate() {
                    grid[Line((destination_top + row_offset) as i32 - 1)]
                        [Column(destination_left + col_offset - 1)] = cell;
                }
            }
        }

        fn assert_shows(&self, grid: &CellGrid, cols: u16) {
            let spec = |rgb: Rgb| {
                ansi::Color::Spec(ansi::Rgb {
                    r: rgb.r,
                    g: rgb.g,
                    b: rgb.b,
                })
            };
            let resolve = |color: ansi::Color| match color {
                ansi::Color::Indexed(index) => {
                    self.term.colors()[index as usize].map_or(color, ansi::Color::Spec)
                }
                ansi::Color::Named(name) if (name as usize) < FIRST_PALETTE_SLOT as usize => {
                    self.term.colors()[name as usize].map_or(color, ansi::Color::Spec)
                }
                other => other,
            };
            for row in 0..grid.rows {
                for col in 0..grid.cols.min(cols) {
                    let Some(expected) = grid.cell(col as i32, row as i32) else {
                        continue;
                    };
                    let shown = &self.term.grid()[Line(row as i32)][Column(col as usize)];
                    let at = format!("({col}, {row}) {expected:?} shown as {shown:?}");
                    if expected.is_wide_continuation() {
                        assert!(shown.flags.contains(Flags::WIDE_CHAR_SPACER), "{at}");
                        continue;
                    }
                    let clipped = is_wide(expected) && col + 1 == cols;
                    let mut shown_text = String::from(shown.c);
                    shown_text.extend(shown.zerowidth().into_iter().flatten());
                    let mut expected_text = String::new();
                    if clipped {
                        expected_text.push(' ');
                    } else {
                        expected.glyph.push_to(&mut expected_text);
                    }
                    assert_eq!(shown_text, expected_text, "{at}");
                    let background = if expected.attrs.contains(CellAttrs::DEFAULT_BACKGROUND) {
                        ansi::Color::Named(ansi::NamedColor::Background)
                    } else {
                        spec(expected.bg)
                    };
                    assert_eq!(resolve(shown.bg), background, "{at}");
                    let underline = expected.attrs.contains(CellAttrs::UNDERLINE);
                    let curly = underline
                        && self.styled_underlines
                        && expected.attrs.contains(CellAttrs::CURLY_UNDERLINE);
                    assert_eq!(
                        shown.flags.contains(Flags::UNDERLINE),
                        underline && !curly,
                        "{at}"
                    );
                    assert_eq!(shown.flags.contains(Flags::UNDERCURL), curly, "{at}");
                    if underline {
                        let color = expected
                            .underline
                            .rgb()
                            .filter(|color| self.styled_underlines && *color != expected.fg);
                        assert_eq!(
                            shown.underline_color().map(resolve),
                            color.map(spec),
                            "{at}"
                        );
                    }
                    if !expected.is_plain_blank() {
                        let foreground = if expected.attrs.contains(CellAttrs::DEFAULT_FOREGROUND) {
                            ansi::Color::Named(ansi::NamedColor::Foreground)
                        } else {
                            spec(expected.fg)
                        };
                        assert_eq!(resolve(shown.fg), foreground, "{at}");
                        let bold = expected.attrs.contains(CellAttrs::BOLD);
                        assert_eq!(shown.flags.contains(Flags::BOLD), bold, "{at}");
                        let italic = expected.attrs.contains(CellAttrs::ITALIC);
                        assert_eq!(shown.flags.contains(Flags::ITALIC), italic, "{at}");
                    }
                }
            }
        }
    }

    fn mutate(grid: &mut CellGrid, random: &mut Random, edits: usize) {
        let palette = [
            Rgb::new(30, 33, 40),
            Rgb::new(40, 44, 52),
            Rgb::new(200, 120, 60),
        ];
        let glyphs = [
            Glyph::from_char('a'),
            Glyph::from_char('b'),
            Glyph::from_char('─'),
            Glyph::from_char('│'),
            Glyph::from_char(' '),
            Glyph::from_char(' '),
            Glyph::from_char(' '),
            Glyph::from_char(WIDE[0]),
            Glyph::from_char(WIDE[1]),
            Glyph::from_cluster("e\u{301}"),
        ];
        let attrs = [
            CellAttrs::empty(),
            CellAttrs::empty(),
            CellAttrs::BOLD,
            CellAttrs::ITALIC,
            CellAttrs::UNDERLINE,
            CellAttrs::UNDERLINE | CellAttrs::CURLY_UNDERLINE,
        ];
        let underlines = [
            UnderlineColor::default(),
            UnderlineColor::of(Rgb::new(224, 108, 117)),
            UnderlineColor::of(Rgb::new(200, 120, 60)),
        ];
        for _ in 0..edits {
            let row = random.next(grid.rows as usize) as i32;
            let start = random.next(grid.cols as usize) as i32;
            let length = 1 + random.next(12) as i32;
            let bg = palette[random.next(palette.len())];
            for col in start..start + length {
                if let Some(cell) = grid.cell_mut(col, row) {
                    *cell = Cell {
                        glyph: glyphs[random.next(glyphs.len())],
                        fg: palette[random.next(palette.len())],
                        bg,
                        attrs: attrs[random.next(attrs.len())],
                        underline: underlines[random.next(underlines.len())],
                    };
                }
            }
        }
        let last_col = grid.cols as i32 - 1;
        for row in 0..grid.rows as i32 {
            for col in 0..=last_col {
                let lead_is_wide = grid.cell(col - 1, row).is_some_and(is_wide);
                let Some(cell) = grid.cell_mut(col, row) else {
                    continue;
                };
                if lead_is_wide {
                    *cell = Cell {
                        glyph: ' '.into(),
                        attrs: CellAttrs::WIDE_CONTINUATION,
                        ..*cell
                    };
                } else if cell.is_wide_continuation() || (is_wide(cell) && col == last_col) {
                    *cell = Cell::blank(cell.bg);
                }
            }
        }
        grid.mark_default_colors(&[Rgb::new(40, 44, 52)], &[Rgb::new(200, 120, 60)]);
    }

    fn shift_rows(grid: &mut CellGrid, random: &mut Random) {
        const SIDEBAR_COLS: usize = 8;
        let rows = grid.rows as usize;
        let cols = grid.cols as usize;
        let top = random.next(rows / 2);
        let bottom = top + 2 + random.next(rows - top - 1);
        let distance = (1 + random.next(bottom - top - 1)) as isize;
        let shift = if random.next(2) == 0 {
            distance
        } else {
            -distance
        };
        let before = grid.clone();
        let scroll = GridScroll {
            top,
            bottom,
            shift,
            columns: None,
        };
        scroll.apply(grid, Cell::blank(Rgb::new(40, 44, 52)));
        for row in scroll.exposed_rows() {
            for col in 0..cols {
                if let Some(cell) = grid.cell_mut(col as i32, row as i32) {
                    *cell = Cell::blank(Rgb::new(40, 44, 52));
                }
            }
        }
        for row in top..bottom {
            for col in cols - SIDEBAR_COLS..cols {
                if let (Some(cell), Some(original)) = (
                    grid.cell_mut(col as i32, row as i32),
                    before.cell(col as i32, row as i32),
                ) {
                    *cell = *original;
                }
            }
        }
    }

    fn shift_within_rows(grid: &mut CellGrid, random: &mut Random) {
        let cols = grid.cols as usize;
        for _ in 0..3 {
            let row = random.next(grid.rows as usize) as u16;
            let start = random.next(cols / 2);
            let distance = 1 + random.next(8);
            let end = if random.next(2) == 0 {
                cols
            } else {
                cols - 1 - random.next(4)
            };
            let cells = &mut grid.row_mut(row)[start..end];
            if random.next(2) == 0 {
                cells.rotate_right(distance.min(cells.len()));
            } else {
                cells.rotate_left(distance.min(cells.len()));
            }
        }
        mutate(grid, random, 0);
    }

    fn output_text(renderer: &Renderer<Vec<u8>>) -> String {
        String::from_utf8(renderer.terminal.output.clone()).unwrap()
    }

    fn render_checked(
        renderer: &mut Renderer<Vec<u8>>,
        emulator: &mut Emulator,
        grid: &CellGrid,
    ) -> String {
        renderer.grid = Some(grid.clone());
        renderer.render().unwrap();
        let output = output_text(renderer);
        emulator.feed(renderer);
        emulator.assert_shows(grid, grid.cols);
        output
    }

    fn editor_frame(first_line: usize) -> CellGrid {
        let editor_lines = source_lines(40, 11);
        let mut grid = CellGrid::new(60, 14, Rgb::new(40, 44, 52));
        text_row(&mut grid, 0, 0, "title bar");
        for row in 1..13 {
            text_row(&mut grid, row, 0, &editor_lines[first_line + row as usize]);
            text_row(&mut grid, row, 48, &format!("file_{row}.rs"));
        }
        text_row(&mut grid, 13, 0, "status bar");
        grid
    }

    #[test]
    fn scrolling_reuses_lines_already_on_screen() {
        let mut renderer = Renderer::new(Vec::new(), 60, 14);
        let mut emulator = Emulator::new(60, 14);
        renderer.grid = Some(editor_frame(0));
        let full = emulator.feed(&mut renderer);

        renderer.grid = Some(editor_frame(3));
        renderer.render().unwrap();
        let output = output_text(&renderer);
        assert!(output.contains("\x1b[2;13r\x1b[3S"), "{output:?}");
        let scrolled = emulator.feed(&mut renderer);
        emulator.assert_shows(&editor_frame(3), 60);
        assert!(
            scrolled * 2 < full,
            "scrolling took {scrolled} bytes, a full frame took {full}"
        );
    }

    #[test]
    fn margins_keep_a_static_sidebar_out_of_the_scroll() {
        let scroll = find_scroll(&editor_frame(0), &editor_frame(3), 14, 60, true).unwrap();
        assert_eq!((scroll.top, scroll.bottom, scroll.shift), (1, 13, 3));
        let columns = scroll.columns.unwrap();
        assert!(columns.start == 0 && columns.end <= 48, "{columns:?}");
        assert!(
            find_scroll(&editor_frame(0), &editor_frame(3), 14, 60, false)
                .is_some_and(|scroll| scroll.columns.is_none())
        );

        let mut renderer = Renderer::new(Vec::new(), 60, 14);
        renderer.terminal.features.left_right_margins = true;
        renderer.grid = Some(editor_frame(0));
        renderer.render().unwrap();
        let full_width = {
            let mut plain = Renderer::new(Vec::new(), 60, 14);
            plain.grid = Some(editor_frame(0));
            plain.render().unwrap();
            plain.terminal.output.clear();
            plain.grid = Some(editor_frame(3));
            plain.render().unwrap();
            plain.terminal.output.len()
        };
        renderer.terminal.output.clear();
        renderer.grid = Some(editor_frame(3));
        renderer.render().unwrap();
        let output = output_text(&renderer);
        let expected = format!(
            "\x1b[?69h\x1b[2;13r\x1b[{};{}s\x1b[3S\x1b[s\x1b[?69l\x1b[r",
            columns.start + 1,
            columns.end
        );
        assert!(output.contains(&expected), "{output:?}");
        assert!(!output.contains("file_"), "{output:?}");
        assert!(
            output.len() < full_width,
            "{} >= {full_width}",
            output.len()
        );
    }

    #[test]
    fn kept_margin_mode_scrolls_without_toggling_it() {
        let scroll = find_scroll(&editor_frame(0), &editor_frame(3), 14, 60, true).unwrap();
        let columns = scroll.columns.unwrap();
        let mut renderer = Renderer::new(Vec::new(), 60, 14);
        renderer.terminal.features.left_right_margins = true;
        renderer.terminal.features.margin_mode = true;
        renderer.grid = Some(editor_frame(0));
        renderer.render().unwrap();
        renderer.terminal.output.clear();
        renderer.grid = Some(editor_frame(3));
        renderer.render().unwrap();
        let output = output_text(&renderer);
        let expected = format!(
            "\x1b[2;13r\x1b[{};{}s\x1b[3S\x1b[s\x1b[r",
            columns.start + 1,
            columns.end
        );
        assert!(output.contains(&expected), "{output:?}");
        assert!(!output.contains("?69"), "{output:?}");
    }

    #[test]
    fn ghostty_margin_mode_stays_on_until_undo() {
        let run = |ghostty: bool, left_right_margins: bool| {
            let mut setup = TerminalSetup::default();
            let mut output = FailingWriter::failing_on_flush(0);
            setup
                .run(&mut output, |_| {
                    Ok(QueryReplies {
                        features: TerminalFeatures {
                            ghostty,
                            left_right_margins,
                            ..TerminalFeatures::default()
                        },
                        ..QueryReplies::default()
                    })
                })
                .unwrap();
            let mut undo = Vec::new();
            setup.undo(&mut undo);
            (
                String::from_utf8(output.written).unwrap(),
                String::from_utf8(undo).unwrap(),
                setup.features.margin_mode,
            )
        };
        let (output, undo, margin_mode) = run(true, true);
        assert!(margin_mode);
        assert_eq!(output.matches("\x1b[?69h").count(), 1, "{output:?}");
        let mode_off = undo.find("\x1b[?69l").unwrap();
        assert!(
            mode_off < undo.find(LEAVE_ALTERNATE_SCREEN).unwrap(),
            "{undo:?}"
        );
        for (ghostty, left_right_margins) in [(true, false), (false, true)] {
            let (output, undo, margin_mode) = run(ghostty, left_right_margins);
            assert!(!margin_mode);
            assert!(!output.contains("?69") && !undo.contains("?69"));
        }
        assert_eq!(
            signal_restore(&HashMap::default(), true, false),
            [MARGIN_MODE_OFF, CURSOR_SHAPE_RESET].concat()
        );
        assert_eq!(
            signal_restore(&HashMap::default(), false, false),
            CURSOR_SHAPE_RESET
        );
    }

    #[test]
    fn palette_slots_replace_truecolor_codes() {
        let mut renderer = Renderer::new(Vec::new(), 60, 14);
        renderer.terminal.palette.enabled = true;
        let mut emulator = Emulator::new(60, 14);
        let mut grid = editor_frame(0);
        for (index, cell) in grid.cells.iter_mut().enumerate() {
            cell.fg = [Rgb::new(198, 120, 221), Rgb::new(97, 175, 239)][index % 2];
        }
        renderer.grid = Some(grid.clone());
        renderer.render().unwrap();
        let output = output_text(&renderer);
        assert!(output.contains("\x1b]4;16;rgb:c6/78/dd;"), "{output:?}");
        assert!(output.contains("\x1b[38;5;16;48;5;17m"), "{output:?}");
        assert!(output.contains("\x1b[38;5;18m"), "{output:?}");
        assert!(!output.contains("38;2;"), "{output:?}");
        emulator.feed(&mut renderer);
        emulator.assert_shows(&grid, 60);
        assert_eq!(
            renderer.terminal.palette.slots_used.load(Ordering::Relaxed),
            3
        );
    }

    #[test]
    fn truecolor_rows_draw_each_color_in_one_pass() {
        let (type_color, punctuation) = (Rgb::new(110, 180, 191), Rgb::new(178, 185, 198));
        let mut grid = CellGrid::new(40, 1, Rgb::new(40, 44, 52));
        text_row(&mut grid, 0, 0, "Alpha, Beta, Gamma, Delta");
        for cell in grid.row_mut(0) {
            cell.fg = if cell.glyph == ',' {
                punctuation
            } else {
                type_color
            };
        }
        let render = |palette_enabled: bool| {
            let mut renderer = Renderer::new(Vec::new(), 40, 1);
            renderer.terminal.palette.enabled = palette_enabled;
            let mut emulator = Emulator::new(40, 1);
            render_checked(&mut renderer, &mut emulator, &grid)
        };
        let truecolor = render(false);
        assert_eq!(truecolor.matches("38;2;").count(), 2, "{truecolor:?}");
        let indexed = render(true);
        assert_eq!(indexed.matches("38;5;").count(), 7, "{indexed:?}");
    }

    fn random_pen_color(random: &mut Random) -> PenColor {
        const CHANNELS: [u8; 6] = [0, 7, 40, 99, 100, 255];
        if random.next(4) == 0 {
            return PenColor::Default;
        }
        let mut channel = || CHANNELS[random.next(CHANNELS.len())];
        PenColor::Color(Rgb::new(channel(), channel(), channel()))
    }

    fn random_attrs(random: &mut Random) -> CellAttrs {
        let flags = [
            CellAttrs::BOLD,
            CellAttrs::ITALIC,
            CellAttrs::UNDERLINE,
            CellAttrs::CURLY_UNDERLINE,
        ];
        flags
            .into_iter()
            .filter(|_| random.next(2) == 0)
            .fold(CellAttrs::empty(), |attrs, flag| attrs | flag)
    }

    fn random_style(random: &mut Random) -> Style {
        let optional =
            |random: &mut Random| (random.next(3) != 0).then(|| random_pen_color(random));
        let fg = optional(random);
        let underline = optional(random);
        Style {
            fg,
            bg: random_pen_color(random),
            underline,
            attrs: random_attrs(random),
            relevant: if random.next(2) == 0 {
                CellAttrs::all()
            } else {
                CellAttrs::UNDERLINE | CellAttrs::CURLY_UNDERLINE
            },
        }
    }

    #[test]
    fn truecolor_styles_match_the_formatted_encoder() {
        let mut random = Random::new(7);
        let mut truecolor = Palette::default();
        let mut full = Palette {
            enabled: true,
            ..Palette::default()
        };
        for index in 0..u16::from(PALETTE_SLOTS) {
            full.slots.insert(Rgb::new(3, index as u8, 3), 0);
        }
        for _ in 0..20_000 {
            let pen = Pen {
                fg: (random.next(4) != 0).then(|| random_pen_color(&mut random)),
                bg: random_pen_color(&mut random),
                underline: random_pen_color(&mut random),
                attrs: random_attrs(&mut random),
            };
            let style = random_style(&mut random);
            let (mut written, mut formatted) = (pen, pen);
            let (mut output, mut expected) = (Vec::new(), Vec::new());
            written
                .write_style(&mut output, &mut truecolor, style)
                .unwrap();
            formatted
                .write_style(&mut expected, &mut full, style)
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&output),
                String::from_utf8_lossy(&expected),
                "{pen:?} {style:?}"
            );
            assert_eq!(written, formatted, "{pen:?} {style:?}");
            match pen.plan(&style, &mut SgrParam::of) {
                Some(plan) => {
                    assert_eq!(plan.len, output.len(), "{pen:?} {output:?}");
                    assert_eq!(
                        pen.after(&style, plan.wanted, plan.resets),
                        written,
                        "{pen:?} {output:?}"
                    );
                }
                None => {
                    assert!(output.is_empty(), "{pen:?} {output:?}");
                    assert_eq!(written, pen);
                    assert!(pen.shows(&style));
                }
            }
        }
    }

    #[test]
    fn decimals_are_written_like_display() {
        for value in [0, 1, 9, 10, 99, 100, 255, 1000, 65535, usize::MAX] {
            let mut output = Vec::new();
            write_decimal(&mut output, value).unwrap();
            assert_eq!(String::from_utf8(output).unwrap(), value.to_string());
            assert_eq!(decimal_len(value), value.to_string().len());
        }
    }

    #[test]
    fn cursor_step_lengths_match_their_bytes() {
        let numbers = [0, 1, 2, 9, 10, 11, 99, 100, 101, 999, 1000, 65535];
        let mut steps = vec![
            CursorStep::Stay,
            CursorStep::CarriageReturn,
            CursorStep::Backspace,
        ];
        for &first in &numbers {
            steps.extend([
                CursorStep::Newlines(first),
                CursorStep::Line(first),
                CursorStep::Column(first),
                CursorStep::CarriageReturnThenRight(first),
            ]);
            steps.extend(
                ['A', 'B', 'C', 'D', 'E', 'F', 'S', 'T', '@', 'P', 'b']
                    .map(|final_byte| CursorStep::Relative(final_byte, first)),
            );
            for &second in &numbers {
                steps.push(CursorStep::Absolute {
                    line: first,
                    column: second,
                });
            }
        }
        for step in steps {
            let mut output = Vec::new();
            step.write(&mut output).unwrap();
            assert_eq!(step.len(), output.len(), "{:?}", String::from_utf8(output));
        }
        let mut random = Random::new(11);
        for _ in 0..5_000 {
            let from = (random.next(5) != 0).then(|| (random.next(300), random.next(120) as u16));
            let (col, row) = (random.next(300), random.next(120) as u16);
            let mut output = Vec::new();
            write_shortest_move(&mut output, from, col, row).unwrap();
            let planned: usize = shortest_move_steps(from, col, row)
                .into_iter()
                .map(CursorStep::len)
                .sum();
            assert_eq!(planned, output.len());
        }
    }

    #[test]
    fn simulated_draws_match_the_bytes_they_write() {
        let mut random = Random::new(5);
        for _ in 0..200 {
            let mut grid = CellGrid::new(60, 1, Rgb::new(40, 44, 52));
            text_row(
                &mut grid,
                0,
                0,
                &source_lines(1, random.next(1000) as u64)[0],
            );
            for cell in grid.row_mut(0) {
                if let PenColor::Color(color) = random_pen_color(&mut random) {
                    cell.fg = color;
                }
                if random.next(6) == 0 {
                    cell.bg = Rgb::new(50, 56, 66);
                }
                cell.attrs = random_attrs(&mut random) - CellAttrs::CURLY_UNDERLINE;
            }
            let mut terminal = Renderer::new(Vec::new(), 60, 1).terminal;
            terminal.cursor = (random.next(2) == 0).then(|| (random.next(60), 0));
            terminal.pen.fg = Some(random_pen_color(&mut random));
            let cells = grid.row(0).to_vec();
            let start = random.next(30);
            let state_start = (terminal.pen, terminal.cursor);
            let mut scratch = DrawScratch::default();
            terminal
                .plan_draw(&cells, start..60, None, &mut scratch)
                .unwrap();
            let mut state = DrawState {
                pen: terminal.pen,
                cursor: terminal.cursor,
            };
            assert!(scratch.segments.len() <= scratch.steps);
            let simulated: usize = scratch
                .segments
                .iter()
                .map(|segment| state.plan_segment(0, segment, false).0)
                .sum();
            for segment in &scratch.segments {
                terminal.draw_segment(0, segment, &scratch.text).unwrap();
            }
            assert_eq!(simulated, terminal.body.len());
            let mut reference = Renderer::new(Vec::new(), 60, 1).terminal;
            reference.cursor = state_start.1;
            reference.pen = state_start.0;
            for step in reference_steps(&cells, start..60, 60) {
                draw_reference_step(&mut reference, 0, &step);
            }
            assert_eq!(
                String::from_utf8_lossy(&reference.body),
                String::from_utf8_lossy(&terminal.body)
            );
            assert_eq!(state.pen, terminal.pen);
            assert_eq!(state.cursor, terminal.cursor);
        }
    }

    #[derive(Clone, Copy)]
    enum ReferenceKind {
        Erase(usize),
        Glyph(Glyph, usize),
    }

    #[derive(Clone, Copy)]
    struct ReferenceStep {
        col: usize,
        style: Style,
        kind: ReferenceKind,
    }

    fn reference_steps(cells: &[Cell], range: Range<usize>, cols: usize) -> Vec<ReferenceStep> {
        let visible_cols = cells.len().min(cols);
        let mut steps = Vec::new();
        let mut col = range.start;
        while col < range.end {
            let cell = cells[col];
            if cell.is_wide_continuation() {
                col += 1;
                continue;
            }
            let background = PenColor::background(&cell);
            let blank_run = cells[col..range.end]
                .iter()
                .take_while(|blank| is_blank_on(blank, background))
                .count();
            let min_run = if col + blank_run == range.end {
                MIN_ERASE_RUN_BEFORE_MOVE
            } else {
                MIN_ERASE_RUN
            };
            if blank_run >= min_run {
                steps.push(ReferenceStep {
                    col,
                    style: Style::blank(background),
                    kind: ReferenceKind::Erase(blank_run),
                });
                col += blank_run;
                continue;
            }
            let is_wide = cells
                .get(col + 1)
                .is_some_and(|next| next.is_wide_continuation());
            let (glyph, width) = match (is_wide, col + 2 <= visible_cols) {
                (true, true) => (cell.glyph, 2),
                (true, false) => (Glyph::from_char(' '), 1),
                (false, _) => (cell.glyph, 1),
            };
            steps.push(ReferenceStep {
                col,
                style: Style::of_cell(&cell, false),
                kind: ReferenceKind::Glyph(glyph, width),
            });
            col += 1;
        }
        steps
    }

    fn draw_reference_step(terminal: &mut Terminal<Vec<u8>>, row: u16, step: &ReferenceStep) {
        terminal.move_to(step.col, row).unwrap();
        terminal
            .pen
            .write_style(&mut terminal.body, &mut terminal.palette, step.style)
            .unwrap();
        match step.kind {
            ReferenceKind::Erase(count) => write!(terminal.body, "\x1b[{count}X").unwrap(),
            ReferenceKind::Glyph(glyph, width) => {
                glyph.write_to(&mut terminal.body).unwrap();
                let next_col = step.col + width;
                terminal.cursor = (next_col < terminal.cols as usize && glyph.as_char().is_some())
                    .then_some((next_col, row));
            }
        }
    }

    fn draw_by_encoding_both_orders(
        terminal: &mut Terminal<Vec<u8>>,
        row: u16,
        cells: &[Cell],
        range: Range<usize>,
    ) {
        let steps = reference_steps(cells, range, terminal.cols as usize);
        let start = terminal.body.len();
        let (pen, cursor) = (terminal.pen, terminal.cursor);
        for step in &steps {
            draw_reference_step(terminal, row, step);
        }
        if terminal.palette.enabled || steps.len() <= 2 {
            return;
        }
        let in_order = terminal.body.split_off(start);
        let in_order_end = (terminal.pen, terminal.cursor);
        (terminal.pen, terminal.cursor) = (pen, cursor);
        let joins = |terminal: &Terminal<Vec<u8>>, step: &ReferenceStep| {
            let is_blank_glyph =
                matches!(step.kind, ReferenceKind::Glyph(..)) && step.style.fg.is_none();
            terminal.pen.shows(&step.style)
                && (!is_blank_glyph || terminal.cursor == Some((step.col, row)))
        };
        let mut pending = steps;
        while !pending.is_empty() {
            if !pending.iter().any(|step| joins(terminal, step)) {
                let leftmost = pending.remove(0);
                draw_reference_step(terminal, row, &leftmost);
            }
            let mut deferred = Vec::new();
            for step in pending {
                if joins(terminal, &step) {
                    draw_reference_step(terminal, row, &step);
                } else {
                    deferred.push(step);
                }
            }
            pending = deferred;
        }
        if terminal.body.len() - start >= in_order.len() {
            terminal.body.truncate(start);
            terminal.body.extend_from_slice(&in_order);
            (terminal.pen, terminal.cursor) = in_order_end;
        }
    }

    #[test]
    fn planned_regrouping_matches_encoding_both_orders() {
        let palette = [
            Rgb::new(110, 180, 191),
            Rgb::new(178, 185, 198),
            Rgb::new(180, 119, 207),
            Rgb::new(9, 0, 255),
        ];
        let mut random = Random::new(13);
        let mut regrouped = 0;
        for _ in 0..400 {
            let mut grid = CellGrid::new(80, 1, Rgb::new(40, 44, 52));
            text_row(
                &mut grid,
                0,
                0,
                &source_lines(2, random.next(1000) as u64).concat(),
            );
            for cell in grid.row_mut(0) {
                cell.fg = palette[random.next(palette.len())];
                if random.next(8) == 0 {
                    cell.bg = Rgb::new(50, 56, 66);
                }
                cell.attrs = match random.next(12) {
                    0 => CellAttrs::BOLD,
                    1 => CellAttrs::UNDERLINE,
                    _ => CellAttrs::empty(),
                };
                if random.next(25) == 0 {
                    cell.attrs |= CellAttrs::DEFAULT_FOREGROUND;
                }
            }
            let cells = grid.row(0).to_vec();
            let pen = Pen {
                fg: Some(PenColor::Color(palette[random.next(palette.len())])),
                ..Pen::default()
            };
            let cursor = (random.next(3) != 0).then(|| (random.next(80), 0));
            let range = random.next(20)..80 - random.next(20);
            let mut planned = Renderer::new(Vec::new(), 80, 1).terminal;
            let mut encoded = Renderer::new(Vec::new(), 80, 1).terminal;
            for terminal in [&mut planned, &mut encoded] {
                terminal.pen = pen;
                terminal.cursor = cursor;
            }
            let planned_end = planned.draw(0, &cells, range.clone(), None).unwrap();
            draw_by_encoding_both_orders(&mut encoded, 0, &cells, range.clone());
            assert_eq!(planned_end, range.end);
            assert_eq!(
                String::from_utf8_lossy(&planned.body),
                String::from_utf8_lossy(&encoded.body)
            );
            assert_eq!((planned.pen, planned.cursor), (encoded.pen, encoded.cursor));
            let mut in_order = Renderer::new(Vec::new(), 80, 1).terminal;
            in_order.pen = pen;
            in_order.cursor = cursor;
            for step in reference_steps(&cells, range, 80) {
                draw_reference_step(&mut in_order, 0, &step);
            }
            regrouped += usize::from(planned.body.len() < in_order.body.len());
        }
        assert!(regrouped > 100, "{regrouped}");
    }

    #[test]
    fn palette_falls_back_to_truecolor_when_full_and_reuses_slots_after_a_clear() {
        let mut renderer = Renderer::new(Vec::new(), 40, 8);
        renderer.terminal.palette.enabled = true;
        let mut emulator = Emulator::new(40, 8);
        let mut grid = CellGrid::new(40, 8, Rgb::new(0, 0, 0));
        for (index, cell) in grid.cells.iter_mut().enumerate() {
            cell.glyph = 'x'.into();
            cell.fg = Rgb::new(index as u8, (index / 256) as u8, 7);
        }
        renderer.grid = Some(grid.clone());
        let bytes = emulator.feed(&mut renderer);
        assert!(bytes > 0);
        emulator.assert_shows(&grid, 40);
        assert_eq!(renderer.terminal.palette.slots.len(), 240);
        assert_eq!(
            renderer.terminal.palette.slots_used.load(Ordering::Relaxed),
            240
        );

        renderer.resize(40, 8);
        renderer.render().unwrap();
        let output = output_text(&renderer);
        assert!(output.contains("\x1b]4;16;rgb:00/00/07"), "{output:?}");
        assert!(output.contains("38;2;"), "{output:?}");
    }

    #[test]
    fn palette_slots_survive_a_resize_until_they_run_out() {
        let mut renderer = Renderer::new(Vec::new(), 60, 14);
        renderer.terminal.palette.enabled = true;
        let mut emulator = Emulator::new(60, 14);
        let grid = editor_frame(0);
        renderer.grid = Some(grid.clone());
        emulator.feed(&mut renderer);

        renderer.resize(60, 14);
        renderer.render().unwrap();
        let output = output_text(&renderer);
        assert!(!output.contains("\x1b]4;"), "{output:?}");
        emulator.feed(&mut renderer);
        emulator.assert_shows(&grid, 60);
    }

    fn parse_replies(chunks: &[&[u8]]) -> QueryReplies {
        let mut parser = vte::Parser::new();
        let mut replies = QueryReplies::default();
        for chunk in chunks {
            parser.advance(&mut replies, chunk);
        }
        replies
    }

    #[test]
    fn palette_replies_are_restored_exactly() {
        let reply = b"\x1b[?69;2$y\x1b]4;16;rgb:0000/0000/0000\x1b\\\x1b]4;17;rgb:0000/0000/5f5f\x07\x1b[?62;22c";
        let original = parse_replies(&[reply]).palette;
        assert_eq!(
            original.get(&16).map(String::as_str),
            Some("rgb:0000/0000/0000")
        );
        assert_eq!(
            original.get(&17).map(String::as_str),
            Some("rgb:0000/0000/5f5f")
        );
        assert_eq!(
            palette_restore(3, &original),
            "\x1b]4;16;rgb:0000/0000/0000;17;rgb:0000/0000/5f5f\x07\x1b]104;18\x07"
        );
        assert!(parse_replies(&[b"\x1b[?62;22c"]).palette.is_empty());
    }

    #[test]
    fn palette_restore_covers_every_used_slot_through_255() {
        let original = HashMap::from_iter([(255, "rgb:1111/2222/3333".to_owned())]);
        let restored_slots = |used| {
            let restore = palette_restore(used, &original);
            let mut slots = Vec::new();
            for sequence in restore
                .split('\x07')
                .filter(|sequence| !sequence.is_empty())
            {
                let params: Vec<&str> = sequence.split(';').collect();
                assert!(params.len() <= 16, "{sequence:?}");
                let step = if params[0] == "\x1b]4" { 2 } else { 1 };
                slots.extend(
                    params[1..]
                        .iter()
                        .step_by(step)
                        .map(|slot| slot.parse::<u8>().unwrap()),
                );
            }
            slots.sort();
            (restore, slots)
        };
        assert_eq!(restored_slots(0).1, Vec::<u8>::new());
        assert_eq!(restored_slots(1).1, vec![16]);
        assert_eq!(restored_slots(239).1, (16..=254).collect::<Vec<_>>());
        let (restore, slots) = restored_slots(PALETTE_SLOTS);
        assert_eq!(slots, (16..=255).collect::<Vec<_>>());
        assert!(restore.starts_with("\x1b]4;255;rgb:1111/2222/3333\x07\x1b]104;16;17;"));
        assert_eq!(
            signal_restore(&original, false, false),
            [restore.as_bytes(), CURSOR_SHAPE_RESET].concat()
        );
    }

    struct FailingWriter {
        written: Vec<u8>,
        flushes: usize,
        fail_on_flush: usize,
    }

    impl FailingWriter {
        fn failing_on_flush(fail_on_flush: usize) -> Self {
            Self {
                written: Vec::new(),
                flushes: 0,
                fail_on_flush,
            }
        }
    }

    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.flushes == self.fail_on_flush {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            Ok(())
        }
    }

    fn undo_after_setup(
        fail_on_flush: usize,
        query: impl FnOnce(&mut FailingWriter) -> io::Result<QueryReplies>,
    ) -> (io::Result<()>, String) {
        let mut setup = TerminalSetup::default();
        let result = setup.run(&mut FailingWriter::failing_on_flush(fail_on_flush), query);
        let mut undo = Vec::new();
        setup.undo(&mut undo);
        (result, String::from_utf8(undo).unwrap())
    }

    const LEAVE_ALTERNATE_SCREEN: &str = "\x1b[?1049l";
    const POP_KEYBOARD_FLAGS: &str = "\x1b[<1u";

    fn keyboard_replies(_: &mut FailingWriter) -> io::Result<QueryReplies> {
        Ok(QueryReplies {
            keyboard_flags: true,
            ..QueryReplies::default()
        })
    }

    #[test]
    fn a_failed_startup_undoes_only_the_steps_it_started() {
        let (result, undo) = undo_after_setup(1, keyboard_replies);
        assert!(result.is_err());
        assert!(undo.contains(LEAVE_ALTERNATE_SCREEN) && undo.ends_with(POP_TITLE));
        assert!(!undo.contains(POP_KEYBOARD_FLAGS));

        let (result, undo) = undo_after_setup(0, |_| Err(io::ErrorKind::TimedOut.into()));
        assert!(result.is_err());
        assert!(undo.contains(LEAVE_ALTERNATE_SCREEN) && undo.ends_with(POP_TITLE));
        assert!(!undo.contains(POP_KEYBOARD_FLAGS));

        let (result, undo) = undo_after_setup(2, keyboard_replies);
        assert!(result.is_err());
        assert!(undo.contains(LEAVE_ALTERNATE_SCREEN));
        assert!(!undo.contains(POP_KEYBOARD_FLAGS));
    }

    #[test]
    fn a_completed_startup_restores_every_step() {
        let (result, undo) = undo_after_setup(0, keyboard_replies);
        assert!(result.is_ok());
        assert!(undo.contains(POP_KEYBOARD_FLAGS));
        assert!(undo.contains(LEAVE_ALTERNATE_SCREEN) && undo.ends_with(POP_TITLE));
    }

    #[test]
    fn rectangle_copies_replace_scroll_regions_when_supported() {
        let mut renderer = Renderer::new(Vec::new(), 60, 14);
        renderer.terminal.features.rectangle_copy = true;
        let mut emulator = Emulator::new(60, 14);
        renderer.grid = Some(editor_frame(0));
        emulator.feed(&mut renderer);

        let scroll = find_scroll(&editor_frame(0), &editor_frame(3), 14, 60, true).unwrap();
        let columns = scroll.columns.unwrap();
        renderer.grid = Some(editor_frame(3));
        renderer.render().unwrap();
        let output = output_text(&renderer);
        let expected = format!("\x1b[5;1;13;{};1;2;1;1$v", columns.end);
        assert!(output.contains(&expected), "{output:?}");
        assert!(!output.contains("\x1b[2;13r"), "{output:?}");
        emulator.feed(&mut renderer);
        emulator.assert_shows(&editor_frame(3), 60);
    }

    #[test]
    fn split_panes_scroll_correctly_with_and_without_rectangle_copies() {
        let lines = source_lines(400, 5);
        let mut bytes_by_capability = Vec::new();
        for rectangle_copy in [false, true] {
            let mut random = Random::new(3);
            let mut renderer = Renderer::new(Vec::new(), 60, 14);
            renderer.terminal.features.rectangle_copy = rectangle_copy;
            let mut emulator = Emulator::new(60, 14);
            let (mut left, mut right) = (0, 0);
            let mut bytes = 0;
            for _ in 0..150 {
                left = (left + random.next(9)).saturating_sub(4).min(250);
                if random.next(2) == 0 {
                    right = (right + random.next(9)).saturating_sub(4).min(250);
                }
                let grid = split_panes(&lines, left, right);
                renderer.grid = Some(grid.clone());
                bytes += emulator.feed(&mut renderer);
                emulator.assert_shows(&grid, 60);
            }
            bytes_by_capability.push(bytes);
        }
        let [plain, rectangles] = bytes_by_capability[..] else {
            unreachable!()
        };
        assert!(rectangles < plain, "{bytes_by_capability:?}");
    }

    #[test]
    fn scrolls_from_the_server_are_drawn_without_a_second_search() {
        let mut encoder = crate::tui::protocol::FrameEncoder::default();
        let mut renderer = Renderer::new(Vec::new(), 60, 14);
        let mut emulator = Emulator::new(60, 14);
        let first = editor_frame(0);
        renderer
            .apply(&encoder.update(None, &first).unwrap())
            .unwrap();
        emulator.feed(&mut renderer);

        let second = editor_frame(3);
        let update = encoder.update(Some(&first), &second).unwrap();
        assert!(matches!(&update, ServerMessage::Diff(_, moves, _, _) if moves.len() == 1));
        renderer.apply(&update).unwrap();
        assert_eq!(renderer.server_moves.len(), 1);
        renderer.render().unwrap();
        let output = output_text(&renderer);
        assert!(output.contains("\x1b[2;13r\x1b[3S"), "{output:?}");
        emulator.feed(&mut renderer);
        emulator.assert_shows(&second, 60);
    }

    #[test]
    fn server_scrolls_the_terminal_would_ignore_are_redrawn_instead() {
        let ignored_scrolls = [
            GridScroll {
                top: 2,
                bottom: 3,
                shift: 1,
                columns: None,
            },
            GridScroll {
                top: 1,
                bottom: 13,
                shift: 3,
                columns: Some(5..6),
            },
        ];
        for scroll in ignored_scrolls {
            let uses_margins = scroll.columns.is_some();
            let mut renderer = Renderer::new(Vec::new(), 60, 14);
            renderer.terminal.features.left_right_margins = uses_margins;
            let mut emulator = Emulator::new(60, 14);
            renderer.grid = Some(editor_frame(0));
            emulator.feed(&mut renderer);

            renderer.grid = Some(editor_frame(3));
            renderer.frames_since_render = 1;
            renderer.server_moves = vec![scroll];
            renderer.render().unwrap();
            let output = output_text(&renderer);
            assert!(!output.contains("\x1b[3;3r"), "{output:?}");
            assert!(!output.contains("\x1b[6;6s"), "{output:?}");
            if !uses_margins {
                emulator.feed(&mut renderer);
                emulator.assert_shows(&editor_frame(3), 60);
            }
        }
    }

    #[test]
    fn clipboard_uses_osc52_with_st() {
        let mut renderer = Renderer::new(Vec::new(), 10, 2);
        renderer
            .apply(&ServerMessage::Clipboard("hi".into()))
            .unwrap();
        assert_eq!(renderer.terminal.output, b"\x1b]52;c;aGk=\x1b\\");
    }

    #[test]
    fn pointer_shapes_use_osc22_with_st_only_on_change() {
        let mut renderer = Renderer::new(Vec::new(), 10, 2);
        renderer.terminal.features.ghostty = true;
        for style in [
            CursorStyle::IBeam,
            CursorStyle::IBeam,
            CursorStyle::PointingHand,
        ] {
            renderer.apply(&ServerMessage::Pointer(style)).unwrap();
        }
        assert_eq!(
            renderer.terminal.output,
            b"\x1b]22;text\x1b\\\x1b]22;pointer\x1b\\"
        );
    }

    #[test]
    fn pointer_shapes_are_not_sent_to_other_terminals() {
        let mut renderer = Renderer::new(Vec::new(), 10, 2);
        renderer
            .apply(&ServerMessage::Pointer(CursorStyle::PointingHand))
            .unwrap();
        assert!(renderer.terminal.output.is_empty());
    }

    #[test]
    fn every_cursor_style_maps_to_a_ghostty_shape_name() {
        const GHOSTTY_W3C_SHAPES: [&str; 34] = [
            "default",
            "context-menu",
            "help",
            "pointer",
            "progress",
            "wait",
            "cell",
            "crosshair",
            "text",
            "vertical-text",
            "alias",
            "copy",
            "move",
            "no-drop",
            "not-allowed",
            "grab",
            "grabbing",
            "all-scroll",
            "col-resize",
            "row-resize",
            "n-resize",
            "e-resize",
            "s-resize",
            "w-resize",
            "ne-resize",
            "nw-resize",
            "se-resize",
            "sw-resize",
            "ew-resize",
            "ns-resize",
            "nesw-resize",
            "nwse-resize",
            "zoom-in",
            "zoom-out",
        ];
        let styles = [
            CursorStyle::Arrow,
            CursorStyle::IBeam,
            CursorStyle::Crosshair,
            CursorStyle::ClosedHand,
            CursorStyle::OpenHand,
            CursorStyle::PointingHand,
            CursorStyle::ResizeLeft,
            CursorStyle::ResizeRight,
            CursorStyle::ResizeLeftRight,
            CursorStyle::ResizeUp,
            CursorStyle::ResizeDown,
            CursorStyle::ResizeUpDown,
            CursorStyle::ResizeUpLeftDownRight,
            CursorStyle::ResizeUpRightDownLeft,
            CursorStyle::ResizeColumn,
            CursorStyle::ResizeRow,
            CursorStyle::IBeamCursorForVerticalLayout,
            CursorStyle::OperationNotAllowed,
            CursorStyle::DragLink,
            CursorStyle::DragCopy,
            CursorStyle::ContextualMenu,
        ];
        for style in styles {
            assert!(
                GHOSTTY_W3C_SHAPES.contains(&pointer_name(style)),
                "{style:?} maps to {:?}",
                pointer_name(style)
            );
        }
    }

    #[test]
    fn up_left_down_right_resizes_use_the_nwse_shape() {
        assert_eq!(
            pointer_name(CursorStyle::ResizeUpLeftDownRight),
            "nwse-resize"
        );
        assert_eq!(
            pointer_name(CursorStyle::ResizeUpRightDownLeft),
            "nesw-resize"
        );
    }

    #[test]
    fn ghostty_pointer_is_reset_on_exit_and_on_signal() {
        let contains_reset = |bytes: &[u8]| {
            bytes
                .windows(POINTER_RESET.len())
                .any(|window| window == POINTER_RESET)
        };
        for ghostty in [true, false] {
            let mut setup = TerminalSetup::default();
            setup
                .run(&mut FailingWriter::failing_on_flush(0), |_| {
                    Ok(QueryReplies {
                        features: TerminalFeatures {
                            ghostty,
                            ..TerminalFeatures::default()
                        },
                        ..QueryReplies::default()
                    })
                })
                .unwrap();
            let mut undo = Vec::new();
            setup.undo(&mut undo);
            assert_eq!(contains_reset(&undo), ghostty, "{undo:?}");
            let restore = signal_restore(
                &setup.original_palette,
                setup.features.margin_mode,
                setup.features.ghostty,
            );
            assert_eq!(contains_reset(&restore), ghostty, "{restore:?}");
        }
    }

    #[test]
    fn keyboard_flags_come_from_the_flags_report() {
        assert!(parse_replies(&[b"\x1b[?0u\x1b[?62;22c"]).keyboard_flags);
        assert!(parse_replies(&[b"\x1b[?69;2$y\x1b[?15u\x1b[?62c"]).keyboard_flags);
        assert!(!parse_replies(&[b"\x1b[?62;22c"]).keyboard_flags);
    }

    #[test]
    fn margin_support_comes_from_the_mode_report() {
        assert!(parse_replies(&[b"\x1b[?62;22c"]).device_attributes);
        assert!(parse_replies(&[b"\x1b[?64;1;28c"]).features.rectangle_copy);
        assert!(!parse_replies(&[b"\x1b[?28;1c"]).features.rectangle_copy);
        assert!(!parse_replies(&[b"\x1b[?62;22c"]).features.rectangle_copy);
        assert!(!parse_replies(&[b"\x1b[?69;2$y"]).device_attributes);
        assert!(
            parse_replies(&[b"\x1b[?69;2$y\x1b[?62;22c"])
                .features
                .left_right_margins
        );
        assert!(
            parse_replies(&[b"\x1b[?69;1$y\x1b[?1;2c"])
                .features
                .left_right_margins
        );
        assert!(
            !parse_replies(&[b"\x1b[?69;0$y\x1b[?62;22c"])
                .features
                .left_right_margins
        );
        assert!(
            !parse_replies(&[b"\x1b[?62;22c"])
                .features
                .left_right_margins
        );
    }

    #[test]
    fn ghostty_comes_from_the_version_report() {
        assert!(
            parse_replies(&[b"\x1bP>|ghostty 1.2.0\x1b\\\x1b[?62;22c"])
                .features
                .ghostty
        );
        assert!(
            parse_replies(&[b"\x1bP>|libghostty\x1b\\\x1b[?62;22c"])
                .features
                .ghostty
        );
        assert!(
            !parse_replies(&[b"\x1bP>|XTerm(390)\x1b\\\x1b[?64;1;28c"])
                .features
                .ghostty
        );
        assert!(
            !parse_replies(&[b"\x1bP>|tmux 3.5a\x1b\\\x1b[?62;22c"])
                .features
                .ghostty
        );
        assert!(
            !parse_replies(&[b"\x1bP1$r0m\x1b\\ghostty\x1b[?62;22c"])
                .features
                .ghostty
        );
        assert!(!parse_replies(&[b"\x1b[?62;22c"]).features.ghostty);
    }

    #[test]
    fn replies_split_across_reads_are_parsed() {
        let replies = parse_replies(&[
            b"\x1b[?69;",
            b"2$y\x1b]4;16;rgb:00",
            b"00/0000/0000\x1b",
            b"\\\x1b[?1u\x1bP>|gho",
            b"stty 1.2\x1b\\\x1b[?62",
            b";22c",
        ]);
        assert!(replies.features.left_right_margins);
        assert!(replies.keyboard_flags);
        assert!(replies.features.ghostty);
        assert!(replies.device_attributes);
        assert_eq!(
            replies.palette.get(&16).map(String::as_str),
            Some("rgb:0000/0000/0000")
        );
    }

    fn shortest_move(from: Option<(usize, u16)>, col: usize, row: u16) -> String {
        let mut output = Vec::new();
        write_shortest_move(&mut output, from, col, row).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn closing_the_terminal_is_noticed_even_with_input_pending() {
        let (mut master, mut slave) = (0, 0);
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0, "{}", io::Error::last_os_error());
        unsafe { libc::write(master, b"x".as_ptr().cast(), 1) };
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || sender.send(wait_for_hangup(slave)).ok());
        assert!(receiver.recv_timeout(Duration::from_millis(300)).is_err());

        unsafe { libc::close(master) };
        assert_eq!(receiver.recv_timeout(Duration::from_secs(5)), Ok(true));
        unsafe { libc::close(slave) };
    }

    #[test]
    fn cursor_moves_pick_the_shortest_sequence() {
        assert_eq!(shortest_move(None, 4, 2), "\x1b[3;5H");
        assert_eq!(shortest_move(Some((9, 2)), 0, 3), "\n\r");
        assert_eq!(shortest_move(Some((9, 2)), 9, 3), "\n");
        assert_eq!(shortest_move(Some((9, 2)), 8, 2), "\x08");
        assert_eq!(shortest_move(Some((9, 2)), 10, 2), "\x1b[C");
        assert_eq!(shortest_move(Some((9, 2)), 40, 2), "\x1b[41G");
        assert_eq!(shortest_move(Some((9, 30)), 9, 2), "\x1b[3d");
        assert_eq!(shortest_move(None, 0, 0), "\x1b[H");
        assert_eq!(shortest_move(None, 4, 0), "\x1b[;5H");
        assert_eq!(shortest_move(Some((9, 12)), 0, 17), "\x1b[5E");
        assert_eq!(shortest_move(Some((9, 20)), 0, 12), "\x1b[8F");
    }

    #[test]
    fn resets_omit_the_zero_parameter() {
        let mut palette = Palette::default();
        let mut pen = Pen::default();
        let mut output = Vec::new();
        let (red, blue) = (Rgb::new(200, 0, 0), Rgb::new(0, 0, 200));
        let mut write = |pen: &mut Pen, fg, bg, attrs| {
            output.clear();
            pen.write_style(
                &mut output,
                &mut palette,
                Style {
                    fg: Some(fg),
                    bg,
                    underline: None,
                    attrs,
                    relevant: CellAttrs::all(),
                },
            )
            .unwrap();
            String::from_utf8(output.clone()).unwrap()
        };
        let all = CellAttrs::BOLD | CellAttrs::ITALIC | CellAttrs::UNDERLINE;
        write(&mut pen, PenColor::Color(red), PenColor::Color(blue), all);
        assert_eq!(
            write(
                &mut pen,
                PenColor::Color(blue),
                PenColor::Default,
                CellAttrs::BOLD
            ),
            "\x1b[;1;38;2;0;0;200m"
        );
        assert_eq!(
            write(
                &mut pen,
                PenColor::Default,
                PenColor::Default,
                CellAttrs::empty()
            ),
            "\x1b[m"
        );
    }

    #[test]
    fn whole_screen_scrolls_skip_the_scroll_region() {
        let mut terminal = Renderer::new(Vec::new(), 20, 6).terminal;
        terminal.cursor = Some((3, 4));
        let scroll = |top, bottom| GridScroll {
            top,
            bottom,
            shift: 1,
            columns: None,
        };
        terminal.scroll(&scroll(0, 6)).unwrap();
        assert_eq!(String::from_utf8(terminal.body.clone()).unwrap(), "\x1b[S");
        assert_eq!(terminal.cursor, Some((3, 4)));
        terminal.body.clear();
        terminal.scroll(&scroll(1, 6)).unwrap();
        assert_eq!(
            String::from_utf8(terminal.body.clone()).unwrap(),
            "\x1b[2;6r\x1b[S\x1b[r"
        );
        assert_eq!(terminal.cursor, Some((0, 0)));
    }

    #[test]
    fn rendered_frames_match_an_emulated_terminal() {
        let mut shifted_rows = false;
        let mut repeated = false;
        let mut curled = false;
        let mut colored_underlines = false;
        let cases = [(1, 40), (2, 40), (3, 33), (4, 27), (5, 44), (6, 40)];
        for (ghostty, (seed, terminal_cols)) in [false, true]
            .into_iter()
            .flat_map(|ghostty| cases.map(|case| (ghostty, case)))
        {
            let mut random = Random::new(seed);
            let mut grid = CellGrid::new(40, 12, Rgb::new(40, 44, 52));
            let mut renderer = Renderer::new(Vec::new(), terminal_cols, 12);
            renderer.terminal.features.ghostty = ghostty;
            renderer.terminal.palette.enabled = seed % 2 == 1;
            if seed % 3 == 0 {
                renderer.terminal.palette = Palette::new(
                    &reported_palette(&[(3, "rgb:c8/78/3c")]),
                    Default::default(),
                );
            }
            let mut emulator = Emulator::new(terminal_cols, 12);
            emulator.styled_underlines = ghostty;
            paint_run(&mut grid, &mut random);
            mutate(&mut grid, &mut random, 60);
            renderer.grid = Some(grid.clone());
            emulator.feed(&mut renderer);
            emulator.assert_shows(&grid, terminal_cols);
            for step in 0..20 {
                match step % 3 {
                    0 => shift_rows(&mut grid, &mut random),
                    1 => shift_within_rows(&mut grid, &mut random),
                    _ => {}
                }
                paint_run(&mut grid, &mut random);
                mutate(&mut grid, &mut random, 3);
                renderer.grid = Some(grid.clone());
                renderer.render().unwrap();
                let output = String::from_utf8_lossy(&renderer.terminal.output);
                shifted_rows |= output.contains('@') || output.contains("\x1b[P");
                let repeats = repeats_in(&output);
                let styled = output.contains("4:3") || output.contains(";58;");
                assert!(ghostty || (repeats == 0 && !styled), "{output:?}");
                repeated |= repeats > 0;
                curled |= output.contains("4:3");
                colored_underlines |= output.contains(";58;") || output.contains("[58;");
                emulator.feed(&mut renderer);
                emulator.assert_shows(&grid, terminal_cols);
            }
        }
        assert!(shifted_rows);
        assert!(repeated && curled && colored_underlines);
    }

    fn underlined_output(ghostty: bool) -> String {
        let red = Rgb::new(224, 108, 117);
        let mut grid = CellGrid::new(12, 1, Rgb::new(40, 44, 52));
        text_row(&mut grid, 0, 0, "abcdefgh");
        for (col, cell) in grid.row_mut(0).iter_mut().enumerate().take(8) {
            cell.fg = Rgb::new(200, 200, 200);
            cell.attrs = match col {
                0..2 => CellAttrs::UNDERLINE | CellAttrs::CURLY_UNDERLINE,
                2..4 => CellAttrs::UNDERLINE,
                4..6 => CellAttrs::UNDERLINE | CellAttrs::CURLY_UNDERLINE,
                _ => CellAttrs::empty(),
            };
            if col < 4 {
                cell.underline = UnderlineColor::of(red);
            }
        }
        let mut renderer = Renderer::new(Vec::new(), 12, 1);
        renderer.terminal.features.ghostty = ghostty;
        let mut emulator = Emulator::new(12, 1);
        emulator.styled_underlines = ghostty;
        render_checked(&mut renderer, &mut emulator, &grid)
    }

    #[test]
    fn ghostty_underlines_carry_their_curl_and_color() {
        let output = underlined_output(true);
        assert!(
            output.contains("4:3;38;2;200;200;200;48;2;40;44;52;58;2;224;108;117mab\x1b[4mcd\x1b[4:3;59mef\x1b[24mgh"),
            "{output:?}"
        );
        let output = underlined_output(false);
        assert!(
            output.contains("\x1b[4;38;2;200;200;200;48;2;40;44;52mabcdef\x1b[24mgh"),
            "{output:?}"
        );
    }

    fn paint_run(grid: &mut CellGrid, random: &mut Random) {
        let glyphs = ['─', '=', 'a', WIDE[0]];
        let attrs = [
            CellAttrs::empty(),
            CellAttrs::BOLD,
            CellAttrs::UNDERLINE,
            CellAttrs::UNDERLINE | CellAttrs::CURLY_UNDERLINE,
        ];
        let run = Cell {
            glyph: glyphs[random.next(glyphs.len())].into(),
            fg: Rgb::new(200, 120, 60),
            bg: Rgb::new(30, 33, 40),
            attrs: attrs[random.next(attrs.len())],
            underline: UnderlineColor::of(Rgb::new(224, 108, 117)),
        };
        let row = random.next(grid.rows as usize) as i32;
        let start = random.next(grid.cols as usize) as i32;
        for col in start..start + 2 + random.next(24) as i32 {
            if let Some(cell) = grid.cell_mut(col, row) {
                *cell = run;
            }
        }
    }

    fn repeats_in(output: &str) -> usize {
        output
            .split("\x1b[")
            .skip(1)
            .filter(|sequence| {
                let digits = sequence.bytes().take_while(u8::is_ascii_digit).count();
                sequence.as_bytes().get(digits) == Some(&b'b')
            })
            .count()
    }

    fn rendered_text(cols: u16, text: &str, ghostty: bool) -> String {
        let mut grid = CellGrid::new(cols, 1, Rgb::new(40, 44, 52));
        text_row(&mut grid, 0, 0, text);
        let mut renderer = Renderer::new(Vec::new(), cols, 1);
        renderer.terminal.features.ghostty = ghostty;
        let mut emulator = Emulator::new(cols, 1);
        render_checked(&mut renderer, &mut emulator, &grid)
    }

    #[test]
    fn ghostty_repeats_runs_only_when_shorter() {
        let border = format!("┌{}┐", "─".repeat(28));
        assert!(rendered_text(40, &border, true).contains("─\x1b[27b┐"));
        assert_eq!(repeats_in(&rendered_text(40, &border, false)), 0);
        let output = rendered_text(40, "x======y=====z", true);
        assert!(output.contains("x=\x1b[5by====="), "{output:?}");
        assert_eq!(repeats_in(&rendered_text(40, "ab───cd", true)), 1);
        assert_eq!(repeats_in(&rendered_text(40, "ab──cd", true)), 0);
        let full_width = "=".repeat(40);
        assert!(rendered_text(40, &full_width, true).contains("=\x1b[39b\x1b[?2026l"));
    }

    #[test]
    fn ghostty_repeats_only_characters_that_start_their_own_cell() {
        assert_eq!(
            repeats_in(&rendered_text(40, "x\u{93e}\u{93e}\u{93e}\u{93e}y", true)),
            0
        );
        assert_eq!(repeats_in(&rendered_text(40, "x▀▀▀▀y", true)), 1);
    }

    #[test]
    fn recolored_underlines_are_redrawn() {
        let red = Rgb::new(224, 108, 117);
        let mut screen = CellGrid::new(3, 1, Rgb::new(40, 44, 52));
        for cell in screen.row_mut(0) {
            cell.glyph = 'x'.into();
            cell.fg = Rgb::new(200, 200, 200);
            cell.attrs = CellAttrs::UNDERLINE | CellAttrs::CURLY_UNDERLINE;
            cell.underline = UnderlineColor::of(red);
        }
        screen.row_mut(0)[2].attrs = CellAttrs::empty();
        forget_cells_drawn_with(&mut screen, &[red]);
        let row = screen.row(0);
        assert_eq!((row[0], row[1]), (unknown_cell(), unknown_cell()));
        assert_eq!(row[2].glyph, 'x');
    }

    #[test]
    fn ghostty_does_not_repeat_clusters() {
        let mut grid = CellGrid::new(20, 1, Rgb::new(40, 44, 52));
        for cell in grid.row_mut(0).iter_mut().take(8) {
            cell.glyph = Glyph::from_cluster("e\u{301}");
        }
        let mut renderer = Renderer::new(Vec::new(), 20, 1);
        renderer.terminal.features.ghostty = true;
        let mut emulator = Emulator::new(20, 1);
        renderer.grid = Some(grid.clone());
        renderer.render().unwrap();
        assert_eq!(repeats_in(&output_text(&renderer)), 0);
        emulator.feed(&mut renderer);
        emulator.assert_shows(&grid, 20);
    }

    #[test]
    fn typing_mid_line_shifts_the_rest_of_the_line() {
        let mut grid = CellGrid::new(80, 3, Rgb::new(40, 44, 52));
        let text = "    let measured_value = compute_bytes(first, second, third);";
        text_row(&mut grid, 1, 0, text);
        for (col, cell) in grid.row_mut(1).iter_mut().enumerate() {
            cell.fg = Rgb::new(100 + (col / 5) as u8 * 10, 120, 60);
        }
        if let Some(cell) = grid.cell_mut(79, 1) {
            cell.glyph = '│'.into();
        }
        let mut renderer = Renderer::new(Vec::new(), 80, 3);
        let mut emulator = Emulator::new(80, 3);
        renderer.grid = Some(grid.clone());
        emulator.feed(&mut renderer);

        let mut typed = grid.clone();
        let row = typed.row_mut(1);
        row[8..79].rotate_right(1);
        row[8].glyph = 'x'.into();
        renderer.grid = Some(typed.clone());
        renderer.render().unwrap();
        let output = output_text(&renderer);
        assert!(output.contains("\x1b[@"), "{output:?}");
        assert!(
            output.len() < 96,
            "typing took {} bytes: {output:?}",
            output.len()
        );
        emulator.feed(&mut renderer);
        emulator.assert_shows(&typed, 80);

        renderer.grid = Some(grid.clone());
        renderer.render().unwrap();
        let output = output_text(&renderer);
        assert!(output.contains("\x1b[P"), "{output:?}");
        emulator.feed(&mut renderer);
        emulator.assert_shows(&grid, 80);
    }

    #[test]
    fn shifting_a_highlighted_line_erases_with_its_background() {
        let editor = Rgb::new(40, 44, 52);
        let mut grid = CellGrid::new(80, 3, editor);
        text_row(
            &mut grid,
            1,
            0,
            "    let measured_value = compute_bytes(first, second, third);",
        );
        for cell in grid.row_mut(1) {
            cell.bg = Rgb::new(50, 56, 66);
        }
        grid.mark_default_colors(&[editor], &[]);
        let mut renderer = Renderer::new(Vec::new(), 80, 3);
        let mut emulator = Emulator::new(80, 3);
        renderer.grid = Some(grid.clone());
        emulator.feed(&mut renderer);

        let mut typed = grid.clone();
        let row = typed.row_mut(1);
        row[8..].rotate_right(1);
        row[8].glyph = 'x'.into();
        renderer.grid = Some(typed.clone());
        renderer.render().unwrap();
        let output = output_text(&renderer);
        assert!(output.contains("\x1b[@"), "{output:?}");
        assert!(
            !output.contains("49m") && !output.contains("\x1b[m"),
            "{output:?}"
        );
        emulator.feed(&mut renderer);
        emulator.assert_shows(&typed, 80);

        renderer.grid = Some(grid.clone());
        renderer.render().unwrap();
        let output = output_text(&renderer);
        assert!(output.contains("\x1b[P"), "{output:?}");
        assert!(!output.contains(' '), "{output:?}");
        emulator.feed(&mut renderer);
        emulator.assert_shows(&grid, 80);
    }

    fn rendered_change(cols: u16, before: &str, after: &str) -> String {
        let mut grid = CellGrid::new(cols, 2, Rgb::new(40, 44, 52));
        text_row(&mut grid, 0, 0, before);
        let mut renderer = Renderer::new(Vec::new(), cols, 2);
        let mut emulator = Emulator::new(cols, 2);
        renderer.grid = Some(grid);
        emulator.feed(&mut renderer);

        let mut changed = CellGrid::new(cols, 2, Rgb::new(40, 44, 52));
        text_row(&mut changed, 0, 0, after);
        render_checked(&mut renderer, &mut emulator, &changed)
    }

    #[test]
    fn blank_tails_are_erased_to_the_end_of_the_line() {
        let output = rendered_change(20, "abcdefghij", "ab");
        assert!(
            output.contains("\x1b[K") && !output.contains('X'),
            "{output:?}"
        );
        let output = rendered_change(20, "abcdefghijklmnopqrst", "abcdefghijklmnopqr");
        assert!(!output.contains("\x1b[K"), "{output:?}");
    }

    #[test]
    fn blank_runs_ending_a_range_use_erase_characters() {
        let output = rendered_change(20, "abcdefghijklmnopqrs|", "ab      ijklmnopqrs|");
        assert!(output.contains("\x1b[6X"), "{output:?}");
    }

    #[test]
    fn combining_marks_reach_the_terminal_with_their_base() {
        let mut grid = CellGrid::new(20, 2, Rgb::new(40, 44, 52));
        text_row(&mut grid, 0, 0, "cafe x");
        if let Some(cell) = grid.cell_mut(3, 0) {
            cell.glyph = Glyph::from_cluster("e\u{301}");
        }
        let mut renderer = Renderer::new(Vec::new(), 20, 2);
        let mut emulator = Emulator::new(20, 2);
        renderer.grid = Some(grid.clone());
        emulator.feed(&mut renderer);
        emulator.assert_shows(&grid, 20);
    }

    #[test]
    fn clusters_the_terminal_splits_do_not_shift_later_cells() {
        let mut grid = CellGrid::new(20, 2, Rgb::new(40, 44, 52));
        if let Some(cell) = grid.cell_mut(0, 0) {
            cell.glyph = Glyph::from_cluster("👩\u{200d}💻");
        }
        if let Some(cell) = grid.cell_mut(1, 0) {
            cell.attrs = CellAttrs::WIDE_CONTINUATION;
        }
        text_row(&mut grid, 0, 2, "XY");
        let mut renderer = Renderer::new(Vec::new(), 20, 2);
        let mut emulator = Emulator::new(20, 2);
        renderer.grid = Some(grid);
        emulator.feed(&mut renderer);
        let shown = |col: usize| emulator.term.grid()[Line(0)][Column(col)].c;
        assert_eq!((shown(2), shown(3)), ('X', 'Y'));
    }

    #[test]
    fn default_background_cells_use_sgr_49() {
        let editor = Rgb::new(40, 44, 52);
        let mut grid = CellGrid::new(20, 2, editor);
        if let Some(cell) = grid.cell_mut(3, 0) {
            cell.glyph = 'x'.into();
        }
        if let Some(cell) = grid.cell_mut(0, 1) {
            cell.bg = Rgb::new(200, 120, 60);
        }
        grid.mark_default_colors(&[editor], &[]);
        let mut renderer = Renderer::new(Vec::new(), 20, 2);
        let mut emulator = Emulator::new(20, 2);
        renderer.grid = Some(grid.clone());
        renderer.render().unwrap();
        let output = output_text(&renderer);
        assert!(!output.contains("48;2;40;44;52"), "{output:?}");
        assert!(output.contains("48;2;200;120;60"), "{output:?}");
        emulator.feed(&mut renderer);
        emulator.assert_shows(&grid, 20);

        if let Some(cell) = grid.cell_mut(0, 1) {
            cell.bg = editor;
        }
        grid.mark_default_colors(&[editor], &[]);
        renderer.grid = Some(grid.clone());
        renderer.render().unwrap();
        let output = output_text(&renderer);
        assert!(
            output.contains("\x1b[49m") || output.contains("\x1b[m"),
            "{output:?}"
        );
        assert!(!output.contains("48;2;40;44;52"), "{output:?}");
        emulator.feed(&mut renderer);
        emulator.assert_shows(&grid, 20);
    }

    #[test]
    fn small_frames_skip_synchronized_update() {
        let mut grid = CellGrid::new(80, 24, Rgb::new(40, 44, 52));
        mutate(&mut grid, &mut Random::new(3), 40);
        let mut renderer = Renderer::new(Vec::new(), 80, 24);
        renderer.grid = Some(grid.clone());
        renderer.render().unwrap();
        let output = output_text(&renderer);
        assert!(output.starts_with("\x1b[?2026h") && output.ends_with("\x1b[?2026l"));

        renderer.terminal.output.clear();
        if let Some(cell) = grid.cell_mut(10, 5) {
            cell.glyph = 'x'.into();
        }
        renderer.grid = Some(grid);
        renderer.render().unwrap();
        let output = output_text(&renderer);
        assert!(!output.is_empty() && !output.contains("2026"), "{output:?}");
    }

    #[test]
    fn palette_definitions_are_batched_before_the_frame() {
        let mut renderer = Renderer::new(Vec::new(), 60, 14);
        renderer.terminal.palette.enabled = true;
        let mut emulator = Emulator::new(60, 14);
        let mut grid = editor_frame(0);
        for (index, cell) in grid.cells.iter_mut().enumerate() {
            cell.fg = Rgb::new((index % 20) as u8 * 10, 120, 60);
        }
        renderer.grid = Some(grid.clone());
        renderer.render().unwrap();
        let output = output_text(&renderer);
        let body = output.strip_prefix("\x1b[?2026h").unwrap();
        let definitions: Vec<&str> = body
            .split('\x07')
            .take_while(|sequence| sequence.starts_with("\x1b]4;"))
            .collect();
        let slots = renderer.terminal.palette.slots.len();
        assert_eq!(definitions.len(), slots.div_ceil(OSC_PALETTE_PAIRS));
        assert!(
            definitions
                .iter()
                .all(|sequence| sequence.split(';').count() <= 1 + 2 * OSC_PALETTE_PAIRS)
        );
        emulator.feed(&mut renderer);
        emulator.assert_shows(&grid, 60);
    }

    fn reported_palette(ansi: &[(u8, &str)]) -> HashMap<u8, String> {
        let mut original: HashMap<u8, String> = (0..=u8::MAX)
            .map(|slot| (slot, "rgb:0000/0000/0000".to_owned()))
            .collect();
        for (slot, spec) in ansi {
            original.insert(*slot, (*spec).to_owned());
        }
        original
    }

    fn assigned(palette: &Palette, color: Rgb) -> Option<u8> {
        palette.ansi.get(&color).copied()
    }

    fn rebalance_after_uses(palette: &mut Palette, uses: &[(Rgb, u32)]) -> Vec<Rgb> {
        for (color, count) in uses {
            for _ in 0..count * MIN_ANSI_SWITCH_USES {
                palette.count_use(PenColor::Color(*color));
            }
        }
        palette.rebalance_ansi()
    }

    #[test]
    fn rgb_specs_parse_at_any_precision() {
        assert_eq!(
            parse_rgb_spec("rgb:ffff/8080/0000"),
            Some(Rgb::new(255, 128, 0))
        );
        assert_eq!(parse_rgb_spec("rgb:f/8/0"), Some(Rgb::new(255, 136, 0)));
        assert_eq!(parse_rgb_spec("rgb:ff/80"), None);
        assert_eq!(parse_rgb_spec("#ff8000"), None);
    }

    #[test]
    fn most_used_grays_take_the_ansi_gray_slots_in_lightness_order() {
        let mut palette = Palette::new(&reported_palette(&[]), Default::default());
        let (darkest, dark, light, lightest, rare) = (
            Rgb::new(30, 33, 40),
            Rgb::new(78, 90, 95),
            Rgb::new(178, 185, 198),
            Rgb::new(220, 224, 229),
            Rgb::new(120, 120, 120),
        );
        rebalance_after_uses(
            &mut palette,
            &[
                (light, 9),
                (lightest, 8),
                (darkest, 7),
                (dark, 6),
                (rare, 1),
            ],
        );
        assert_eq!(assigned(&palette, darkest), Some(0));
        assert_eq!(assigned(&palette, dark), Some(8));
        assert_eq!(assigned(&palette, light), Some(7));
        assert_eq!(assigned(&palette, lightest), Some(15));
        assert_eq!(assigned(&palette, rare), None);
        assert_eq!(palette.definitions.len(), 4);
    }

    #[test]
    fn near_matching_colors_redefine_the_stock_slot_with_the_same_meaning() {
        let mut palette = Palette::new(
            &reported_palette(&[
                (1, "rgb:e0/6c/75"),
                (2, "rgb:98/c3/79"),
                (4, "rgb:61/af/ef"),
                (5, "rgb:c6/78/dd"),
            ]),
            Default::default(),
        );
        let (red, green, blue, purple) = (
            Rgb::new(208, 114, 119),
            Rgb::new(161, 193, 129),
            Rgb::new(115, 173, 233),
            Rgb::new(180, 119, 207),
        );
        rebalance_after_uses(
            &mut palette,
            &[(red, 4), (green, 3), (blue, 2), (purple, 1)],
        );
        assert_eq!(assigned(&palette, red), Some(1));
        assert_eq!(assigned(&palette, green), Some(2));
        assert_eq!(assigned(&palette, blue), Some(4));
        assert_eq!(assigned(&palette, purple), Some(5));
        assert!(palette.definitions.contains(&(1, red)));
    }

    #[test]
    fn colors_without_a_near_match_stay_in_high_slots() {
        let mut palette = Palette::new(
            &reported_palette(&[(1, "rgb:e0/6c/75"), (4, "rgb:61/af/ef")]),
            Default::default(),
        );
        let orange = Rgb::new(191, 149, 106);
        let green = Rgb::new(161, 193, 129);
        rebalance_after_uses(&mut palette, &[(orange, 2), (green, 1)]);
        assert_eq!(assigned(&palette, orange), None);
        assert_eq!(assigned(&palette, green), None);
        assert_eq!(palette.slot_for(orange), Some(FIRST_PALETTE_SLOT));
    }

    #[test]
    fn grays_stay_in_high_slots_when_the_terminal_did_not_report_them() {
        let mut original = reported_palette(&[]);
        for slot in GRAY_SLOTS_BY_LIGHTNESS {
            original.remove(&slot);
        }
        let mut palette = Palette::new(&original, Default::default());
        let gray = Rgb::new(178, 185, 198);
        rebalance_after_uses(&mut palette, &[(gray, 3)]);
        assert_eq!(assigned(&palette, gray), None);
    }

    #[test]
    fn every_reported_ansi_slot_is_restored_on_exit_and_signal() {
        let original = reported_palette(&[(3, "rgb:e5/c0/7b")]);
        let restore = palette_restore(0, &original);
        for slot in 0..FIRST_PALETTE_SLOT {
            assert!(
                restore.contains(&format!(";{slot};rgb:")),
                "slot {slot} missing from {restore:?}"
            );
        }
        assert!(restore.contains(";3;rgb:e5/c0/7b"));
        assert!(!restore.contains(";16;"));
        assert_eq!(
            signal_restore(&original, false, false),
            [
                palette_restore(PALETTE_SLOTS, &original).as_bytes(),
                CURSOR_SHAPE_RESET
            ]
            .concat()
        );
    }

    #[test]
    fn ansi_slots_render_with_short_codes() {
        let mut renderer = Renderer::new(Vec::new(), 60, 14);
        renderer.terminal.palette = Palette::new(
            &reported_palette(&[(1, "rgb:e0/6c/75")]),
            Default::default(),
        );
        let mut emulator = Emulator::new(60, 14);
        let mut grid = editor_frame(0);
        for (index, cell) in grid.cells.iter_mut().enumerate() {
            if index % 3 == 0 {
                cell.fg = Rgb::new(208, 114, 119);
            }
        }
        renderer.grid = Some(grid.clone());
        emulator.feed(&mut renderer);
        renderer.resize(60, 14);
        renderer.render().unwrap();
        let output = output_text(&renderer);
        assert!(output.contains(";1;rgb:d0/72/77"), "{output:?}");
        assert!(
            output.contains("\x1b[31m") || output.contains(";31m"),
            "{output:?}"
        );
        assert!(!output.contains("38;5;"), "{output:?}");
        emulator.feed(&mut renderer);
        emulator.assert_shows(&grid, 60);
    }

    #[test]
    fn reassigning_a_slot_redraws_cells_drawn_with_its_old_color() {
        let mut renderer = Renderer::new(Vec::new(), 60, 14);
        renderer.terminal.palette = Palette::new(&reported_palette(&[]), Default::default());
        let mut emulator = Emulator::new(60, 14);
        let early = [Rgb::new(120, 120, 120), Rgb::new(255, 255, 255)];
        let late = [
            Rgb::new(30, 30, 30),
            Rgb::new(60, 60, 60),
            Rgb::new(200, 200, 200),
            Rgb::new(230, 230, 230),
        ];
        let mut grid = CellGrid::new(60, 14, Rgb::new(0, 0, 0));
        for (index, cell) in grid.cells.iter_mut().enumerate().take(6 * 60) {
            cell.glyph = 'a'.into();
            cell.fg = early[index % early.len()];
        }
        renderer.grid = Some(grid.clone());
        emulator.feed(&mut renderer);
        emulator.feed(&mut renderer);
        assert!(
            early
                .iter()
                .all(|color| renderer.terminal.palette.ansi.contains_key(color))
        );
        renderer.resize(60, 14);
        renderer.render().unwrap();
        let output = output_text(&renderer);
        assert!(output.contains("\x1b[37m"), "{output:?}");
        emulator.feed(&mut renderer);

        for step in 0..12 {
            for (index, cell) in grid.cells.iter_mut().enumerate().skip(6 * 60) {
                cell.glyph = if step % 2 == 0 { 'x' } else { 'y' }.into();
                cell.fg = late[index % late.len()];
            }
            renderer.grid = Some(grid.clone());
            emulator.feed(&mut renderer);
            emulator.assert_shows(&grid, 60);
        }
        assert!(
            early
                .iter()
                .all(|color| !renderer.terminal.palette.ansi.contains_key(color))
        );
        assert!(
            late.iter()
                .all(|color| renderer.terminal.palette.ansi.contains_key(color))
        );
    }

    #[test]
    fn unchanged_frames_write_nothing() {
        let mut grid = CellGrid::new(20, 4, Rgb::new(40, 44, 52));
        mutate(&mut grid, &mut Random::new(7), 10);
        grid.cursor = Some(CursorPosition { col: 5, row: 2 });
        let mut renderer = Renderer::new(Vec::new(), 20, 4);
        let mut emulator = Emulator::new(20, 4);
        renderer.grid = Some(grid.clone());
        assert!(emulator.feed(&mut renderer) > 0);
        assert_eq!(emulator.feed(&mut renderer), 0);
    }

    #[test]
    fn the_terminal_cursor_follows_the_frame_cursor() {
        let mut grid = CellGrid::new(20, 4, Rgb::new(40, 44, 52));
        text_row(&mut grid, 1, 0, "some text");
        let mut renderer = Renderer::new(Vec::new(), 20, 4);
        let mut emulator = Emulator::new(20, 4);
        emulator.processor.advance(&mut emulator.term, b"\x1b[?25l");
        let shown_at = |emulator: &Emulator| {
            let cursor = emulator.term.grid().cursor.point;
            emulator
                .term
                .mode()
                .contains(TermMode::SHOW_CURSOR)
                .then_some((cursor.column.0, cursor.line.0))
        };

        renderer.grid = Some(grid.clone());
        emulator.feed(&mut renderer);
        assert_eq!(shown_at(&emulator), None);

        grid.cursor = Some(CursorPosition { col: 4, row: 1 });
        renderer.grid = Some(grid.clone());
        emulator.feed(&mut renderer);
        assert_eq!(shown_at(&emulator), Some((4, 1)));

        text_row(&mut grid, 3, 0, "more");
        grid.cursor = Some(CursorPosition { col: 9, row: 1 });
        renderer.grid = Some(grid.clone());
        emulator.feed(&mut renderer);
        assert_eq!(shown_at(&emulator), Some((9, 1)));
        emulator.assert_shows(&grid, 20);

        grid.cursor = None;
        renderer.grid = Some(grid.clone());
        emulator.feed(&mut renderer);
        assert_eq!(shown_at(&emulator), None);
    }

    #[test]
    fn the_terminal_cursor_takes_the_frame_cursor_shape() {
        let mut grid = CellGrid::new(20, 4, Rgb::new(40, 44, 52));
        text_row(&mut grid, 1, 0, "some text");
        grid.cursor = Some(CursorPosition { col: 4, row: 1 });
        let mut renderer = Renderer::new(Vec::new(), 20, 4);
        let mut emulator = Emulator::new(20, 4);
        for (shape, expected) in [
            (CursorShape::Underline, ansi::CursorShape::Underline),
            (CursorShape::Block, ansi::CursorShape::Block),
            (CursorShape::Bar, ansi::CursorShape::Beam),
        ] {
            grid.cursor_shape = shape;
            renderer.grid = Some(grid.clone());
            emulator.feed(&mut renderer);
            let style = emulator.term.cursor_style();
            assert_eq!(
                (style.shape, style.blinking),
                (expected, false),
                "{shape:?}"
            );
        }
        assert_eq!(emulator.feed(&mut renderer), 0);
    }

    #[test]
    fn the_cursor_shape_is_reset_on_exit_and_on_signal() {
        let contains_reset = |bytes: &[u8]| {
            bytes
                .windows(CURSOR_SHAPE_RESET.len())
                .any(|window| window == CURSOR_SHAPE_RESET)
        };
        let mut setup = TerminalSetup::default();
        setup
            .run(&mut FailingWriter::failing_on_flush(0), |_| {
                Ok(QueryReplies::default())
            })
            .unwrap();
        let mut undo = Vec::new();
        setup.undo(&mut undo);
        assert!(contains_reset(&undo), "{undo:?}");
        let restore = signal_restore(
            &setup.original_palette,
            setup.features.margin_mode,
            setup.features.ghostty,
        );
        assert!(contains_reset(&restore), "{restore:?}");
    }

    #[test]
    fn single_cell_changes_write_a_short_update() {
        let grid = CellGrid::new(80, 24, Rgb::new(40, 44, 52));
        let mut renderer = Renderer::new(Vec::new(), 80, 24);
        let mut emulator = Emulator::new(80, 24);
        renderer.grid = Some(grid.clone());
        let full = emulator.feed(&mut renderer);
        assert!(full < 500, "blank frame took {full} bytes");

        let mut next = grid;
        if let Some(cell) = next.cell_mut(10, 5) {
            cell.glyph = 'x'.into();
            cell.fg = Rgb::new(200, 120, 60);
        }
        renderer.grid = Some(next.clone());
        let update = emulator.feed(&mut renderer);
        assert!(update < 48, "one cell took {update} bytes");
        emulator.assert_shows(&next, 80);
    }

    #[derive(Clone, Default)]
    struct SharedOutput(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn full_frame(cols: u16, rows: u16) -> RenderEvent {
        let mut grid = CellGrid::new(cols, rows, Rgb::new(40, 44, 52));
        text_row(&mut grid, 0, 0, "hello");
        let mut encoder = crate::tui::protocol::FrameEncoder::default();
        RenderEvent::Server(encoder.update(None, &grid).unwrap())
    }

    #[test]
    fn a_resize_waits_for_the_resized_frame_before_repainting() {
        let output = SharedOutput::default();
        let (sender, receiver) = mpsc::channel();
        let (socket, _peer) = UnixStream::pair().unwrap();
        let render_thread = thread::spawn({
            let output = output.clone();
            move || {
                let mut renderer = Renderer::new(output, 20, 4);
                render_messages(&mut renderer, &receiver, &Mutex::new(socket));
            }
        });
        let take_output = || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while output.0.lock().is_empty() {
                assert!(Instant::now() < deadline, "nothing was rendered");
                thread::sleep(Duration::from_millis(5));
            }
            String::from_utf8(std::mem::take(&mut *output.0.lock())).unwrap()
        };

        sender.send(full_frame(20, 4)).unwrap();
        assert!(take_output().contains("hello"));

        sender.send(RenderEvent::Resized(30, 6)).unwrap();
        thread::sleep(RESIZE_FRAME_WAIT / 3);
        assert!(output.0.lock().is_empty());
        sender.send(full_frame(30, 6)).unwrap();
        assert_eq!(take_output().matches("\x1b[2J").count(), 1);

        sender.send(RenderEvent::Resized(25, 5)).unwrap();
        assert!(take_output().contains("\x1b[2J"));

        drop(sender);
        render_thread.join().unwrap();
    }

    #[test]
    fn resizing_redraws_everything() {
        let mut grid = CellGrid::new(20, 4, Rgb::new(40, 44, 52));
        mutate(&mut grid, &mut Random::new(9), 10);
        let mut renderer = Renderer::new(Vec::new(), 20, 4);
        renderer.grid = Some(grid.clone());
        Emulator::new(20, 4).feed(&mut renderer);

        let mut emulator = Emulator::new(20, 4);
        renderer.resize(20, 4);
        emulator.feed(&mut renderer);
        emulator.assert_shows(&grid, 20);
    }

    fn mouse(action: MouseAction, col: u16) -> TermEvent {
        TermEvent::Mouse {
            action,
            col,
            row: 0,
            modifiers: Modifiers::default(),
        }
    }

    fn columns(inputs: &[TermEvent]) -> Vec<u16> {
        inputs
            .iter()
            .map(|input| match input {
                TermEvent::Mouse { col, .. } => *col,
                _ => u16::MAX,
            })
            .collect()
    }

    #[test]
    fn mouse_moves_are_throttled_to_the_latest_position() {
        let mut throttle = MoveThrottle::default();
        let start = Instant::now();
        let moved = |col| vec![mouse(MouseAction::Moved, col)];

        assert_eq!(columns(&throttle.push(start, moved(1))), [1]);
        assert!(throttle.push(start, moved(2)).is_empty());
        assert!(
            throttle
                .push(start + Duration::from_millis(3), moved(3))
                .is_empty()
        );
        let deadline = start + MOUSE_MOVE_INTERVAL;
        assert_eq!(throttle.deadline(), Some(deadline));
        assert!(
            throttle
                .take_due(deadline - Duration::from_millis(1))
                .is_none()
        );
        assert_eq!(
            columns(&throttle.take_due(deadline).into_iter().collect::<Vec<_>>()),
            [3]
        );
        assert_eq!(throttle.deadline(), None);

        let quiet = deadline + MOUSE_MOVE_INTERVAL;
        assert_eq!(columns(&throttle.push(quiet, moved(4))), [4]);
    }

    #[test]
    fn a_click_flushes_the_held_move_first() {
        let mut throttle = MoveThrottle::default();
        let start = Instant::now();
        throttle.push(start, vec![mouse(MouseAction::Moved, 1)]);
        assert!(
            throttle
                .push(start, vec![mouse(MouseAction::Moved, 2)])
                .is_empty()
        );

        let click = mouse(MouseAction::Down(MouseButtonKind::Left), 2);
        let sent = throttle.push(start, vec![click.clone()]);
        assert_eq!(sent, [mouse(MouseAction::Moved, 2), click]);
        assert_eq!(throttle.deadline(), None);
    }
}
