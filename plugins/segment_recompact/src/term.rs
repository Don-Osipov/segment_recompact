//! The terminal between the user and claude.
//!
//! `recompact shell` runs claude on a pseudo-terminal it owns and copies bytes both ways, so it
//! can type into the running claude (`/resume <twin>`: compaction without a restart, so background
//! shells, monitors and agents keep running) and hold the user's keys for the moment that takes.
//! Nothing else changes what either side sees.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

static WINCH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: libc::c_int) {
    WINCH.store(true, Ordering::SeqCst);
}

/// What a run of bytes from the user's terminal is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputKind {
    /// A key: it may change what claude's input box holds.
    Key,
    /// Part of a bracketed paste: it puts text in the box.
    Paste,
    /// Plain Enter: the box was submitted (or a picker closed), so it is empty again.
    Enter,
    /// The terminal answering one of claude's queries; claude may be waiting for it.
    Reply,
    /// Focus and mouse reports: input, but never text.
    Passive,
}

/// Splits terminal input into classified runs. A bracketed paste can span reads.
#[derive(Default)]
pub struct InputSplitter {
    in_paste: bool,
}

const PASTE_END: &[u8] = b"\x1b[201~";

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// End of a string-type sequence (OSC, DCS, APC): BEL or ESC \, inclusive.
fn string_end(seq: &[u8]) -> usize {
    for i in 2..seq.len() {
        if seq[i] == 0x07 {
            return i + 1;
        }
        if seq[i] == 0x1b && seq.get(i + 1) == Some(&b'\\') {
            return i + 2;
        }
    }
    seq.len()
}

/// A kitty keyboard protocol key (`CSI code[:alts] ; mods[:event] [; text] u`): the key code,
/// the modifier bits (shift 1, alt 2, ctrl 4, super 8; lock keys left out), and the event (1
/// press, 2 repeat, 3 release).
fn csi_u(params: &[u8]) -> Option<(u32, u32, u32)> {
    let text = std::str::from_utf8(params).ok()?;
    let mut fields = text.split(';');
    let code = fields.next()?.split(':').next()?.parse().ok()?;
    let mut mods = fields.next().unwrap_or("").split(':');
    let m: u32 = mods
        .next()
        .filter(|m| !m.is_empty())
        .map_or(Some(1), |m| m.parse().ok())?;
    let event = mods.next().map_or(Some(1), |e| e.parse().ok())?;
    Some((code, m.saturating_sub(1) & !(64 | 128), event))
}

/// Length and kind of the escape sequence at the start of `seq` (which begins with ESC).
/// `None` as the kind marks the start of a bracketed paste.
fn escape(seq: &[u8]) -> (usize, Option<InputKind>) {
    match seq.get(1) {
        None => (1, Some(InputKind::Key)),
        Some(b'[') => {
            let mut i = 2;
            while i < seq.len() && (0x30..=0x3f).contains(&seq[i]) {
                i += 1;
            }
            let params = &seq[2..i];
            while i < seq.len() && (0x20..=0x2f).contains(&seq[i]) {
                i += 1;
            }
            let Some(&fin) = seq.get(i) else {
                return (seq.len(), Some(InputKind::Key));
            };
            let len = i + 1;
            if !(0x40..=0x7e).contains(&fin) {
                return (len, Some(InputKind::Key));
            }
            let kind = match (params, fin) {
                (b"200", b'~') => None,
                (b"", b'I' | b'O') => Some(InputKind::Passive),
                // X10 mouse: three raw bytes follow the final.
                (b"", b'M') => return ((len + 3).min(seq.len()), Some(InputKind::Passive)),
                (p, b'M' | b'm') if p.starts_with(b"<") => Some(InputKind::Passive),
                (p, _) if p.starts_with(b"?") || p.starts_with(b">") || p.starts_with(b"=") => {
                    Some(InputKind::Reply)
                }
                (_, b'R' | b'n' | b't') => Some(InputKind::Reply),
                (p, b'u') => match csi_u(p) {
                    Some((_, _, 3)) => Some(InputKind::Passive),
                    // Enter, or keypad Enter, with no modifier.
                    Some((13 | 57414, 0, _)) => Some(InputKind::Enter),
                    _ => Some(InputKind::Key),
                },
                _ => Some(InputKind::Key),
            };
            (len, kind)
        }
        Some(b']' | b'P' | b'_' | b'^' | b'X') => (string_end(seq), Some(InputKind::Reply)),
        Some(b'O') if seq.get(2) == Some(&b'M') => (3, Some(InputKind::Enter)),
        Some(b'O') => (seq.len().min(3), Some(InputKind::Key)),
        Some(&c) => {
            // Alt + a key; a multi-byte character stays whole.
            let n = if c >= 0xc0 {
                1 + (c.leading_ones() as usize).clamp(1, 4)
            } else {
                2
            };
            (n.min(seq.len()), Some(InputKind::Key))
        }
    }
}

impl InputSplitter {
    pub fn split(&mut self, buf: &[u8]) -> Vec<(InputKind, Vec<u8>)> {
        let mut out: Vec<(InputKind, Vec<u8>)> = Vec::new();
        let mut i = 0;
        while i < buf.len() {
            if self.in_paste {
                let end = match find(&buf[i..], PASTE_END) {
                    Some(p) => {
                        self.in_paste = false;
                        i + p + PASTE_END.len()
                    }
                    None => buf.len(),
                };
                out.push((InputKind::Paste, buf[i..end].to_vec()));
                i = end;
                continue;
            }
            match buf[i] {
                0x1b => {
                    let (len, kind) = escape(&buf[i..]);
                    let kind = kind.unwrap_or_else(|| {
                        self.in_paste = true;
                        InputKind::Paste
                    });
                    out.push((kind, buf[i..i + len].to_vec()));
                    i += len;
                }
                b'\r' => {
                    out.push((InputKind::Enter, vec![b'\r']));
                    i += 1;
                }
                _ => {
                    let start = i;
                    while i < buf.len() && buf[i] != 0x1b && buf[i] != b'\r' {
                        i += 1;
                    }
                    out.push((InputKind::Key, buf[start..i].to_vec()));
                }
            }
        }
        out
    }
}

/// What a key does to claude's input box.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Effect {
    /// Types this printable character.
    Char(u8),
    /// Puts in text this does not follow (a newline, a paste, non-ASCII text).
    Insert,
    /// Deletes one character (Backspace).
    Erase,
    /// Empties the box (Ctrl+C).
    Clear,
    /// May replace the box with text from elsewhere (history, completion, the clipboard, an
    /// editor).
    Recall,
    /// May delete text, never adds any.
    Delete,
    /// Moves the cursor.
    Move,
    /// Leaves the text alone (Esc, Shift+Tab, function keys, other shortcuts).
    Other,
}

/// A control character's effect, with Ctrl+letter given as the letter.
fn ctrl_effect(letter: u8) -> Effect {
    match letter {
        b'c' => Effect::Clear,
        b'i' | b'r' | b'v' | b'y' | b'g' | b'p' | b'n' => Effect::Recall,
        b'j' => Effect::Insert,
        b'h' => Effect::Erase,
        b'k' | b'u' | b'w' | b'd' => Effect::Delete,
        b'a' | b'e' | b'b' | b'f' => Effect::Move,
        _ => Effect::Other,
    }
}

/// The effect of one key that came as an escape sequence.
fn escape_effect(seq: &[u8]) -> Effect {
    let (params, fin) = match seq {
        [0x1b] => return Effect::Other,
        [0x1b, b'[', rest @ ..] | [0x1b, b'O', rest @ ..] if !rest.is_empty() => {
            (&rest[..rest.len() - 1], rest[rest.len() - 1])
        }
        // Alt + a key.
        [0x1b, 0x7f] | [0x1b, b'd'] => return Effect::Delete,
        [0x1b, b'b'] | [0x1b, b'f'] => return Effect::Move,
        [0x1b, b'\r'] => return Effect::Insert,
        [0x1b, b'v'] => return Effect::Recall,
        _ => return Effect::Other,
    };
    match fin {
        b'A' | b'B' => Effect::Recall,
        b'C' | b'D' | b'H' | b'F' => Effect::Move,
        b'~' => match params.split(|&b| b == b';').next().unwrap_or(b"") {
            b"3" => Effect::Delete,
            b"1" | b"4" | b"7" | b"8" => Effect::Move,
            _ => Effect::Other,
        },
        b'u' => match csi_u(params) {
            Some((13, 0, _)) => Effect::Other,
            Some((13, _, _)) => Effect::Insert,
            Some((9, 0, _)) => Effect::Recall,
            Some((127, 0, _)) => Effect::Erase,
            Some((127, _, _)) => Effect::Delete,
            Some((code @ 97..=122, 4, _)) => ctrl_effect(code as u8),
            Some((code @ 97..=122, 2, _)) => match code as u8 {
                b'b' | b'f' => Effect::Move,
                b'd' => Effect::Delete,
                b'v' => Effect::Recall,
                _ => Effect::Other,
            },
            _ => Effect::Other,
        },
        _ => Effect::Other,
    }
}

/// What claude's input box holds, followed from the keys sent to it.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum InputBox {
    #[default]
    Empty,
    /// Exactly this text, typed as plain characters.
    Text(Vec<u8>),
    /// Something, not followed exactly.
    Draft,
    /// Maybe something (after history recall, completion, or a deletion in a draft).
    Unknown,
}

impl InputBox {
    pub fn feed(&mut self, kind: InputKind, bytes: &[u8]) {
        match kind {
            InputKind::Enter => *self = InputBox::Empty,
            InputKind::Paste => self.apply(Effect::Insert),
            InputKind::Key if bytes.first() == Some(&0x1b) => self.apply(escape_effect(bytes)),
            InputKind::Key => {
                for &b in bytes {
                    self.apply(match b {
                        0x20..=0x7e => Effect::Char(b),
                        0x7f => Effect::Erase,
                        0x80..=0xff => Effect::Insert,
                        c => ctrl_effect(c + 0x60),
                    });
                }
            }
            InputKind::Reply | InputKind::Passive => {}
        }
    }

    fn apply(&mut self, effect: Effect) {
        use InputBox::*;
        let next = match (effect, std::mem::take(self)) {
            (Effect::Char(c), Empty) => Text(vec![c]),
            (Effect::Char(c), Text(mut t)) => {
                t.push(c);
                Text(t)
            }
            (Effect::Char(_) | Effect::Insert, _) => Draft,
            (Effect::Erase, Text(mut t)) => {
                t.pop();
                if t.is_empty() {
                    Empty
                } else {
                    Text(t)
                }
            }
            (Effect::Clear, _) => Empty,
            (Effect::Recall, _) => Unknown,
            (Effect::Erase | Effect::Delete | Effect::Move | Effect::Other, Empty) => Empty,
            (Effect::Erase | Effect::Delete, _) => Unknown,
            (Effect::Move, Text(_)) => Draft,
            (Effect::Move | Effect::Other, state) => state,
        };
        *self = next;
    }

    /// The box holds exactly `text`.
    pub fn is(&self, text: &str) -> bool {
        matches!(self, InputBox::Text(t) if t == text.as_bytes())
    }
}

struct Input {
    holding: bool,
    held: Vec<(InputKind, Vec<u8>)>,
    /// When a key or Enter last reached claude.
    last: Option<Instant>,
    line: InputBox,
    /// Commands the launcher handles itself when typed and entered as they are.
    commands: Vec<String>,
    /// The last such command, for the launcher to pick up.
    typed: Option<String>,
}

/// What claude's terminal title says about it. Claude Code shows `✳ <title>` while idle and
/// turns the first character into a spinner (`◐`, `◑`, …) while it works, also while a long
/// tool call runs or the terminal is in the background (measured, CLI 2.1.286).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TitleState {
    Idle,
    Busy,
}

/// Classify a terminal title. Titles that are not Claude Code's own (hooks can set one) say
/// nothing.
pub fn title_state(title: &str) -> Option<TitleState> {
    match title.chars().next()? {
        '✳' => Some(TitleState::Idle),
        '◐'..='◓' | '\u{2801}'..='\u{28ff}' => Some(TitleState::Busy),
        _ => None,
    }
}

/// Finds the titles (`OSC 0` and `OSC 2`) in a terminal output stream; a title can be split
/// across reads.
#[derive(Default)]
pub struct TitleWatch {
    partial: Vec<u8>,
}

impl TitleWatch {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut data = std::mem::take(&mut self.partial);
        data.extend_from_slice(bytes);
        let mut titles = Vec::new();
        let mut i = 0;
        while let Some(at) = data[i..]
            .windows(2)
            .position(|w| w == b"\x1b]")
            .map(|p| i + p)
        {
            let body = at + 2;
            // BEL or ESC \ ends it; any other escape cancels it.
            let end = (body..data.len()).find_map(|j| match data[j] {
                0x07 => Some((j, j + 1, true)),
                0x1b if data.get(j + 1) == Some(&b'\\') => Some((j, j + 2, true)),
                0x1b if j + 1 < data.len() => Some((j, j, false)),
                _ => None,
            });
            let Some((stop, next, ended)) = end else {
                // Unfinished: keep it for the next read, unless it cannot be a title.
                if data.len() - at < 4096 {
                    self.partial = data[at..].to_vec();
                }
                return titles;
            };
            let osc = &data[body..stop];
            if let Some(t) = osc
                .strip_prefix(b"0;")
                .or_else(|| osc.strip_prefix(b"2;"))
                .filter(|_| ended)
            {
                titles.push(String::from_utf8_lossy(t).into_owned());
            }
            i = next.max(at + 1);
        }
        if data.last() == Some(&0x1b) {
            self.partial = vec![0x1b];
        }
        titles
    }
}

struct Title {
    state: Option<TitleState>,
    since: Instant,
}

struct Shared {
    master: AtomicI32,
    input: Mutex<Input>,
    title: Mutex<Title>,
}

fn write_all(fd: RawFd, mut bytes: &[u8]) -> bool {
    while !bytes.is_empty() {
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        bytes = &bytes[n as usize..];
    }
    true
}

impl Shared {
    /// Deliver one run to claude, or keep it for the next claude when none is running.
    fn forward(&self, input: &mut Input, kind: InputKind, bytes: Vec<u8>) {
        let fd = self.master.load(Ordering::SeqCst);
        // One of the launcher's commands, typed and entered: erase it instead of submitting it,
        // so claude never sees it (a hook that blocks it reads as an error).
        if fd >= 0 && kind == InputKind::Enter {
            if let Some(cmd) = input.commands.iter().find(|c| input.line.is(c)).cloned() {
                if write_all(fd, &vec![0x7f; cmd.len()]) {
                    input.line = InputBox::Empty;
                    input.last = Some(Instant::now());
                    input.typed = Some(cmd);
                    return;
                }
            }
        }
        if fd < 0 || !write_all(fd, &bytes) {
            if kind != InputKind::Reply {
                input.held.push((kind, bytes));
            }
            return;
        }
        input.line.feed(kind, &bytes);
        if matches!(kind, InputKind::Key | InputKind::Paste | InputKind::Enter) {
            input.last = Some(Instant::now());
        }
    }
}

fn input_loop(shared: Arc<Shared>) {
    let mut splitter = InputSplitter::default();
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if n == 0 {
            return;
        }
        let mut input = shared.input.lock().unwrap();
        for (kind, bytes) in splitter.split(&buf[..n as usize]) {
            if input.holding && kind != InputKind::Reply {
                input.held.push((kind, bytes));
            } else {
                shared.forward(&mut input, kind, bytes);
            }
        }
    }
}

/// Copy claude's screen to the user's until its terminal closes, or until `stop` once nothing is
/// left to read (a process claude left behind can keep the terminal open after claude exits).
fn output_loop(master: RawFd, stop: Arc<AtomicBool>, shared: Arc<Shared>) {
    let mut buf = [0u8; 65536];
    let mut watch = TitleWatch::default();
    loop {
        let mut pfd = libc::pollfd {
            fd: master,
            events: libc::POLLIN,
            revents: 0,
        };
        match unsafe { libc::poll(&mut pfd, 1, 100) } {
            0 if stop.load(Ordering::SeqCst) => return,
            0 => continue,
            // Interrupted (SIGWINCH): poll again rather than block in read.
            n if n < 0 => continue,
            _ => {}
        }
        let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if n == 0 || !write_all(1, &buf[..n as usize]) {
            return;
        }
        for t in watch.feed(&buf[..n as usize]) {
            if let Some(state) = title_state(&t) {
                let mut title = shared.title.lock().unwrap();
                if title.state != Some(state) {
                    *title = Title {
                        state: Some(state),
                        since: Instant::now(),
                    };
                }
            }
        }
    }
}

fn window_size() -> libc::winsize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    for fd in [1, 0, 2] {
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
            return ws;
        }
    }
    ws.ws_row = 24;
    ws.ws_col = 80;
    ws
}

fn cloexec(fd: RawFd) {
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
}

/// The text in terminal output: escape sequences, control characters and spacing left out (a
/// screen drawn with cursor moves has no reliable spaces).
pub fn plain_text(bytes: &[u8]) -> String {
    let mut kept = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => i += escape(&bytes[i..]).0.max(1),
            b if b <= 0x20 || b == 0x7f => i += 1,
            b => {
                kept.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&kept)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// A new pseudo-terminal: (master, slave).
fn open_pty() -> io::Result<(File, File)> {
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master < 0 {
            return Err(io::Error::last_os_error());
        }
        let master_file = File::from_raw_fd(master);
        cloexec(master);
        if libc::grantpt(master) != 0 || libc::unlockpt(master) != 0 {
            return Err(io::Error::last_os_error());
        }
        let name = libc::ptsname(master);
        if name.is_null() {
            return Err(io::Error::last_os_error());
        }
        let slave = libc::open(name, libc::O_RDWR | libc::O_NOCTTY);
        if slave < 0 {
            return Err(io::Error::last_os_error());
        }
        cloexec(slave);
        Ok((master_file, File::from_raw_fd(slave)))
    }
}

/// Run `cmd` as the session leader of a new pseudo-terminal of size `ws`: the child, and the
/// master side to talk to it through.
fn spawn_on_pty(cmd: &mut Command, ws: &libc::winsize) -> io::Result<(Child, File)> {
    let (master, slave) = open_pty()?;
    unsafe {
        libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, ws);
    }
    cmd.stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?));
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn();
    // The child has its copies; the parent's must close, or the master never sees the end.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    drop(slave);
    Ok((child?, master))
}

/// The size of the terminal `tty` names (`ttys013`, `pts/4`, as `ps` prints it).
pub fn tty_size(tty: &str) -> Option<(u16, u16)> {
    let tty = tty.trim();
    if tty.is_empty() || tty.starts_with('?') || tty.contains("..") {
        return None;
    }
    let path = std::ffi::CString::new(format!("/dev/{tty}")).ok()?;
    unsafe {
        let fd = libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_NOCTTY | libc::O_NONBLOCK,
        );
        if fd < 0 {
            return None;
        }
        let mut ws: libc::winsize = std::mem::zeroed();
        let ok = libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0;
        libc::close(fd);
        (ok && ws.ws_row > 0 && ws.ws_col > 0).then_some((ws.ws_row, ws.ws_col))
    }
}

#[derive(Default)]
struct Drawn {
    /// Bytes drawn since the start.
    total: u64,
    /// The last of them.
    tail: Vec<u8>,
    last: Option<Instant>,
}

const DRAWN_TAIL: usize = 64 << 10;

/// A terminal recompact drives with no one at it: a background job opened with `claude attach`
/// on a pseudo-terminal of recompact's own. It keeps the end of what the job draws, so a caller
/// can read what a key did.
pub struct Attach {
    child: Child,
    master: File,
    drawn: Arc<Mutex<Drawn>>,
    reader: Option<(JoinHandle<()>, Arc<AtomicBool>)>,
}

impl Attach {
    pub fn open(cmd: &mut Command, rows: u16, cols: u16) -> io::Result<Attach> {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let (child, master) = spawn_on_pty(cmd, &ws)?;
        let drawn = Arc::new(Mutex::new(Drawn::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (fd, sink, flag) = (master.as_raw_fd(), drawn.clone(), stop.clone());
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            loop {
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                match unsafe { libc::poll(&mut pfd, 1, 100) } {
                    0 if flag.load(Ordering::SeqCst) => return,
                    0 => continue,
                    n if n < 0 => continue,
                    _ => {}
                }
                let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if n <= 0 {
                    return;
                }
                let mut d = sink.lock().unwrap();
                d.total += n as u64;
                d.tail.extend_from_slice(&buf[..n as usize]);
                if d.tail.len() > DRAWN_TAIL {
                    let cut = d.tail.len() - DRAWN_TAIL;
                    d.tail.drain(..cut);
                }
                d.last = Some(Instant::now());
            }
        });
        Ok(Attach {
            child,
            master,
            drawn,
            reader: Some((reader, stop)),
        })
    }

    pub fn type_keys(&self, bytes: &[u8]) -> bool {
        write_all(self.master.as_raw_fd(), bytes)
    }

    /// How much has been drawn: a mark for `text_since`.
    pub fn mark(&self) -> u64 {
        self.drawn.lock().unwrap().total
    }

    /// The text drawn since `mark`, as far back as the kept end reaches (see `plain_text`).
    pub fn text_since(&self, mark: u64) -> String {
        let d = self.drawn.lock().unwrap();
        let new = (d.total.saturating_sub(mark) as usize).min(d.tail.len());
        plain_text(&d.tail[d.tail.len() - new..])
    }

    /// How long since anything was drawn; `None` before the first output.
    pub fn still_for(&self) -> Option<Duration> {
        self.drawn.lock().unwrap().last.map(|t| t.elapsed())
    }

    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Leave, with the job running: Ctrl+Z ends `claude attach` (measured: exit 0, CLI 2.1.293).
    /// A client that stays is stopped when this is dropped.
    pub fn detach(mut self) {
        self.type_keys(b"\x1a");
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Attach {
    fn drop(&mut self) {
        if self.running() {
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while self.running() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        // The reader polls the master's descriptor: it must be done before the master closes.
        if let Some((h, stop)) = self.reader.take() {
            stop.store(true, Ordering::SeqCst);
            let _ = h.join();
        }
    }
}

/// The proxy for one launcher: the user's terminal on one side, the current claude's
/// pseudo-terminal on the other.
pub struct Proxy {
    shared: Arc<Shared>,
    /// The user's terminal mode before raw mode, when stdin is a terminal.
    saved: Option<libc::termios>,
    /// The user's stderr. While claude draws the screen, fd 2 goes to `log` instead, so nothing
    /// the launcher prints (or compaction, running alongside) lands in the middle of it.
    real_err: RawFd,
    log: File,
    master: Mutex<Option<File>>,
    output: Mutex<Option<(JoinHandle<()>, Arc<AtomicBool>)>>,
}

impl Proxy {
    /// Get ready to run claude on a terminal of its own: a reader on stdin, window-size
    /// tracking, and `log` for what the launcher prints while claude is on screen.
    pub fn start(log: &std::path::Path) -> Option<Proxy> {
        let saved = unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            (libc::isatty(0) == 1 && libc::tcgetattr(0, &mut t) == 0).then_some(t)
        };
        if let Some(dir) = log.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .ok()?;
        cloexec(log.as_raw_fd());
        let real_err = unsafe { libc::dup(2) };
        if real_err < 0 {
            return None;
        }
        cloexec(real_err);
        unsafe {
            libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t);
        }
        let shared = Arc::new(Shared {
            master: AtomicI32::new(-1),
            input: Mutex::new(Input {
                holding: false,
                held: Vec::new(),
                last: None,
                line: InputBox::Empty,
                commands: Vec::new(),
                typed: None,
            }),
            title: Mutex::new(Title {
                state: None,
                since: Instant::now(),
            }),
        });
        let reader = shared.clone();
        std::thread::spawn(move || input_loop(reader));
        Some(Proxy {
            shared,
            saved,
            real_err,
            log,
            master: Mutex::new(None),
            output: Mutex::new(None),
        })
    }

    fn raw(&self) {
        if let Some(saved) = self.saved {
            let mut t = saved;
            unsafe {
                libc::cfmakeraw(&mut t);
                libc::tcsetattr(0, libc::TCSANOW, &t);
            }
        }
    }

    fn cooked(&self) {
        if let Some(saved) = self.saved {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &saved);
            }
        }
    }

    /// Run `cmd` as the session leader of a fresh pseudo-terminal sized like the user's.
    pub fn spawn(&self, cmd: &mut Command) -> io::Result<Child> {
        // A new claude starts with an empty input box.
        self.shared.input.lock().unwrap().line = InputBox::Empty;
        self.raw();
        let (child, master) = match spawn_on_pty(cmd, &window_size()) {
            Ok(v) => v,
            Err(e) => {
                self.cooked();
                return Err(e);
            }
        };
        unsafe {
            libc::dup2(self.log.as_raw_fd(), 2);
        }
        let fd = master.as_raw_fd();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        *self.shared.title.lock().unwrap() = Title {
            state: None,
            since: Instant::now(),
        };
        let shared = self.shared.clone();
        *self.output.lock().unwrap() = Some((
            std::thread::spawn(move || output_loop(fd, flag, shared)),
            stop,
        ));
        *self.master.lock().unwrap() = Some(master);
        self.shared.master.store(fd, Ordering::SeqCst);
        Ok(child)
    }

    /// After claude exits: drain its last output, close its terminal, give the user's back.
    pub fn child_gone(&self) {
        self.shared.master.store(-1, Ordering::SeqCst);
        if let Some((h, stop)) = self.output.lock().unwrap().take() {
            let deadline = Instant::now() + Duration::from_secs(1);
            while !h.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            stop.store(true, Ordering::SeqCst);
            let _ = h.join();
        }
        self.master.lock().unwrap().take();
        self.cooked();
        unsafe {
            libc::dup2(self.real_err, 2);
        }
    }

    /// Follow the user's window size (call often).
    pub fn tick(&self) {
        if WINCH.swap(false, Ordering::SeqCst) {
            self.sync_size();
        }
    }

    fn sync_size(&self) {
        let fd = self.shared.master.load(Ordering::SeqCst);
        if fd >= 0 {
            let ws = window_size();
            unsafe {
                libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
            }
        }
    }

    /// Ctrl-Z stopped claude: give the terminal back before the launcher stops too.
    pub fn suspend(&self) {
        self.cooked();
    }

    pub fn resume(&self) {
        self.raw();
        self.sync_size();
    }

    /// Type into claude as if from the keyboard.
    pub fn type_keys(&self, bytes: &[u8]) -> bool {
        let fd = self.shared.master.load(Ordering::SeqCst);
        fd >= 0 && write_all(fd, bytes)
    }

    /// Keep the user's keys from claude (terminal replies still pass) until `release`.
    pub fn hold(&self) {
        self.shared.input.lock().unwrap().holding = true;
    }

    pub fn holding(&self) -> bool {
        self.shared.input.lock().unwrap().holding
    }

    /// Deliver the held keys, in order.
    pub fn release(&self) {
        let mut input = self.shared.input.lock().unwrap();
        input.holding = false;
        for (kind, bytes) in std::mem::take(&mut input.held) {
            self.shared.forward(&mut input, kind, bytes);
        }
    }

    /// Handle these commands when typed as they are, instead of passing them to claude.
    pub fn intercept(&self, commands: &[&str]) {
        let mut input = self.shared.input.lock().unwrap();
        if input
            .commands
            .iter()
            .map(String::as_str)
            .ne(commands.iter().copied())
        {
            input.commands = commands.iter().map(|c| c.to_string()).collect();
        }
    }

    /// A command from `intercept` the user typed since the last call.
    pub fn take_typed(&self) -> Option<String> {
        self.shared.input.lock().unwrap().typed.take()
    }

    /// Claude's input box is empty and no keys are held for it.
    pub fn input_clean(&self) -> bool {
        let input = self.shared.input.lock().unwrap();
        input.line == InputBox::Empty && input.held.is_empty()
    }

    /// What claude's input box holds, as far as the keys sent to it tell.
    pub fn input_box(&self) -> InputBox {
        self.shared.input.lock().unwrap().line.clone()
    }

    /// How long claude's title has said it is idle (zero while it says busy); `None` until it
    /// has set a title of its own.
    pub fn title_idle_for(&self) -> Option<Duration> {
        let title = self.shared.title.lock().unwrap();
        title.state.map(|s| match s {
            TitleState::Idle => title.since.elapsed(),
            TitleState::Busy => Duration::ZERO,
        })
    }

    /// How long since the user's last key reached claude.
    pub fn quiet_for(&self) -> Duration {
        self.shared
            .input
            .lock()
            .unwrap()
            .last
            .map_or(Duration::MAX, |t| t.elapsed())
    }

    /// A line on the user's terminal, for when claude is not drawing it.
    pub fn say(&self, line: &str) {
        write_all(self.real_err, format!("{line}\r\n").as_bytes());
    }
}

static LED: AtomicI32 = AtomicI32::new(0);

extern "C" fn pass_term(sig: libc::c_int) {
    let pid = LED.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

/// `recompact pty-leader <bin> [args]`: lead the session of claude's terminal so that claude
/// is not its session leader. A session leader's process group has no parent inside its session,
/// so the kernel drops the SIGTSTP claude sends itself on Ctrl-Z; in a process group of its own
/// under this process, claude stops as it does under a shell. A stop and an exit pass through to
/// the launcher (this process stops, or exits with claude's code); SIGTERM passes to claude. The
/// launcher learns claude's pid from `$RECOMPACT_SHELL/child.json`.
pub fn cmd_pty_leader(args: &[String]) -> i32 {
    let Some((bin, rest)) = args.split_first() else {
        eprintln!("usage: recompact pty-leader <command> [args]");
        return 2;
    };
    unsafe {
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, pass_term as *const () as libc::sighandler_t);
    }
    let child = unsafe { libc::fork() };
    if child < 0 {
        return 127;
    }
    if child == 0 {
        use std::os::unix::process::CommandExt;
        unsafe {
            libc::setpgid(0, 0);
            libc::tcsetpgrp(0, libc::getpid());
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
        }
        let err = Command::new(bin).args(rest).exec();
        eprintln!("recompact: cannot run {bin}: {err}");
        unsafe { libc::_exit(127) }
    }
    LED.store(child, Ordering::SeqCst);
    unsafe {
        libc::setpgid(child, child);
        libc::tcsetpgrp(0, child);
    }
    if let Some(dir) = std::env::var_os("RECOMPACT_SHELL") {
        let path = std::path::Path::new(&dir).join("child.json");
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, format!("{{\"pid\":{child}}}")).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
    loop {
        let mut status = 0;
        if unsafe { libc::waitpid(child, &mut status, libc::WUNTRACED) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return 1;
        }
        if libc::WIFSTOPPED(status) {
            unsafe {
                libc::raise(libc::SIGSTOP);
                libc::tcsetpgrp(0, child);
                libc::kill(-child, libc::SIGCONT);
            }
        } else if libc::WIFEXITED(status) {
            return libc::WEXITSTATUS(status);
        } else if libc::WIFSIGNALED(status) {
            return 128 + libc::WTERMSIG(status);
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.cooked();
        unsafe {
            libc::dup2(self.real_err, 2);
        }
    }
}
