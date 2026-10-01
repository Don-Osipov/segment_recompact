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
    /// A key or a paste: it may put text in claude's input box.
    Key,
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
                // Kitty keyboard protocol: Enter with no modifier, as a press.
                (b"13" | b"13;1" | b"13;1:1", b'u') => Some(InputKind::Enter),
                (p, b'u') if p.ends_with(b":3") => Some(InputKind::Passive),
                _ => Some(InputKind::Key),
            };
            (len, kind)
        }
        Some(b']' | b'P' | b'_' | b'^' | b'X') => (string_end(seq), Some(InputKind::Reply)),
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
                out.push((InputKind::Key, buf[i..end].to_vec()));
                i = end;
                continue;
            }
            match buf[i] {
                0x1b => {
                    let (len, kind) = escape(&buf[i..]);
                    let kind = kind.unwrap_or_else(|| {
                        self.in_paste = true;
                        InputKind::Key
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

/// What the user typed into claude's input box since the last Enter, while that was only plain
/// characters and Backspace. Anything else (arrows, Tab, a paste, history) makes it unknown: the
/// box may then hold text this cannot see.
pub struct TypedLine(Option<Vec<u8>>);

impl Default for TypedLine {
    fn default() -> Self {
        TypedLine(Some(Vec::new()))
    }
}

impl TypedLine {
    pub fn feed(&mut self, kind: InputKind, bytes: &[u8]) {
        match kind {
            InputKind::Enter => self.0 = Some(Vec::new()),
            InputKind::Key if bytes.first() == Some(&0x1b) => self.0 = None,
            InputKind::Key => {
                for &b in bytes {
                    let Some(line) = self.0.as_mut() else {
                        return;
                    };
                    match b {
                        0x7f | 0x08 => {
                            line.pop();
                        }
                        0x20..=0x7e => line.push(b),
                        _ => self.0 = None,
                    }
                }
            }
            InputKind::Reply | InputKind::Passive => {}
        }
    }

    /// The box holds exactly `text`.
    pub fn is(&self, text: &str) -> bool {
        self.0.as_deref() == Some(text.as_bytes())
    }
}

struct Input {
    holding: bool,
    held: Vec<(InputKind, Vec<u8>)>,
    /// A key reached claude since the last plain Enter: its input box may hold text.
    dirty: bool,
    /// When a key or Enter last reached claude.
    last: Option<Instant>,
    line: TypedLine,
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
                    input.line = TypedLine::default();
                    input.dirty = false;
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
        match kind {
            InputKind::Key => input.dirty = true,
            InputKind::Enter => input.dirty = false,
            InputKind::Reply | InputKind::Passive => return,
        }
        input.last = Some(Instant::now());
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
                dirty: false,
                last: None,
                line: TypedLine::default(),
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
        let (master, slave) = open_pty()?;
        let ws = window_size();
        unsafe {
            libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
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
        self.raw();
        let child = cmd.spawn();
        // The child has its copies; the parent's must close, or the master never sees the end.
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        drop(slave);
        let child = match child {
            Ok(c) => c,
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

    /// Nothing typed into claude since the last Enter, and nothing held: its input box is empty.
    pub fn input_clean(&self) -> bool {
        let input = self.shared.input.lock().unwrap();
        !input.dirty && input.held.is_empty()
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
