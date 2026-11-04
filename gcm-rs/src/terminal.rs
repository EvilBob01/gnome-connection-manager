//! Terminal tab: alacritty_terminal does the VT emulation, this module feeds
//! it bytes from a PTY or serial port and draws its grid with egui.

use std::io::{ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    ClearMode, Color as AnsiColor, CursorShape, Handler, NamedColor, Processor, Rgb,
};
use anyhow::{Context as _, Result};
use eframe::egui::{
    self, Align2, Color32, Event, EventFilter, FontId, Key, Modifiers, MouseWheelUnit,
    PointerButton, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::config::{FlowControl, Parity, SerialSettings};
use crate::launch::Autofill;

#[derive(Clone, Copy)]
struct TermSize {
    cols: usize,
    lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

fn write_all(writer: &Writer, bytes: &[u8]) {
    if let Ok(mut w) = writer.lock() {
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }
}

/// State shared with the reader thread and the terminal's event listener.
#[derive(Default)]
struct Shared {
    title: Mutex<Option<String>>,
    exited: AtomicBool,
    dark: AtomicBool,
    window_size: Mutex<Option<WindowSize>>,
    /// Cleared on the first keystroke: after that a "Password:" prompt is
    /// the user's business, not something to answer with the saved password.
    autofill_armed: AtomicBool,
}

#[derive(Clone)]
struct Listener {
    writer: Writer,
    shared: Arc<Shared>,
    ctx: egui::Context,
}

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        match event {
            TermEvent::PtyWrite(text) => write_all(&self.writer, text.as_bytes()),
            TermEvent::Title(t) => *self.shared.title.lock().unwrap() = Some(t),
            TermEvent::ResetTitle => *self.shared.title.lock().unwrap() = None,
            TermEvent::ClipboardStore(_, text) => self.ctx.copy_text(text),
            TermEvent::ColorRequest(index, reply) => {
                let rgb = palette_rgb(index, self.shared.dark.load(Ordering::Relaxed));
                write_all(&self.writer, reply(rgb).as_bytes());
            }
            TermEvent::TextAreaSizeRequest(reply) => {
                if let Some(ws) = *self.shared.window_size.lock().unwrap() {
                    write_all(&self.writer, reply(ws).as_bytes());
                }
            }
            _ => {}
        }
    }
}

/// The process or device behind a terminal.
trait Backend: Send {
    fn resize(&mut self, cols: u16, rows: u16);
    /// True once the child has exited. Needed on Windows, where the ConPTY
    /// reader does not always see EOF when the child goes away.
    fn exited(&mut self) -> bool;
    fn kill(&mut self);
}

struct PtyBackend {
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl Backend for PtyBackend {
    fn resize(&mut self, cols: u16, rows: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }
    fn exited(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }
    fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

struct SerialBackend {
    stop: Arc<AtomicBool>,
}

impl Backend for SerialBackend {
    fn resize(&mut self, _cols: u16, _rows: u16) {}
    fn exited(&mut self) -> bool {
        false
    }
    fn kill(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Serial reads time out so the thread can notice it was asked to stop.
struct SerialReader {
    port: Box<dyn serialport::SerialPort>,
    stop: Arc<AtomicBool>,
}

impl Read for SerialReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(0);
            }
            match self.port.read(buf) {
                Err(e) if e.kind() == ErrorKind::TimedOut => continue,
                r => return r,
            }
        }
    }
}

/// Watches output for login and password prompts and answers each once.
struct AutofillState {
    user: Option<String>,
    password: Option<String>,
    tail: String,
}

impl AutofillState {
    fn feed(&mut self, bytes: &[u8], writer: &Writer, armed: &AtomicBool) {
        if !armed.load(Ordering::Relaxed) {
            self.user = None;
            self.password = None;
        }
        if self.user.is_none() && self.password.is_none() {
            return;
        }
        self.tail.push_str(&String::from_utf8_lossy(bytes));
        if self.tail.len() > 512 {
            let cut = self.tail.len() - 256;
            let cut = (cut..self.tail.len())
                .find(|&i| self.tail.is_char_boundary(i))
                .unwrap_or(0);
            self.tail.drain(..cut);
        }
        let prompt = self.tail.trim_end().to_ascii_lowercase();
        if prompt.ends_with("login:") || prompt.ends_with("username:") {
            if let Some(user) = self.user.take() {
                write_all(writer, format!("{user}\r").as_bytes());
                self.tail.clear();
            }
        } else if prompt.ends_with("assword:")
            && let Some(pw) = self.password.take()
        {
            write_all(writer, format!("{pw}\r").as_bytes());
            self.tail.clear();
        }
    }
}

/// One piece of user input, kept in a form that can be replayed into other
/// terminals (multi-exec). Keys are re-encoded for each target, so arrow keys
/// work whether or not that terminal is in application-cursor mode.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Bytes(Vec<u8>),
    Key(Key, Modifiers),
    Paste(String),
}

pub struct Terminal {
    term: Arc<Mutex<Term<Listener>>>,
    writer: Writer,
    shared: Arc<Shared>,
    backend: Box<dyn Backend>,
    size: TermSize,
    scroll_acc: f32,
    /// Text queued to be typed after a delay (startup commands).
    scheduled: Vec<(Instant, String)>,
    mouse_buttons: u8,
    focus_pending: bool,
    /// User input since the app last called `take_input`.
    recorded: Vec<Input>,
}

impl Terminal {
    pub fn spawn_pty(
        ctx: &egui::Context,
        program: &str,
        args: &[String],
        autofill: Autofill,
        scrollback: usize,
    ) -> Result<Self> {
        let size = TermSize {
            cols: 80,
            lines: 24,
        };
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: size.lines as u16,
                cols: size.cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening a pseudo-terminal")?;
        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        if let Some(home) = dirs::home_dir() {
            cmd.cwd(home);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("starting {program}"))?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let backend = PtyBackend {
            master: pair.master,
            child,
        };
        Ok(Self::start(
            ctx,
            reader,
            writer,
            Box::new(backend),
            autofill,
            scrollback,
            size,
        ))
    }

    pub fn open_serial(
        ctx: &egui::Context,
        device: &str,
        s: &SerialSettings,
        scrollback: usize,
    ) -> Result<Self> {
        use serialport::{DataBits, FlowControl as F, Parity as P, StopBits};
        let port = serialport::new(device, s.baud)
            .data_bits(match s.data_bits {
                5 => DataBits::Five,
                6 => DataBits::Six,
                7 => DataBits::Seven,
                _ => DataBits::Eight,
            })
            .parity(match s.parity {
                Parity::None => P::None,
                Parity::Odd => P::Odd,
                Parity::Even => P::Even,
            })
            .stop_bits(if s.stop_bits == 2 {
                StopBits::Two
            } else {
                StopBits::One
            })
            .flow_control(match s.flow_control {
                FlowControl::None => F::None,
                FlowControl::Software => F::Software,
                FlowControl::Hardware => F::Hardware,
            })
            .timeout(Duration::from_millis(200))
            .open()
            .with_context(|| format!("opening {device}"))?;
        let writer = port.try_clone().context("cloning serial port handle")?;
        let stop = Arc::new(AtomicBool::new(false));
        let reader = SerialReader {
            port,
            stop: stop.clone(),
        };
        let backend = SerialBackend { stop };
        let size = TermSize {
            cols: 80,
            lines: 24,
        };
        Ok(Self::start(
            ctx,
            Box::new(reader),
            writer,
            Box::new(backend),
            Autofill::default(),
            scrollback,
            size,
        ))
    }

    fn start(
        ctx: &egui::Context,
        mut reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        backend: Box<dyn Backend>,
        autofill: Autofill,
        scrollback: usize,
        size: TermSize,
    ) -> Self {
        let writer: Writer = Arc::new(Mutex::new(writer));
        let shared = Arc::new(Shared::default());
        shared.autofill_armed.store(true, Ordering::Relaxed);
        let listener = Listener {
            writer: writer.clone(),
            shared: shared.clone(),
            ctx: ctx.clone(),
        };
        let config = TermConfig {
            scrolling_history: scrollback,
            ..Default::default()
        };
        let term = Arc::new(Mutex::new(Term::new(config, &size, listener)));

        let (t, w, sh, c) = (term.clone(), writer.clone(), shared.clone(), ctx.clone());
        let mut autofill = AutofillState {
            user: autofill.user,
            password: autofill.password,
            tail: String::new(),
        };
        thread::spawn(move || {
            let mut parser: Processor = Processor::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        parser.advance(&mut *t.lock().unwrap(), &buf[..n]);
                        autofill.feed(&buf[..n], &w, &sh.autofill_armed);
                        c.request_repaint();
                    }
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            sh.exited.store(true, Ordering::Relaxed);
            c.request_repaint();
        });

        Self {
            term,
            writer,
            shared,
            backend,
            size,
            scroll_acc: 0.0,
            scheduled: Vec::new(),
            mouse_buttons: 0,
            focus_pending: true,
            recorded: Vec::new(),
        }
    }

    pub fn title(&self) -> Option<String> {
        self.shared.title.lock().unwrap().clone()
    }

    pub fn is_exited(&mut self) -> bool {
        if !self.shared.exited.load(Ordering::Relaxed) && self.backend.exited() {
            self.shared.exited.store(true, Ordering::Relaxed);
        }
        self.shared.exited.load(Ordering::Relaxed)
    }

    pub fn write(&self, bytes: &[u8]) {
        write_all(&self.writer, bytes);
    }

    pub fn schedule(&mut self, delay_ms: u64, text: String) {
        self.scheduled
            .push((Instant::now() + Duration::from_millis(delay_ms), text));
    }

    /// Apply user input and record it for multi-exec.
    fn input(&mut self, input: Input) {
        self.apply(&input);
        self.recorded.push(input);
    }

    fn apply(&mut self, input: &Input) {
        match input {
            Input::Bytes(b) => self.write(b),
            Input::Key(key, mods) => {
                let mode = *self.term.lock().unwrap().mode();
                if let Some(b) = key_bytes(*key, *mods, mode) {
                    self.write(&b);
                }
            }
            Input::Paste(text) => self.paste(text),
        }
        self.shared.autofill_armed.store(false, Ordering::Relaxed);
        let mut term = self.term.lock().unwrap();
        term.scroll_display(Scroll::Bottom);
        term.selection = None;
    }

    /// Input typed into this terminal since the last call.
    pub fn take_input(&mut self) -> Vec<Input> {
        std::mem::take(&mut self.recorded)
    }

    /// Feed input typed elsewhere, without recording it again.
    pub fn replay(&mut self, inputs: &[Input]) {
        if self.is_exited() {
            return;
        }
        for i in inputs {
            self.apply(i);
        }
    }

    fn paste(&self, text: &str) {
        let bracketed = self
            .term
            .lock()
            .unwrap()
            .mode()
            .contains(TermMode::BRACKETED_PASTE);
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if bracketed {
            self.write(format!("\x1b[200~{text}\x1b[201~").as_bytes());
        } else {
            self.write(text.as_bytes());
        }
    }

    /// Take keyboard focus on the next frame.
    pub fn request_focus(&mut self) {
        self.focus_pending = true;
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term
            .lock()
            .unwrap()
            .selection_to_string()
            .filter(|s| !s.is_empty())
    }

    pub fn clear_scrollback(&self) {
        self.term.lock().unwrap().clear_screen(ClearMode::Saved);
    }

    /// Draw the terminal into all available space and handle its input.
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        font_size: f32,
        dark: bool,
        copy_on_select: bool,
        paste_on_right_click: bool,
    ) {
        self.shared.dark.store(dark, Ordering::Relaxed);
        let now = Instant::now();
        let (due, later): (Vec<_>, Vec<_>) =
            self.scheduled.drain(..).partition(|(at, _)| *at <= now);
        self.scheduled = later;
        for (_, text) in due {
            self.write(text.as_bytes());
        }
        if let Some(next) = self.scheduled.iter().map(|(at, _)| *at).min() {
            ui.ctx().request_repaint_after(next - now);
        }
        if !self.is_exited() {
            // Poll for child exit even when no output arrives.
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }

        let font = FontId::monospace(font_size);
        let (cw, ch) = ui.fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let cols = ((rect.width() / cw).floor() as usize).max(2);
        let lines = ((rect.height() / ch).floor() as usize).max(1);
        if cols != self.size.cols || lines != self.size.lines {
            self.size = TermSize { cols, lines };
            self.term.lock().unwrap().resize(self.size);
            self.backend.resize(cols as u16, lines as u16);
        }
        *self.shared.window_size.lock().unwrap() = Some(WindowSize {
            num_lines: lines as u16,
            num_cols: cols as u16,
            cell_width: cw as u16,
            cell_height: ch as u16,
        });

        if std::mem::take(&mut self.focus_pending)
            || response.clicked()
            || response.drag_started()
            || response.secondary_clicked()
        {
            response.request_focus();
        }
        let focused = response.has_focus();
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    response.id,
                    EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
        }

        let cell = vec2(cw, ch);
        self.handle_mouse(
            ui,
            &response,
            rect,
            cell,
            copy_on_select,
            paste_on_right_click,
        );
        if focused {
            self.handle_keys(ui);
        }
        self.paint(ui, rect, cell, &font, dark, focused);
    }

    fn cell_at(&self, rect: Rect, cell: Vec2, pos: Pos2) -> (usize, usize, Side) {
        let x = (pos.x - rect.left()).max(0.0);
        let y = (pos.y - rect.top()).max(0.0);
        let col = ((x / cell.x) as usize).min(self.size.cols - 1);
        let row = ((y / cell.y) as usize).min(self.size.lines - 1);
        let side = if x - col as f32 * cell.x > cell.x / 2.0 {
            Side::Right
        } else {
            Side::Left
        };
        (col, row, side)
    }

    fn grid_point(term: &Term<Listener>, col: usize, row: usize) -> Point {
        let offset = term.grid().display_offset() as i32;
        Point::new(Line(row as i32 - offset), Column(col))
    }

    fn handle_mouse(
        &mut self,
        ui: &mut Ui,
        response: &egui::Response,
        rect: Rect,
        cell: Vec2,
        copy_on_select: bool,
        paste_on_right_click: bool,
    ) {
        let mode = *self.term.lock().unwrap().mode();
        let events = ui.input(|i| i.events.clone());
        let shift = ui.input(|i| i.modifiers.shift);
        let report = mode.intersects(TermMode::MOUSE_MODE) && !shift;

        for ev in &events {
            match ev {
                Event::MouseWheel { unit, delta, .. } if response.contains_pointer() => {
                    let lines = match unit {
                        MouseWheelUnit::Point => delta.y / cell.y,
                        MouseWheelUnit::Line => delta.y,
                        MouseWheelUnit::Page => delta.y * self.size.lines as f32,
                    };
                    self.scroll_acc += lines;
                    let steps = self.scroll_acc.trunc() as i32;
                    if steps == 0 {
                        continue;
                    }
                    self.scroll_acc -= steps as f32;
                    if report {
                        let pos = ui.input(|i| i.pointer.hover_pos()).unwrap_or(rect.min);
                        let (col, row, _) = self.cell_at(rect, cell, pos);
                        let button = if steps > 0 { 64 } else { 65 };
                        for _ in 0..steps.abs() {
                            self.report_mouse(mode, button, col, row, true);
                        }
                    } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
                        // less, man, vim: turn the wheel into arrow keys.
                        let key: &[u8] = match (steps > 0, mode.contains(TermMode::APP_CURSOR)) {
                            (true, true) => b"\x1bOA",
                            (true, false) => b"\x1b[A",
                            (false, true) => b"\x1bOB",
                            (false, false) => b"\x1b[B",
                        };
                        for _ in 0..steps.abs() * 3 {
                            self.write(key);
                        }
                    } else {
                        self.term
                            .lock()
                            .unwrap()
                            .scroll_display(Scroll::Delta(steps * 3));
                    }
                }
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } if report && rect.contains(*pos) => {
                    let (code, bit) = match button {
                        PointerButton::Primary => (0, 1),
                        PointerButton::Middle => (1, 2),
                        PointerButton::Secondary => (2, 4),
                        _ => continue,
                    };
                    if *pressed {
                        self.mouse_buttons |= bit;
                    } else {
                        self.mouse_buttons &= !bit;
                    }
                    let (col, row, _) = self.cell_at(rect, cell, *pos);
                    self.report_mouse(mode, code, col, row, *pressed);
                }
                Event::PointerMoved(pos)
                    if report
                        && rect.contains(*pos)
                        && (mode.contains(TermMode::MOUSE_MOTION)
                            || (mode.contains(TermMode::MOUSE_DRAG)
                                && self.mouse_buttons != 0)) =>
                {
                    let (col, row, _) = self.cell_at(rect, cell, *pos);
                    let button = match self.mouse_buttons {
                        b if b & 1 != 0 => 32,
                        b if b & 2 != 0 => 33,
                        b if b & 4 != 0 => 34,
                        _ => 35,
                    };
                    self.report_mouse(mode, button, col, row, true);
                }
                _ => {}
            }
        }
        if report {
            return;
        }

        // Local selection.
        let pointer = response.interact_pointer_pos();
        if let Some(pos) = pointer {
            let (col, row, side) = self.cell_at(rect, cell, pos);
            let mut term = self.term.lock().unwrap();
            let point = Self::grid_point(&term, col, row);
            if response.triple_clicked() {
                term.selection = Some(Selection::new(SelectionType::Lines, point, side));
            } else if response.double_clicked() {
                term.selection = Some(Selection::new(SelectionType::Semantic, point, side));
            } else if response.drag_started_by(PointerButton::Primary) {
                let ty = if ui.input(|i| i.modifiers.alt) {
                    SelectionType::Block
                } else {
                    SelectionType::Simple
                };
                term.selection = Some(Selection::new(ty, point, side));
            } else if response.dragged_by(PointerButton::Primary) {
                if let Some(sel) = term.selection.as_mut() {
                    sel.update(point, side);
                }
                // Auto-scroll when dragging past the top or bottom edge.
                if pos.y < rect.top() {
                    term.scroll_display(Scroll::Delta(1));
                } else if pos.y > rect.bottom() {
                    term.scroll_display(Scroll::Delta(-1));
                }
            } else if response.clicked_by(PointerButton::Primary) {
                term.selection = None;
            }
        }
        let finished =
            response.drag_stopped_by(PointerButton::Primary) || response.double_clicked();
        if copy_on_select
            && finished
            && let Some(text) = self.selection_text()
        {
            ui.ctx().copy_text(text);
        }

        if paste_on_right_click {
            if response.secondary_clicked()
                && let Some(text) = read_clipboard()
            {
                self.input(Input::Paste(text));
            }
        } else {
            response.context_menu(|ui| {
                if ui.button("Copy").clicked() {
                    if let Some(text) = self.selection_text() {
                        ui.ctx().copy_text(text);
                    }
                    ui.close();
                }
                if ui.button("Paste").clicked() {
                    if let Some(text) = read_clipboard() {
                        self.input(Input::Paste(text));
                    }
                    ui.close();
                }
                if ui.button("Clear scrollback").clicked() {
                    self.clear_scrollback();
                    ui.close();
                }
            });
        }
    }

    fn report_mouse(&self, mode: TermMode, button: u8, col: usize, row: usize, pressed: bool) {
        if mode.contains(TermMode::SGR_MOUSE) {
            let end = if pressed { 'M' } else { 'm' };
            self.write(format!("\x1b[<{button};{};{}{end}", col + 1, row + 1).as_bytes());
        } else {
            // Legacy X10 encoding cannot express which button was released.
            let b = if pressed { button } else { 3 };
            let enc = |v: usize| (32 + 1 + v).min(255) as u8;
            self.write(&[0x1b, b'[', b'M', 32 + b, enc(col), enc(row)]);
        }
    }

    fn handle_keys(&mut self, ui: &mut Ui) {
        let (events, mods) = ui.input(|i| (i.events.clone(), i.modifiers));
        for ev in events {
            match ev {
                // egui-winit turns Ctrl+C/X/V into these; plain Ctrl must still reach the shell.
                // Ctrl+Insert also arrives as Copy on Windows, so there Ctrl+C
                // copies when text is selected, as in Windows Terminal.
                Event::Copy => {
                    let selection = self.selection_text();
                    if mods.shift || (cfg!(windows) && selection.is_some()) {
                        if let Some(text) = selection {
                            ui.ctx().copy_text(text);
                        }
                        self.term.lock().unwrap().selection = None;
                    } else {
                        self.input(Input::Bytes(b"\x03".to_vec()));
                    }
                }
                Event::Cut => {
                    if !mods.shift {
                        self.input(Input::Bytes(b"\x18".to_vec()));
                    }
                }
                Event::Paste(text) => {
                    if mods.ctrl && !mods.shift {
                        self.input(Input::Bytes(b"\x16".to_vec()));
                    } else {
                        self.input(Input::Paste(text));
                    }
                }
                // Alt+key is sent as ESC+key from the Key event; Ctrl+Alt is AltGr on Windows.
                Event::Text(text) if !(mods.alt && !mods.ctrl) => {
                    self.input(Input::Bytes(text.into_bytes()));
                }
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if modifiers.shift && !modifiers.ctrl {
                        let scroll = match key {
                            Key::PageUp => Some(Scroll::PageUp),
                            Key::PageDown => Some(Scroll::PageDown),
                            Key::Home => Some(Scroll::Top),
                            Key::End => Some(Scroll::Bottom),
                            _ => None,
                        };
                        if let Some(s) = scroll {
                            self.term.lock().unwrap().scroll_display(s);
                            continue;
                        }
                    }
                    if key == Key::Insert && modifiers.shift && !modifiers.ctrl {
                        if let Some(text) = read_clipboard() {
                            self.input(Input::Paste(text));
                        }
                        continue;
                    }
                    if key == Key::Insert && modifiers.ctrl && !modifiers.shift {
                        if let Some(text) = self.selection_text() {
                            ui.ctx().copy_text(text);
                        }
                        continue;
                    }
                    let mode = *self.term.lock().unwrap().mode();
                    if key_bytes(key, modifiers, mode).is_some() {
                        self.input(Input::Key(key, modifiers));
                    }
                }
                _ => {}
            }
        }
    }

    fn paint(&self, ui: &mut Ui, rect: Rect, cell: Vec2, font: &FontId, dark: bool, focused: bool) {
        let painter = ui.painter_at(rect);
        let term = self.term.lock().unwrap();
        let content = term.renderable_content();
        let colors = content.colors;
        let resolve = |c: AnsiColor| -> Color32 {
            let rgb = match c {
                AnsiColor::Spec(rgb) => rgb,
                AnsiColor::Indexed(i) => {
                    colors[i as usize].unwrap_or_else(|| palette_rgb(i as usize, dark))
                }
                AnsiColor::Named(n) => {
                    colors[n as usize].unwrap_or_else(|| palette_rgb(n as usize, dark))
                }
            };
            Color32::from_rgb(rgb.r, rgb.g, rgb.b)
        };
        let default_bg = resolve(AnsiColor::Named(NamedColor::Background));
        painter.rect_filled(rect, 0.0, default_bg);

        let offset = content.display_offset as i32;
        let selection = content.selection;
        let cursor = content.cursor;
        let origin = rect.min;
        let cell_rect = |col: usize, row: i32| {
            Rect::from_min_size(
                pos2(
                    origin.x + col as f32 * cell.x,
                    origin.y + row as f32 * cell.y,
                ),
                cell,
            )
        };

        // Runs of same-styled ASCII text are drawn as one string; wide and
        // non-ASCII glyphs are drawn alone so they cannot shift the columns.
        struct Run {
            row: i32,
            col: usize,
            text: String,
            fg: Color32,
            underline: bool,
            strike: bool,
            /// Wide and non-ASCII glyphs are never extended.
            open: bool,
        }
        let mut runs: Vec<Run> = Vec::new();
        let mut cursor_cell: Option<(Rect, char, Color32, Color32)> = None;

        for item in content.display_iter {
            let row = item.point.line.0 + offset;
            let col = item.point.column.0;
            let c = &item.cell;
            if c.flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let mut fg_color = c.fg;
            if c.flags.contains(Flags::BOLD)
                && let AnsiColor::Named(n) = fg_color
            {
                fg_color = AnsiColor::Named(n.to_bright());
            }
            let mut fg = resolve(fg_color);
            let mut bg = resolve(c.bg);
            if c.flags.contains(Flags::DIM) {
                fg = fg.gamma_multiply(0.66);
            }
            if c.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if selection.is_some_and(|s| s.contains(item.point)) {
                fg = resolve(AnsiColor::Named(NamedColor::Background));
                bg = resolve(AnsiColor::Named(NamedColor::Foreground));
            }
            if c.flags.contains(Flags::HIDDEN) {
                fg = bg;
            }
            let wide = c.flags.contains(Flags::WIDE_CHAR);
            let mut r = cell_rect(col, row);
            if wide {
                r.max.x += cell.x;
            }
            if bg != default_bg {
                painter.rect_filled(r, 0.0, bg);
            }
            if item.point == cursor.point {
                cursor_cell = Some((r, c.c, fg, bg));
            }
            let ch = c.c;
            if ch == ' ' || ch == '\0' {
                continue;
            }
            let underline = c.flags.intersects(Flags::ALL_UNDERLINES);
            let strike = c.flags.contains(Flags::STRIKEOUT);
            let simple = ch.is_ascii() && !wide;
            if simple
                && let Some(last) = runs.last_mut()
                && last.row == row
                && last.col + last.text.len() == col
                && last.fg == fg
                && last.underline == underline
                && last.strike == strike
                && last.open
            {
                last.text.push(ch);
                continue;
            }
            let mut text = String::from(ch);
            if let Some(zw) = c.zerowidth() {
                text.extend(zw);
            }
            runs.push(Run {
                row,
                col,
                text,
                fg,
                underline,
                strike,
                open: simple,
            });
        }

        for run in &runs {
            let r = cell_rect(run.col, run.row);
            painter.text(r.min, Align2::LEFT_TOP, &run.text, font.clone(), run.fg);
            let width = cell.x * run.text.chars().count() as f32;
            if run.underline {
                let y = r.bottom() - 1.0;
                painter.line_segment(
                    [pos2(r.left(), y), pos2(r.left() + width, y)],
                    Stroke::new(1.0, run.fg),
                );
            }
            if run.strike {
                let y = r.center().y;
                painter.line_segment(
                    [pos2(r.left(), y), pos2(r.left() + width, y)],
                    Stroke::new(1.0, run.fg),
                );
            }
        }

        if offset == 0 || cursor.point.line.0 + offset < self.size.lines as i32 {
            let cursor_color = resolve(AnsiColor::Named(NamedColor::Cursor));
            let (r, ch, _fg, bg) = cursor_cell.unwrap_or((
                cell_rect(cursor.point.column.0, cursor.point.line.0 + offset),
                ' ',
                cursor_color,
                default_bg,
            ));
            match cursor.shape {
                CursorShape::Hidden => {}
                _ if !focused => {
                    painter.rect_stroke(r, 0.0, Stroke::new(1.0, cursor_color), StrokeKind::Inside);
                }
                CursorShape::Block => {
                    painter.rect_filled(r, 0.0, cursor_color);
                    if ch != ' ' && ch != '\0' {
                        painter.text(r.min, Align2::LEFT_TOP, ch, font.clone(), bg);
                    }
                }
                CursorShape::HollowBlock => {
                    painter.rect_stroke(r, 0.0, Stroke::new(1.0, cursor_color), StrokeKind::Inside);
                }
                CursorShape::Beam => {
                    painter.rect_filled(
                        Rect::from_min_size(r.min, vec2(2.0, r.height())),
                        0.0,
                        cursor_color,
                    );
                }
                CursorShape::Underline => {
                    let y = r.bottom() - 2.0;
                    painter.rect_filled(
                        Rect::from_min_max(pos2(r.left(), y), r.right_bottom()),
                        0.0,
                        cursor_color,
                    );
                }
            }
        }

        if offset > 0 {
            let label = format!("↑ {offset} lines");
            let pos = rect.right_top() + vec2(-8.0, 4.0);
            painter.text(
                pos,
                Align2::RIGHT_TOP,
                label,
                FontId::proportional(12.0),
                Color32::from_rgb(200, 160, 0),
            );
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.backend.kill();
    }
}

fn read_clipboard() -> Option<String> {
    arboard::Clipboard::new()
        .ok()?
        .get_text()
        .ok()
        .filter(|t| !t.is_empty())
}

/// xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl.
fn modifier_param(m: Modifiers) -> u8 {
    1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.ctrl as u8
}

fn key_bytes(key: Key, m: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let param = modifier_param(m);
    let csi = |final_byte: char| -> Vec<u8> {
        if param > 1 {
            format!("\x1b[1;{param}{final_byte}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{final_byte}").into_bytes()
        } else {
            format!("\x1b[{final_byte}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if param > 1 {
            format!("\x1b[{n};{param}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    let ss3 = |final_byte: char| -> Vec<u8> {
        if param > 1 {
            format!("\x1b[1;{param}{final_byte}").into_bytes()
        } else {
            format!("\x1bO{final_byte}").into_bytes()
        }
    };
    let alt = |b: &[u8]| -> Vec<u8> {
        let mut v = if m.alt { vec![0x1b] } else { Vec::new() };
        v.extend_from_slice(b);
        v
    };
    Some(match key {
        Key::Enter => alt(b"\r"),
        Key::Backspace => alt(if m.ctrl { b"\x08" } else { b"\x7f" }),
        Key::Tab if m.shift => b"\x1b[Z".to_vec(),
        Key::Tab => alt(b"\t"),
        Key::Escape => b"\x1b".to_vec(),
        Key::ArrowUp => csi('A'),
        Key::ArrowDown => csi('B'),
        Key::ArrowRight => csi('C'),
        Key::ArrowLeft => csi('D'),
        Key::Home => csi('H'),
        Key::End => csi('F'),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::F1 => ss3('P'),
        Key::F2 => ss3('Q'),
        Key::F3 => ss3('R'),
        Key::F4 => ss3('S'),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        _ if m.ctrl && !m.shift => {
            let name = key.name();
            let b = match key {
                Key::Space | Key::Num2 => 0x00,
                Key::OpenBracket => 0x1b,
                Key::Backslash => 0x1c,
                Key::CloseBracket => 0x1d,
                Key::Num6 => 0x1e,
                Key::Slash | Key::Minus => 0x1f,
                _ if name.len() == 1 && name.as_bytes()[0].is_ascii_alphabetic() => {
                    name.as_bytes()[0].to_ascii_uppercase() - b'A' + 1
                }
                _ => return None,
            };
            alt(&[b])
        }
        _ if m.alt && !m.ctrl => {
            let name = key.name();
            if name.len() != 1 {
                return None;
            }
            let c = name.chars().next()?;
            let c = if m.shift {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            vec![0x1b, c as u8]
        }
        _ => return None,
    })
}

/// Default palette (xterm-like), foreground/background follow the UI theme.
fn palette_rgb(index: usize, dark: bool) -> Rgb {
    const BASE: [(u8, u8, u8); 16] = [
        (0x1e, 0x1e, 0x1e),
        (0xcd, 0x31, 0x31),
        (0x0d, 0xbc, 0x79),
        (0xe5, 0xe5, 0x10),
        (0x24, 0x72, 0xc8),
        (0xbc, 0x3f, 0xbc),
        (0x11, 0xa8, 0xcd),
        (0xe5, 0xe5, 0xe5),
        (0x66, 0x66, 0x66),
        (0xf1, 0x4c, 0x4c),
        (0x23, 0xd1, 0x8b),
        (0xf5, 0xf5, 0x43),
        (0x3b, 0x8e, 0xea),
        (0xd6, 0x70, 0xd6),
        (0x29, 0xb8, 0xdb),
        (0xff, 0xff, 0xff),
    ];
    let rgb = |(r, g, b): (u8, u8, u8)| Rgb { r, g, b };
    let (fg, bg) = if dark {
        ((0xd4, 0xd4, 0xd4), (0x1e, 0x1e, 0x1e))
    } else {
        ((0x1e, 0x1e, 0x1e), (0xff, 0xff, 0xff))
    };
    match index {
        0..=15 => rgb(BASE[index]),
        16..=231 => {
            let i = index - 16;
            let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
            rgb((level(i / 36), level((i / 6) % 6), level(i % 6)))
        }
        232..=255 => {
            let v = (8 + (index - 232) * 10) as u8;
            rgb((v, v, v))
        }
        i if i == NamedColor::Foreground as usize || i == NamedColor::BrightForeground as usize => {
            rgb(fg)
        }
        i if i == NamedColor::Background as usize => rgb(bg),
        i if i == NamedColor::Cursor as usize => rgb(fg),
        i if i == NamedColor::DimForeground as usize => {
            let f = rgb(fg);
            Rgb {
                r: f.r / 3 * 2,
                g: f.g / 3 * 2,
                b: f.b / 3 * 2,
            }
        }
        // DimBlack..DimWhite
        i if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) => {
            let b = rgb(BASE[i - NamedColor::DimBlack as usize]);
            Rgb {
                r: b.r / 3 * 2,
                g: b.g / 3 * 2,
                b: b.b / 3 * 2,
            }
        }
        _ => rgb(fg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        let none = Modifiers::NONE;
        let ctrl = Modifiers::CTRL;
        assert_eq!(
            key_bytes(Key::ArrowUp, none, TermMode::NONE).unwrap(),
            b"\x1b[A"
        );
        assert_eq!(
            key_bytes(Key::ArrowUp, none, TermMode::APP_CURSOR).unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            key_bytes(Key::ArrowLeft, ctrl, TermMode::NONE).unwrap(),
            b"\x1b[1;5D"
        );
        assert_eq!(key_bytes(Key::D, ctrl, TermMode::NONE).unwrap(), b"\x04");
        assert_eq!(key_bytes(Key::Z, ctrl, TermMode::NONE).unwrap(), b"\x1a");
        assert_eq!(
            key_bytes(Key::F5, none, TermMode::NONE).unwrap(),
            b"\x1b[15~"
        );
        assert_eq!(
            key_bytes(Key::B, Modifiers::ALT, TermMode::NONE).unwrap(),
            b"\x1bb"
        );
        assert!(key_bytes(Key::A, none, TermMode::NONE).is_none());
    }

    #[test]
    fn autofill_answers_once() {
        let buf: Arc<Mutex<Vec<u8>>> = Arc::default();
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let writer: Writer = Arc::new(Mutex::new(Box::new(Sink(buf.clone()))));
        let mut a = AutofillState {
            user: Some("bob".into()),
            password: Some("pw".into()),
            tail: String::new(),
        };
        let armed = AtomicBool::new(true);
        a.feed(b"Welcome\r\nrouter login: ", &writer, &armed);
        a.feed(b"Pass", &writer, &armed);
        a.feed(b"word: ", &writer, &armed);
        a.feed(b"\r\nLogin incorrect\r\nlogin: ", &writer, &armed);
        a.feed(b"Password: ", &writer, &armed);
        assert_eq!(&*buf.lock().unwrap(), b"bob\rpw\r");

        // Once the user has typed, prompts are left alone.
        let mut a = AutofillState {
            user: None,
            password: Some("pw".into()),
            tail: String::new(),
        };
        armed.store(false, Ordering::Relaxed);
        a.feed(b"Password: ", &writer, &armed);
        assert_eq!(&*buf.lock().unwrap(), b"bob\rpw\r");
    }

    #[test]
    fn palette_cube() {
        assert_eq!(palette_rgb(16, true), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(
            palette_rgb(231, true),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(palette_rgb(232, true), Rgb { r: 8, g: 8, b: 8 });
    }
}
