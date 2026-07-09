use crate::shared::{
    display_command, shell_command, validate_session_name, Request, Response, SessionRecord,
    SessionState, StayError, StayResult, VERSION,
};
use chrono::SecondsFormat;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::pty::{openpty, OpenptyResult, Winsize};
use nix::sys::signal::{kill, Signal};
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg, Termios};
use nix::unistd::{read as nix_read, setsid, write as nix_write, Pid};
use std::borrow::Cow;
use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const DAEMON_ARG: &str = "__stay_daemon";
const ALT_SCREEN_ENTER: &str = "\x1b[?1049h";
const ALT_SCREEN_EXIT: &str = "\x1b[?1049l";
const CLEAR_SCREEN: &str = "\x1b[2J\x1b[H";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
/// Enable SGR mouse reporting with drag motion so Stay can provide scrollback
/// and selection inside the alternate screen.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
/// Turn off the mouse reporting enabled by `MOUSE_ON`.
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";
const BOLD: &str = "\x1b[1m";
const ACCENT: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const INVERT_ON: &str = "\x1b[7m";
const INVERT_OFF: &str = "\x1b[27m";

/// Delay between transition animation frames. Lower is faster.
const FRAME_DELAY_MS: u64 = 36;
/// Maximum bytes of per-session output kept for replay on re-attach.
const SCROLLBACK_LIMIT: usize = 2 * 1024 * 1024;
/// Lines of scrollback the client's emulator keeps for wheel scrolling.
const SCROLLBACK_LINES: usize = 10_000;
/// Lines moved per wheel notch.
const SCROLL_STEP: usize = 3;
/// How often drag-selection scrolls while the pointer is held at an edge.
const AUTO_SCROLL_INTERVAL_MS: u16 = 60;

#[derive(Clone)]
struct Paths {
    state_dir: PathBuf,
    sessions_dir: PathBuf,
    daemon_socket: PathBuf,
}

struct ManagedSession {
    record: SessionRecord,
    master: Option<File>,
    io: SharedIo,
    attached: bool,
}

type Sessions = Arc<Mutex<HashMap<String, ManagedSession>>>;
type SharedIo = Arc<Mutex<SessionIo>>;

/// Live output plumbing for one session.
///
/// A background reader thread drains the PTY into `buffer` even while no client
/// is attached, so nothing produced in the background is lost. On attach the
/// whole buffer is replayed, then `subscriber` receives live output.
struct SessionIo {
    buffer: Vec<u8>,
    subscriber: Option<UnixStream>,
}

impl SessionIo {
    fn new() -> Self {
        Self {
            buffer: Vec::new(),
            subscriber: None,
        }
    }

    fn shared() -> SharedIo {
        Arc::new(Mutex::new(Self::new()))
    }

    /// Append a chunk of PTY output to the replay buffer, capped to the most
    /// recent `SCROLLBACK_LIMIT` bytes and trimmed at a line boundary.
    fn record(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);

        if self.buffer.len() > SCROLLBACK_LIMIT {
            let overflow = self.buffer.len() - SCROLLBACK_LIMIT;
            let mut cut = overflow;
            if let Some(nl) = self.buffer[cut..].iter().position(|byte| *byte == b'\n') {
                cut += nl + 1;
            }
            self.buffer.drain(..cut.min(self.buffer.len()));
        }
    }
}

/// Remove `ESC [ 3 J`, which clears terminal scrollback. The visible-screen
/// clear (`ESC [ 2 J`) is left intact, but Stay keeps prior output available on
/// reattach instead of letting a shell `clear` erase the replay history.
fn strip_clear_scrollback(chunk: &[u8]) -> Cow<'_, [u8]> {
    const SEQ: &[u8] = b"\x1b[3J";
    if chunk.len() < SEQ.len() {
        return Cow::Borrowed(chunk);
    }
    if !chunk.windows(SEQ.len()).any(|window| window == SEQ) {
        return Cow::Borrowed(chunk);
    }

    let mut filtered = Vec::with_capacity(chunk.len());
    let mut i = 0;
    while i < chunk.len() {
        if chunk[i..].starts_with(SEQ) {
            i += SEQ.len();
        } else {
            filtered.push(chunk[i]);
            i += 1;
        }
    }
    Cow::Owned(filtered)
}

pub fn run() -> StayResult<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();

    if args.first().is_some_and(|arg| arg == DAEMON_ARG) {
        return run_daemon();
    }

    match args.as_slice() {
        [] => {
            print_usage();
            Ok(())
        }
        [flag] if is_help_flag(flag) => {
            print_usage();
            Ok(())
        }
        [flag] if flag == "--version" || flag == "-V" => {
            println!("stay {VERSION}");
            Ok(())
        }
        [cmd, flag] if cmd == "ls" && is_help_flag(flag) => {
            print_ls_usage();
            Ok(())
        }
        [cmd] if cmd == "ls" => {
            ensure_daemon()?;
            list_sessions()
        }
        [cmd, ..] if cmd == "ls" => Err(StayError::new("Usage: stay ls")),
        [cmd] if cmd == "completions" || cmd == "completion" => {
            print_completion_usage();
            Ok(())
        }
        [cmd, flag] if (cmd == "completions" || cmd == "completion") && is_help_flag(flag) => {
            print_completion_usage();
            Ok(())
        }
        [cmd, shell] if cmd == "completions" || cmd == "completion" => print_completions(shell),
        [cmd, ..] if cmd == "completions" || cmd == "completion" => {
            Err(StayError::new("Usage: stay completions <bash|zsh|fish>"))
        }
        [cmd, flag] if cmd == "kill" && is_help_flag(flag) => {
            print_kill_usage();
            Ok(())
        }
        [cmd, name] if cmd == "kill" => {
            validate_session_name(name).map_err(StayError::new)?;
            ensure_daemon()?;
            simple_request(Request::Kill { name: name.clone() })
        }
        [cmd, ..] if cmd == "kill" => Err(StayError::new("Usage: stay kill <name>")),
        [cmd, flag] if cmd == "rm" && is_help_flag(flag) => {
            print_rm_usage();
            Ok(())
        }
        [cmd, name] if cmd == "rm" => {
            validate_session_name(name).map_err(StayError::new)?;
            ensure_daemon()?;
            simple_request(Request::Remove { name: name.clone() })
        }
        [cmd, ..] if cmd == "rm" => Err(StayError::new("Usage: stay rm <name>")),
        _ => attach_command(args),
    }
}

fn is_help_flag(arg: &str) -> bool {
    arg == "--help" || arg == "-h"
}

fn attach_command(args: Vec<String>) -> StayResult<()> {
    let separator = args.iter().position(|arg| arg == "--");
    let name = args
        .first()
        .ok_or_else(|| StayError::new("Missing session name."))?
        .clone();
    validate_session_name(&name).map_err(StayError::new)?;

    let command = separator.map(|index| args[index + 1..].to_vec());
    if command.as_ref().is_some_and(|cmd| cmd.is_empty()) {
        return Err(StayError::new("Missing command after --."));
    }

    ensure_daemon()?;
    attach(&name, command, false)
}

fn print_usage() {
    println!("Stay");
    println!("Run a command, press Ctrl+A to leave, come back with stay <name>.");
    println!();
    println!("Usage:");
    println!("  stay <name>");
    println!("  stay <name> -- <command>");
    println!("  stay ls");
    println!("  stay kill <name>");
    println!("  stay rm <name>");
    println!("  stay completions <bash|zsh|fish>");
}

fn print_ls_usage() {
    println!("Usage: stay ls");
    println!();
    println!("List sessions.");
}

fn print_kill_usage() {
    println!("Usage: stay kill <name>");
    println!();
    println!("Kill a running session.");
}

fn print_rm_usage() {
    println!("Usage: stay rm <name>");
    println!();
    println!("Remove a stopped session.");
}

fn run_daemon() -> StayResult<()> {
    let paths = prepare_paths()?;
    if paths.daemon_socket.exists() {
        let _ = fs::remove_file(&paths.daemon_socket);
    }

    let listener = UnixListener::bind(&paths.daemon_socket).map_err(|_| {
        StayError::new(format!(
            "Failed to create socket:\n\n{}\n\nPlease check directory permissions.",
            paths.daemon_socket.display()
        ))
    })?;
    fs::set_permissions(&paths.daemon_socket, fs::Permissions::from_mode(0o600))?;

    let sessions = Arc::new(Mutex::new(load_records(&paths)?));

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let paths = paths.clone();
                let sessions = Arc::clone(&sessions);
                thread::spawn(move || {
                    if let Err(err) = handle_client(stream, sessions, paths) {
                        eprintln!("{err}");
                    }
                });
            }
            Err(err) => eprintln!("{err}"),
        }
    }

    Ok(())
}

fn handle_client(mut stream: UnixStream, sessions: Sessions, paths: Paths) -> StayResult<()> {
    let mut line = String::new();
    read_line_unbuffered(&mut stream, &mut line)?;
    let request = serde_json::from_str::<Request>(&line)?;

    match request {
        Request::Attach {
            name,
            cwd,
            command,
            restart,
            rows,
            cols,
        } => handle_attach(
            stream, sessions, paths, name, cwd, command, restart, rows, cols,
        ),
        Request::Kill { name } => {
            write_command_result(&mut stream, kill_session(&name, &sessions, &paths))
        }
        Request::List => {
            let mut sessions = sessions.lock().expect("sessions lock poisoned");
            let mut list = sessions
                .values_mut()
                .map(|session| {
                    refresh_record_state(&mut session.record);
                    session.record.clone()
                })
                .collect::<Vec<_>>();
            list.sort_by(|a, b| a.name.cmp(&b.name));
            write_response(&mut stream, &Response::Sessions { sessions: list })
        }
        Request::Remove { name } => {
            write_command_result(&mut stream, remove_session(&name, &sessions, &paths))
        }
    }
}

fn write_command_result(stream: &mut UnixStream, result: StayResult<String>) -> StayResult<()> {
    match result {
        Ok(message) => write_response(stream, &Response::Ok { message }),
        Err(err) => write_response(
            stream,
            &Response::Error {
                message: err.to_string(),
            },
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_attach(
    mut stream: UnixStream,
    sessions: Sessions,
    paths: Paths,
    name: String,
    cwd: String,
    command: Option<Vec<String>>,
    restart: bool,
    rows: u16,
    cols: u16,
) -> StayResult<()> {
    let mut start_command = command.clone().unwrap_or_else(shell_command);
    let mut start_cwd = cwd.clone();
    let mut message = String::new();

    {
        let mut sessions_guard = sessions.lock().expect("sessions lock poisoned");
        if let Some(existing) = sessions_guard.get_mut(&name) {
            refresh_record_state(&mut existing.record);

            match existing.record.state {
                SessionState::Running => {
                    if existing.attached {
                        return write_response(
                            &mut stream,
                            &Response::Error {
                                message: format!("Session {name} is already attached."),
                            },
                        );
                    }

                    // Match the live PTY to the terminal we are attaching from
                    // so both the replayed history and new output wrap the same
                    // way this client sees them.
                    if let Some(master) = existing.master.as_ref() {
                        set_pty_size(master, rows, cols);
                    }
                    if command.is_some()
                        && command
                            .as_ref()
                            .is_some_and(|cmd| cmd != &existing.record.command)
                    {
                        message.push_str(&format!(
                            "{DIM}existing session kept; command ignored{RESET}\n"
                        ));
                    }
                    existing.record.last_attached_at = Some(now());
                    write_record(&paths, &existing.record)?;
                }
                SessionState::Exited | SessionState::Stopped if !restart => {
                    return write_response(
                        &mut stream,
                        &Response::NeedsRestart {
                            name,
                            state: existing.record.state.clone(),
                            exit_code: existing.record.exit_code,
                            command: existing.record.command.clone(),
                        },
                    );
                }
                SessionState::Exited | SessionState::Stopped => {
                    start_command = if let Some(command) = command {
                        command
                    } else {
                        existing.record.command.clone()
                    };
                    start_cwd = existing.record.cwd.clone();
                    let (record, master, io) =
                        spawn_session(&name, &start_cwd, &start_command, rows, cols, &paths)?;
                    existing.record = record;
                    existing.master = Some(master);
                    existing.io = io;
                    existing.attached = false;
                }
            }
        } else {
            let (record, master, io) =
                spawn_session(&name, &start_cwd, &start_command, rows, cols, &paths)?;
            sessions_guard.insert(
                name.clone(),
                ManagedSession {
                    record,
                    master: Some(master),
                    io,
                    attached: false,
                },
            );
        }
    }

    write_response(&mut stream, &Response::AttachReady { message })?;
    attach_stream(name, stream, sessions)
}

fn attach_stream(name: String, mut stream: UnixStream, sessions: Sessions) -> StayResult<()> {
    let (master, io) = {
        let mut sessions_guard = sessions.lock().expect("sessions lock poisoned");
        let session = sessions_guard
            .get_mut(&name)
            .ok_or_else(|| StayError::new(format!("Session not found: {name}")))?;
        let master = session
            .master
            .as_ref()
            .ok_or_else(|| StayError::new(format!("Session {name} is stopped.")))?
            .try_clone()?;
        session.attached = true;
        (master, Arc::clone(&session.io))
    };

    let result = serve_attached(&mut stream, &master, &io);

    // Always release the session, even if replay or input pumping failed, so a
    // future attach is not blocked by a stale `attached` flag.
    io.lock().expect("session io lock poisoned").subscriber = None;
    let mut sessions_guard = sessions.lock().expect("sessions lock poisoned");
    if let Some(session) = sessions_guard.get_mut(&name) {
        session.attached = false;
    }

    result
}

/// Replay recorded history, then subscribe to live output and forward input.
///
/// The replay and the subscription happen together under the io lock so the
/// reader thread cannot interleave new output with the replay or drop anything
/// in the gap between them.
fn serve_attached(stream: &mut UnixStream, master: &File, io: &SharedIo) -> StayResult<()> {
    {
        let mut io_guard = io.lock().expect("session io lock poisoned");
        stream.write_all(&io_guard.buffer)?;
        stream.flush()?;
        io_guard.subscriber = Some(stream.try_clone()?);
    }

    pump_input(stream, master)
}

/// Forward client keystrokes to the PTY. Live PTY output flows the other way
/// through the session's reader thread (see `run_pty_reader`), so this only
/// watches the client side and exits when the client detaches or the PTY closes.
fn pump_input(stream: &mut UnixStream, master: &File) -> StayResult<()> {
    let mut from_client = [0_u8; 8192];

    loop {
        let stream_ready = {
            let mut fds = [PollFd::new(
                stream.as_fd(),
                PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR,
            )];
            poll(&mut fds, PollTimeout::NONE).map_err(|err| StayError::new(err.to_string()))?;
            fds[0].revents().unwrap_or(PollFlags::empty())
        };

        if stream_ready.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            break;
        }
        if stream_ready.contains(PollFlags::POLLIN) {
            let read = stream.read(&mut from_client)?;
            if read == 0 {
                break;
            }
            write_all_fd(master, &from_client[..read])?;
        }
    }

    Ok(())
}

fn spawn_session(
    name: &str,
    cwd: &str,
    command: &[String],
    rows: u16,
    cols: u16,
    paths: &Paths,
) -> StayResult<(SessionRecord, File, SharedIo)> {
    let winsize = Winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let OpenptyResult { master, slave } =
        openpty(Some(&winsize), None).map_err(|err| StayError::new(err.to_string()))?;
    let master = unsafe { File::from_raw_fd(master.into_raw_fd()) };
    let slave_file = unsafe { File::from_raw_fd(slave.into_raw_fd()) };

    let mut cmd = Command::new(&command[0]);
    if command.len() > 1 {
        cmd.args(&command[1..]);
    }
    cmd.current_dir(cwd)
        .stdin(Stdio::from(slave_file.try_clone()?))
        .stdout(Stdio::from(slave_file.try_clone()?))
        .stderr(Stdio::from(slave_file));

    unsafe {
        cmd.pre_exec(|| {
            setsid().map_err(|err| io::Error::new(io::ErrorKind::Other, err.to_string()))?;
            if libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn().map_err(|err| {
        StayError::new(format!(
            "Failed to start command:\n\n{}\n\nReason:\n{}",
            display_command(command),
            err
        ))
    })?;
    let pid = child.id() as i32;
    let record = SessionRecord {
        name: name.to_string(),
        cwd: cwd.to_string(),
        command: command.to_vec(),
        state: SessionState::Running,
        pid: Some(pid),
        created_at: now(),
        last_attached_at: Some(now()),
        exit_code: None,
    };
    write_record(paths, &record)?;

    // Continuously drain the PTY into a replay buffer, even while detached, so
    // background output is never lost and can be replayed on the next attach.
    let io = SessionIo::shared();
    let reader_master = master.try_clone()?;
    let reader_io = Arc::clone(&io);
    thread::spawn(move || run_pty_reader(reader_master, reader_io));

    let paths_for_thread = paths.clone();
    let name_for_thread = name.to_string();
    thread::spawn(move || {
        let exit_code = child.wait().ok().and_then(|status| status.code());
        if let Ok(Some(mut record)) = read_record(&paths_for_thread, &name_for_thread) {
            record.state = SessionState::Exited;
            record.pid = None;
            record.exit_code = exit_code;
            let _ = write_record(&paths_for_thread, &record);
        }
    });

    Ok((record, master, io))
}

/// Drain the PTY master for the life of the session: record every byte for
/// replay and forward it to the attached client, if any. When the PTY closes
/// (the child exited), detach the client so it returns to its own terminal.
fn run_pty_reader(master: File, io: SharedIo) {
    let mut buffer = [0_u8; 8192];
    loop {
        match nix_read(master.as_raw_fd(), &mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let chunk = strip_clear_scrollback(&buffer[..read]);
                if chunk.is_empty() {
                    continue;
                }
                let mut io_guard = io.lock().expect("session io lock poisoned");
                io_guard.record(&chunk);
                if let Some(subscriber) = io_guard.subscriber.as_mut() {
                    if subscriber.write_all(&chunk).is_err() {
                        io_guard.subscriber = None;
                    }
                }
            }
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }
    }

    let mut io_guard = io.lock().expect("session io lock poisoned");
    if let Some(subscriber) = io_guard.subscriber.take() {
        let _ = subscriber.shutdown(Shutdown::Both);
    }
}

fn kill_session(name: &str, sessions: &Sessions, paths: &Paths) -> StayResult<String> {
    let pid = {
        let mut sessions_guard = sessions.lock().expect("sessions lock poisoned");
        let session = sessions_guard
            .get_mut(name)
            .ok_or_else(|| StayError::new(format!("Session not found: {name}")))?;
        refresh_record_state(&mut session.record);
        session.record.pid
    };

    let Some(pid) = pid else {
        return Ok(format!("Killed {name}."));
    };
    let pgid = Pid::from_raw(-pid);
    let _ = kill(pgid, Signal::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !process_alive(pid) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    if process_alive(pid) {
        let _ = kill(pgid, Signal::SIGKILL);
    }

    let mut sessions_guard = sessions.lock().expect("sessions lock poisoned");
    if let Some(session) = sessions_guard.get_mut(name) {
        session.record.state = SessionState::Exited;
        session.record.pid = None;
        session.record.exit_code = None;
        session.master = None;
        session.attached = false;
        write_record(paths, &session.record)?;
    }

    Ok(format!("Killed {name}."))
}

fn remove_session(name: &str, sessions: &Sessions, paths: &Paths) -> StayResult<String> {
    let mut sessions_guard = sessions.lock().expect("sessions lock poisoned");
    {
        let Some(session) = sessions_guard.get_mut(name) else {
            return Err(StayError::new(format!("Session not found: {name}")));
        };
        refresh_record_state(&mut session.record);

        if session.record.state == SessionState::Running {
            return Err(StayError::new(format!(
                "{name} is still running.\nRun `stay kill {name}` first."
            )));
        }
    }

    sessions_guard.remove(name);
    let record_path = session_path(paths, name);
    if record_path.exists() {
        fs::remove_file(record_path)?;
    }

    Ok(format!("Removed {name}."))
}

fn refresh_record_state(record: &mut SessionRecord) {
    if record.state == SessionState::Running {
        match record.pid {
            Some(pid) if process_alive(pid) => {}
            Some(_) => {
                record.state = SessionState::Exited;
                record.pid = None;
            }
            None => record.state = SessionState::Stopped,
        }
    }
}

fn process_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

fn ensure_daemon() -> StayResult<()> {
    if connect_daemon().is_ok() {
        return Ok(());
    }

    let paths = prepare_paths()?;
    if paths.daemon_socket.exists() {
        let _ = fs::remove_file(&paths.daemon_socket);
    }

    let exe = env::current_exe()?;
    let mut daemon = Command::new(exe);
    daemon
        .arg(DAEMON_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        daemon.pre_exec(|| {
            setsid().map_err(|err| io::Error::new(io::ErrorKind::Other, err.to_string()))?;
            Ok(())
        });
    }
    daemon.spawn()?;

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if connect_daemon().is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }

    Err(StayError::new("Failed to start stay daemon."))
}

fn connect_daemon() -> StayResult<UnixStream> {
    let paths = prepare_paths()?;
    Ok(UnixStream::connect(paths.daemon_socket)?)
}

fn attach(name: &str, command: Option<Vec<String>>, restart: bool) -> StayResult<()> {
    let mut stream = connect_daemon()?;
    let cwd = env::current_dir()?.display().to_string();
    let (rows, cols) = terminal_size();
    let request = Request::Attach {
        name: name.to_string(),
        cwd,
        command: command.clone(),
        restart,
        rows,
        cols,
    };
    write_json_line(&mut stream, &request)?;

    let response = read_response(&stream)?;
    match response {
        Response::AttachReady { message } => run_raw_client(stream, name, &message),
        Response::NeedsRestart {
            state,
            exit_code,
            command,
            ..
        } => {
            match state {
                SessionState::Stopped => println!("Session {name} is stopped."),
                SessionState::Exited => match exit_code {
                    Some(code) => println!("Session {name} has exited with code {code}."),
                    None => println!("Session {name} has exited."),
                },
                SessionState::Running => {}
            }
            println!();
            if !command.is_empty() {
                println!("Last command:");
                println!("{}", display_command(&command));
                println!();
            }
            println!("Press Enter to run again, or Ctrl+C to cancel.");
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            attach(name, command.into(), true)
        }
        Response::Error { message } => Err(StayError::new(message)),
        other => Err(StayError::new(format!("Unexpected response: {other:?}"))),
    }
}

fn run_raw_client(mut stream: UnixStream, name: &str, _message: &str) -> StayResult<()> {
    let stdin = io::stdin();
    let stdin_fd = stdin.as_fd();
    let original = tcgetattr(stdin_fd).map_err(|err| StayError::new(err.to_string()))?;
    let mut guard = TerminalGuard {
        fd_termios: Some((stdin.as_raw_fd(), original)),
        in_world: false,
        world_name: name.to_string(),
    };

    guard.in_world = true;
    enter_world(name)?;

    let mut raw = guard
        .fd_termios
        .as_ref()
        .expect("raw mode guard initialized")
        .1
        .clone();
    cfmakeraw(&mut raw);
    tcsetattr(stdin_fd, SetArg::TCSANOW, &raw).map_err(|err| StayError::new(err.to_string()))?;

    let (rows, cols) = terminal_size();
    let mut view = Screenview::new(rows, cols);
    let mut out = io::stdout();

    let mut input_buffer = [0_u8; 8192];
    let mut output_buffer = [0_u8; 8192];
    let mut pending_input: Vec<u8> = Vec::new();
    let mut detached = false;
    loop {
        let (stdin_ready, stream_ready) = {
            let mut fds = [
                PollFd::new(
                    stdin.as_fd(),
                    PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR,
                ),
                PollFd::new(
                    stream.as_fd(),
                    PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR,
                ),
            ];
            let timeout = if view.auto_scroll_direction().is_some() {
                PollTimeout::from(AUTO_SCROLL_INTERVAL_MS)
            } else {
                PollTimeout::NONE
            };
            let ready = poll(&mut fds, timeout).map_err(|err| StayError::new(err.to_string()))?;
            if ready == 0 {
                view.auto_scroll_selection(&mut out)?;
                out.flush()?;
                continue;
            }
            (
                fds[0].revents().unwrap_or(PollFlags::empty()),
                fds[1].revents().unwrap_or(PollFlags::empty()),
            )
        };

        if stream_ready.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            break;
        }
        if stream_ready.contains(PollFlags::POLLIN) {
            let read = stream.read(&mut output_buffer)?;
            if read == 0 {
                break;
            }
            view.feed(&output_buffer[..read]);
            // Coalesce a burst (e.g. the whole history replayed on attach) into a
            // single repaint instead of flickering through it chunk by chunk.
            while stream_has_input(&stream)? {
                let more = stream.read(&mut output_buffer)?;
                if more == 0 {
                    break;
                }
                view.feed(&output_buffer[..more]);
            }
            // While scrolled up, keep the frozen view; new output stays buffered
            // and shows when the user scrolls back down.
            if view.is_live() || view.selection_visible() {
                view.paint(&mut out)?;
                out.flush()?;
            }
        }

        if stdin_ready.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            break;
        }
        if !stdin_ready.contains(PollFlags::POLLIN) {
            continue;
        }

        let read = io::stdin().read(&mut input_buffer)?;
        if read == 0 {
            break;
        }
        if handle_client_input(
            &mut view,
            &mut stream,
            &mut out,
            &mut pending_input,
            &input_buffer[..read],
        )? {
            detached = true;
            let _ = stream.shutdown(Shutdown::Both);
            break;
        }
    }

    drop(guard);
    if detached {
        println!("Returned from {name}.");
        println!("Reattach with: stay {name}");
    }

    Ok(())
}

/// Client-side terminal emulator. Session output is fed into a `vt100` grid so
/// the alternate screen gains its own scrollback. Stay also draws a lightweight
/// selection overlay on top of that grid because terminal-native selection
/// cannot see this virtual scrollback.
struct Screenview {
    parser: vt100::Parser,
    prev: vt100::Screen,
    scroll: usize,
    selection: Option<Selection>,
    overlay_drawn: bool,
}

impl Screenview {
    fn new(rows: u16, cols: u16) -> Self {
        let parser = vt100::Parser::new(rows.max(1), cols.max(1), SCROLLBACK_LINES);
        let prev = parser.screen().clone();
        Self {
            parser,
            prev,
            scroll: 0,
            selection: None,
            overlay_drawn: false,
        }
    }

    fn is_live(&self) -> bool {
        self.scroll == 0
    }

    fn selection_visible(&self) -> bool {
        self.selection.is_some_and(|selection| selection.moved)
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Draw the current view (live or scrolled). With no selection, this is a
    /// diff against the previous grid; with a selection, it repaints the visible
    /// screen and overlays inverse-video selected spans.
    fn paint(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.parser.screen_mut().set_scrollback(self.scroll);
        self.scroll = self.parser.screen().scrollback();
        let screen = self.parser.screen();

        if self.selection_visible() || self.overlay_drawn {
            out.write_all(&screen.contents_formatted())?;
            if self.selection_visible() {
                paint_selection_overlay(screen, self.scroll, self.selection, out)?;
                self.overlay_drawn = true;
            } else {
                self.overlay_drawn = false;
            }
        } else {
            let diff = screen.contents_diff(&self.prev);
            out.write_all(&diff)?;
        }

        self.prev = screen.clone();
        Ok(())
    }

    /// Move the scrollback view; `up` heads toward older output.
    fn scroll_view(&mut self, up: bool, out: &mut impl Write) -> io::Result<()> {
        self.clear_selection();
        self.scroll_by_rows(
            if up {
                SCROLL_STEP as isize
            } else {
                -(SCROLL_STEP as isize)
            },
            out,
        )?;
        self.paint(out)
    }

    /// Snap back to the live bottom, the way typing in a normal terminal does.
    fn snap_to_live(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.clear_selection();
        if !self.is_live() {
            self.scroll = 0;
            out.write_all(SHOW_CURSOR.as_bytes())?;
            self.paint(out)?;
        } else if self.overlay_drawn {
            self.paint(out)?;
        }
        Ok(())
    }

    fn start_selection(&mut self, row: u16, col: u16) {
        let point = self.point_for_visible_cell(row, col);
        self.selection = Some(Selection {
            anchor: point,
            focus: point,
            dragging: true,
            moved: false,
            edge: self.edge_for_row(row),
            last_col: col,
        });
    }

    fn update_selection(&mut self, row: u16, col: u16) {
        let focus = self.point_for_visible_cell(row, col);
        let edge = self.edge_for_row(row);
        if let Some(selection) = self.selection.as_mut() {
            selection.focus = focus;
            selection.moved |= selection.focus != selection.anchor;
            selection.edge = edge;
            selection.last_col = col;
        }
    }

    fn finish_selection(&mut self) -> Option<String> {
        if let Some(selection) = self.selection.as_mut() {
            selection.dragging = false;
            selection.edge = None;
        }
        if !self.selection_visible() {
            self.selection = None;
            return None;
        }

        let text = self.selected_text();
        if text.is_empty() {
            self.selection = None;
            None
        } else {
            Some(text)
        }
    }

    fn clear_selection(&mut self) {
        self.selection = None;
    }

    fn auto_scroll_direction(&self) -> Option<EdgeScroll> {
        self.selection.and_then(|selection| {
            if selection.dragging {
                selection.edge
            } else {
                None
            }
        })
    }

    fn auto_scroll_selection(&mut self, out: &mut impl Write) -> io::Result<()> {
        let Some(direction) = self.auto_scroll_direction() else {
            return Ok(());
        };
        let delta = match direction {
            EdgeScroll::Up => 1,
            EdgeScroll::Down => -1,
        };
        if !self.scroll_by_rows(delta, out)? {
            return Ok(());
        }

        let (rows, _) = self.parser.screen().size();
        let row = match direction {
            EdgeScroll::Up => 0,
            EdgeScroll::Down => rows.saturating_sub(1),
        };
        let col = self.selection.map_or(0, |selection| selection.last_col);
        self.update_selection(row, col);
        self.paint(out)
    }

    fn scroll_by_rows(&mut self, delta: isize, out: &mut impl Write) -> io::Result<bool> {
        let was_live = self.is_live();
        let target = if delta >= 0 {
            self.scroll.saturating_add(delta as usize)
        } else {
            self.scroll.saturating_sub(delta.unsigned_abs())
        };

        self.parser.screen_mut().set_scrollback(target);
        let new_scroll = self.parser.screen().scrollback();
        let changed = new_scroll != self.scroll;
        self.scroll = new_scroll;

        if was_live && !self.is_live() {
            out.write_all(HIDE_CURSOR.as_bytes())?;
        } else if !was_live && self.is_live() {
            out.write_all(SHOW_CURSOR.as_bytes())?;
        }

        Ok(changed)
    }

    fn point_for_visible_cell(&self, row: u16, col: u16) -> SelectionPoint {
        let (rows, cols) = self.parser.screen().size();
        let row = row.min(rows.saturating_sub(1));
        let col = col.min(cols.saturating_sub(1));
        SelectionPoint {
            row: row as isize - self.scroll as isize,
            col,
        }
    }

    fn edge_for_row(&self, row: u16) -> Option<EdgeScroll> {
        let (rows, _) = self.parser.screen().size();
        if row == 0 {
            Some(EdgeScroll::Up)
        } else if row >= rows.saturating_sub(1) {
            Some(EdgeScroll::Down)
        } else {
            None
        }
    }

    fn selected_text(&mut self) -> String {
        let Some(selection) = self.selection else {
            return String::new();
        };
        let Some((start, end)) = selection.ordered_bounds() else {
            return String::new();
        };

        let saved_scroll = self.scroll;
        let (_, cols) = self.parser.screen().size();
        let mut text = String::new();

        for row in start.row..=end.row {
            let Some(visible_row) = self.make_history_row_visible(row) else {
                continue;
            };

            let start_col = if row == start.row { start.col } else { 0 };
            let end_col = if row == end.row {
                end.col.saturating_add(1).min(cols)
            } else {
                cols
            };

            if start_col < end_col {
                text.push_str(&self.visible_row_text(visible_row, start_col, end_col));
            }
            if row != end.row && !self.parser.screen().row_wrapped(visible_row) {
                text.push('\n');
            }
        }

        self.parser.screen_mut().set_scrollback(saved_scroll);
        self.scroll = self.parser.screen().scrollback();
        text
    }

    fn make_history_row_visible(&mut self, row: isize) -> Option<u16> {
        let requested_scroll = if row < 0 { row.unsigned_abs() } else { 0 };
        self.parser.screen_mut().set_scrollback(requested_scroll);
        let actual_scroll = self.parser.screen().scrollback();
        let visible_row = row + actual_scroll as isize;
        let (rows, _) = self.parser.screen().size();
        if (0..rows as isize).contains(&visible_row) {
            Some(visible_row as u16)
        } else {
            None
        }
    }

    fn visible_row_text(&self, row: u16, start_col: u16, end_col: u16) -> String {
        self.parser
            .screen()
            .rows(start_col, end_col.saturating_sub(start_col))
            .nth(usize::from(row))
            .unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SelectionPoint {
    row: isize,
    col: u16,
}

#[derive(Clone, Copy, Debug)]
struct Selection {
    anchor: SelectionPoint,
    focus: SelectionPoint,
    dragging: bool,
    moved: bool,
    edge: Option<EdgeScroll>,
    last_col: u16,
}

impl Selection {
    fn ordered_bounds(self) -> Option<(SelectionPoint, SelectionPoint)> {
        if !self.moved {
            return None;
        }
        if self.anchor <= self.focus {
            Some((self.anchor, self.focus))
        } else {
            Some((self.focus, self.anchor))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EdgeScroll {
    Up,
    Down,
}

fn paint_selection_overlay(
    screen: &vt100::Screen,
    scroll: usize,
    selection: Option<Selection>,
    out: &mut impl Write,
) -> io::Result<()> {
    let Some(selection) = selection else {
        return Ok(());
    };
    let Some((start, end)) = selection.ordered_bounds() else {
        return Ok(());
    };

    let (rows, cols) = screen.size();
    for row in 0..rows {
        let history_row = row as isize - scroll as isize;
        let Some((start_col, end_col)) =
            selected_span_for_visible_row(history_row, cols, start, end)
        else {
            continue;
        };
        let text = selected_cells_text(screen, row, start_col, end_col);
        if text.is_empty() {
            continue;
        }
        write!(
            out,
            "\x1b[{};{}H{}{}{}",
            row + 1,
            start_col + 1,
            INVERT_ON,
            text,
            INVERT_OFF
        )?;
    }
    write!(out, "{RESET}")?;
    out.write_all(&screen.cursor_state_formatted())?;
    Ok(())
}

fn selected_span_for_visible_row(
    row: isize,
    cols: u16,
    start: SelectionPoint,
    end: SelectionPoint,
) -> Option<(u16, u16)> {
    if row < start.row || row > end.row || cols == 0 {
        return None;
    }

    let start_col = if row == start.row { start.col } else { 0 };
    let end_col = if row == end.row {
        end.col.saturating_add(1).min(cols)
    } else {
        cols
    };

    (start_col < end_col).then_some((start_col.min(cols), end_col))
}

fn selected_cells_text(screen: &vt100::Screen, row: u16, start_col: u16, end_col: u16) -> String {
    let mut text = String::new();
    for col in start_col..end_col {
        let Some(cell) = screen.cell(row, col) else {
            text.push(' ');
            continue;
        };
        if cell.is_wide_continuation() {
            continue;
        }
        if cell.has_contents() {
            text.push_str(cell.contents());
        } else {
            text.push(' ');
        }
    }
    text
}

/// Feed raw stdin: intercept SGR mouse reports for scrollback and selection,
/// treat Ctrl+A (0x01) as detach, and forward everything else to the PTY.
/// `pending` carries an unfinished mouse sequence split across reads.
fn handle_client_input(
    view: &mut Screenview,
    stream: &mut UnixStream,
    out: &mut impl Write,
    pending: &mut Vec<u8>,
    chunk: &[u8],
) -> StayResult<bool> {
    let mut data = std::mem::take(pending);
    data.extend_from_slice(chunk);

    let mut forward: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let byte = data[i];

        if byte == 0x1b && data.get(i + 1) == Some(&b'[') && data.get(i + 2) == Some(&b'<') {
            if let Some(rel) = data[i + 3..].iter().position(|&c| c == b'M' || c == b'm') {
                let term = i + 3 + rel;
                if let Some(report) = parse_mouse_report(&data[i + 3..term], data[term]) {
                    flush_forward(view, stream, out, &mut forward)?;
                    handle_mouse_report(view, out, report)?;
                }
                i = term + 1;
                continue;
            }
            *pending = data[i..].to_vec();
            break;
        }

        if byte == 0x01 {
            flush_forward(view, stream, out, &mut forward)?;
            out.flush()?;
            return Ok(true);
        }

        forward.push(byte);
        i += 1;
    }

    flush_forward(view, stream, out, &mut forward)?;
    out.flush()?;
    Ok(false)
}

fn flush_forward(
    view: &mut Screenview,
    stream: &mut UnixStream,
    out: &mut impl Write,
    forward: &mut Vec<u8>,
) -> StayResult<()> {
    if forward.is_empty() {
        return Ok(());
    }
    view.snap_to_live(out)?;
    stream.write_all(forward)?;
    forward.clear();
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MouseReport {
    code: u16,
    col: u16,
    row: u16,
    pressed: bool,
}

fn parse_mouse_report(payload: &[u8], terminator: u8) -> Option<MouseReport> {
    let mut parts = std::str::from_utf8(payload).ok()?.split(';');
    let code = parts.next()?.parse().ok()?;
    let col = parts.next()?.parse::<u16>().ok()?.saturating_sub(1);
    let row = parts.next()?.parse::<u16>().ok()?.saturating_sub(1);
    Some(MouseReport {
        code,
        col,
        row,
        pressed: terminator == b'M',
    })
}

fn handle_mouse_report(
    view: &mut Screenview,
    out: &mut impl Write,
    report: MouseReport,
) -> StayResult<()> {
    if let Some(up) = mouse_wheel_direction(report.code) {
        view.scroll_view(up, out)?;
        return Ok(());
    }

    if !report.pressed {
        if let Some(text) = view.finish_selection() {
            write_osc52_clipboard(out, &text)?;
        }
        view.paint(out)?;
        return Ok(());
    }

    let left_button = report.code & 0b11 == 0;
    let motion = report.code & 0b0010_0000 != 0;

    if left_button && motion {
        view.update_selection(report.row, report.col);
        view.paint(out)?;
    } else if left_button {
        view.start_selection(report.row, report.col);
        view.paint(out)?;
    }

    Ok(())
}

fn mouse_wheel_direction(code: u16) -> Option<bool> {
    if code & 0b0100_0000 == 0 {
        return None;
    }
    match code & 0b0000_0011 {
        0 => Some(true),
        1 => Some(false),
        _ => None,
    }
}

fn write_osc52_clipboard(out: &mut impl Write, text: &str) -> io::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    write!(out, "\x1b]52;c;{}\x07", base64_encode(text.as_bytes()))
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);

        encoded.push(TABLE[(a >> 2) as usize] as char);
        encoded.push(TABLE[(((a & 0b0000_0011) << 4) | (b >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(TABLE[(((b & 0b0000_1111) << 2) | (c >> 6)) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(TABLE[(c & 0b0011_1111) as usize] as char);
        } else {
            encoded.push('=');
        }
    }

    encoded
}

/// Non-blocking check for more session output, used to drain a burst before
/// repainting.
fn stream_has_input(stream: &UnixStream) -> StayResult<bool> {
    let mut fds = [PollFd::new(stream.as_fd(), PollFlags::POLLIN)];
    poll(&mut fds, PollTimeout::ZERO).map_err(|err| StayError::new(err.to_string()))?;
    Ok(fds[0]
        .revents()
        .unwrap_or(PollFlags::empty())
        .contains(PollFlags::POLLIN))
}

struct TerminalGuard {
    fd_termios: Option<(i32, Termios)>,
    in_world: bool,
    world_name: String,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Some((fd, termios)) = self.fd_termios.take() {
            let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
            let _ = tcsetattr(borrowed, SetArg::TCSANOW, &termios);
        }
        if self.in_world {
            // Turn off mouse reporting, play the exit animation on the same
            // alternate screen we have been on all session, then leave it so the
            // user's original terminal view returns.
            print!("{MOUSE_OFF}{SHOW_CURSOR}");
            let _ = play_pixel_transition(&self.world_name, false);
            print!("{ALT_SCREEN_EXIT}{SHOW_CURSOR}");
            let _ = io::stdout().flush();
        }
    }
}

fn enter_world(name: &str) -> StayResult<()> {
    // Keep the whole session on a dedicated alternate screen (the portal), then
    // render it through the client-side scrollback view. Mouse reporting lets
    // Stay make wheel scrolling and drag selection work inside that view.
    print!("{ALT_SCREEN_ENTER}");
    play_pixel_transition(name, true)?;
    print!("{CLEAR_SCREEN}{MOUSE_ON}");
    io::stdout().flush()?;
    Ok(())
}

fn play_pixel_transition(name: &str, entering: bool) -> StayResult<()> {
    let label = if entering {
        format!("inside: {name}")
    } else {
        "outside".to_string()
    };
    let frames = if entering {
        [0_usize, 1, 2, 3, 4, 5, 6, 7, 8]
    } else {
        [8_usize, 7, 6, 5, 4, 3, 2, 1, 0]
    };

    for frame in frames {
        draw_pixel_frame(&label, frame, 8)?;
        thread::sleep(Duration::from_millis(FRAME_DELAY_MS));
    }

    print!("{SHOW_CURSOR}");
    io::stdout().flush()?;
    Ok(())
}

fn draw_pixel_frame(label: &str, frame: usize, max_frame: usize) -> StayResult<()> {
    let (rows, cols) = terminal_size();
    let available_width = (cols as usize).saturating_sub(4).max(1);
    let available_height = (rows as usize).saturating_sub(6).max(1);
    let max_width = available_width
        .min(72)
        .max(label.len().min(available_width));
    let max_height = available_height.min(11).max(1);
    let width = 1 + max_width.saturating_sub(1) * frame / max_frame.max(1);
    let height = 1 + max_height.saturating_sub(1) * frame / max_frame.max(1);
    let row = ((rows as usize).saturating_sub(height) / 2).max(1);
    let col = ((cols as usize).saturating_sub(width) / 2).max(1);
    let label_col = ((cols as usize).saturating_sub(label.len()) / 2).max(1);

    print!("{HIDE_CURSOR}{CLEAR_SCREEN}");
    print!(
        "\x1b[{};{}H{DIM}{label}{RESET}",
        row.saturating_sub(2).max(1),
        label_col
    );

    for offset in 0..height {
        let edge = offset == 0 || offset + 1 == height;
        let line = if edge {
            "#".repeat(width)
        } else if width > 2 {
            format!("#{}#", " ".repeat(width - 2))
        } else {
            "#".repeat(width)
        };
        print!("\x1b[{};{}H{ACCENT}{BOLD}{line}{RESET}", row + offset, col);
    }
    io::stdout().flush()?;
    Ok(())
}

fn list_sessions() -> StayResult<()> {
    let mut stream = connect_daemon()?;
    write_json_line(&mut stream, &Request::List)?;
    match read_response(&stream)? {
        Response::Sessions { sessions } => {
            println!("{:<16}{:<10}COMMAND", "NAME", "STATE");
            for session in sessions {
                println!(
                    "{:<16}{:<10}{}",
                    session.name,
                    session.state.to_string(),
                    display_command(&session.command)
                );
            }
            Ok(())
        }
        Response::Error { message } => Err(StayError::new(message)),
        other => Err(StayError::new(format!("Unexpected response: {other:?}"))),
    }
}

fn simple_request(request: Request) -> StayResult<()> {
    let mut stream = connect_daemon()?;
    write_json_line(&mut stream, &request)?;
    match read_response(&stream)? {
        Response::Ok { message } => {
            println!("{message}");
            Ok(())
        }
        Response::Error { message } => Err(StayError::new(message)),
        other => Err(StayError::new(format!("Unexpected response: {other:?}"))),
    }
}

fn read_response(stream: &UnixStream) -> StayResult<Response> {
    let mut line = String::new();
    let mut stream = stream.try_clone()?;
    read_line_unbuffered(&mut stream, &mut line)?;
    Ok(serde_json::from_str(&line)?)
}

fn write_response(stream: &mut UnixStream, response: &Response) -> StayResult<()> {
    write_json_line(stream, response)
}

fn write_json_line<T: serde::Serialize>(stream: &mut UnixStream, value: &T) -> StayResult<()> {
    serde_json::to_writer(&mut *stream, value)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn read_line_unbuffered(stream: &mut UnixStream, line: &mut String) -> StayResult<()> {
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let read = stream.read(&mut byte)?;
        if read == 0 {
            break;
        }
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    *line = String::from_utf8(bytes).map_err(|err| StayError::new(err.to_string()))?;
    Ok(())
}

fn prepare_paths() -> StayResult<Paths> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| StayError::new("HOME is not set."))?;
    let state_dir = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"))
        .join("stay");
    let sessions_dir = state_dir.join("sessions");
    let sockets_dir = state_dir.join("sockets");
    fs::create_dir_all(&sessions_dir)?;
    fs::create_dir_all(&sockets_dir)?;
    fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700))?;

    Ok(Paths {
        daemon_socket: sockets_dir.join("daemon.sock"),
        state_dir,
        sessions_dir,
    })
}

fn load_records(paths: &Paths) -> StayResult<HashMap<String, ManagedSession>> {
    let mut sessions = HashMap::new();
    if !paths.sessions_dir.exists() {
        return Ok(sessions);
    }

    for entry in fs::read_dir(&paths.sessions_dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }

        let mut record = serde_json::from_slice::<SessionRecord>(&fs::read(entry.path())?)?;
        if record.state == SessionState::Running {
            record.state = SessionState::Stopped;
            record.pid = None;
            write_record(paths, &record)?;
        }
        sessions.insert(
            record.name.clone(),
            ManagedSession {
                record,
                master: None,
                io: SessionIo::shared(),
                attached: false,
            },
        );
    }

    Ok(sessions)
}

fn read_record(paths: &Paths, name: &str) -> StayResult<Option<SessionRecord>> {
    let path = session_path(paths, name);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
}

fn write_record(paths: &Paths, record: &SessionRecord) -> StayResult<()> {
    let path = session_path(paths, &record.name);
    let data = serde_json::to_vec_pretty(record)?;
    fs::write(path, data)?;
    Ok(())
}

fn session_path(paths: &Paths, name: &str) -> PathBuf {
    paths.sessions_dir.join(format!("{name}.json"))
}

/// Resize the session's PTY to the attaching terminal. The kernel delivers
/// SIGWINCH to the running program, so it redraws for the size the user is
/// actually looking at instead of the size the session was first opened at.
fn set_pty_size(master: &File, rows: u16, cols: u16) {
    let winsize = libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &winsize);
    }
}

fn print_completion_usage() {
    println!("Usage: stay completions <bash|zsh|fish>");
}

fn print_completions(shell: &str) -> StayResult<()> {
    match shell {
        "bash" => {
            print!("{}", bash_completions());
            Ok(())
        }
        "zsh" => {
            print!("{}", zsh_completions());
            Ok(())
        }
        "fish" => {
            print!("{}", fish_completions());
            Ok(())
        }
        _ => Err(StayError::new(format!(
            "Unsupported shell: {shell}\n\nUse one of: bash, zsh, fish."
        ))),
    }
}

fn bash_completions() -> &'static str {
    r#"_stay_sessions() {
  local dir="${XDG_STATE_HOME:-$HOME/.local/state}/stay/sessions"
  local path name
  [ -d "$dir" ] || return 0
  for path in "$dir"/*.json; do
    [ -e "$path" ] || continue
    name="${path##*/}"
    printf '%s\n' "${name%.json}"
  done
}

_stay_complete() {
  local cur prev commands shells
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]}"
  commands="ls kill rm completions completion --version -V"
  shells="bash zsh fish"

  if [ "$COMP_CWORD" -eq 1 ]; then
    COMPREPLY=( $(compgen -W "$commands $(_stay_sessions)" -- "$cur") )
    return 0
  fi

  case "$prev" in
    kill|rm)
      COMPREPLY=( $(compgen -W "$(_stay_sessions)" -- "$cur") )
      ;;
    completions|completion)
      COMPREPLY=( $(compgen -W "$shells" -- "$cur") )
      ;;
    *)
      COMPREPLY=()
      ;;
  esac
}

complete -F _stay_complete stay
"#
}

fn zsh_completions() -> &'static str {
    r#"#compdef stay

_stay_sessions() {
  local dir="${XDG_STATE_HOME:-$HOME/.local/state}/stay/sessions"
  local -a sessions
  local path name
  [[ -d "$dir" ]] || return 1
  for path in "$dir"/*.json(N); do
    name="${path:t:r}"
    sessions+=("$name")
  done
  compadd -- "$sessions[@]"
}

_stay() {
  local -a commands shells
  commands=(
    'ls:list sessions'
    'kill:kill a running session'
    'rm:remove a stopped session'
    'completions:print shell completions'
    'completion:print shell completions'
  )
  shells=(bash zsh fish)

  case $CURRENT in
    2)
      _alternative \
        'commands:command:->commands' \
        'sessions:session:->sessions'
      case $state in
        commands) _describe 'command' commands ;;
        sessions) _stay_sessions ;;
      esac
      ;;
    3)
      case $words[2] in
        kill|rm) _stay_sessions ;;
        completions|completion) compadd -- "$shells[@]" ;;
      esac
      ;;
  esac
}

_stay "$@"
"#
}

fn fish_completions() -> &'static str {
    r#"function __stay_sessions
    set -l dir "$HOME/.local/state/stay/sessions"
    if set -q XDG_STATE_HOME
        set dir "$XDG_STATE_HOME/stay/sessions"
    end
    test -d "$dir"; or return
    for path in "$dir"/*.json
        test -e "$path"; and basename "$path" .json
    end
end

complete -c stay -f
complete -c stay -n "__fish_use_subcommand" -a "ls" -d "List sessions"
complete -c stay -n "__fish_use_subcommand" -a "kill" -d "Kill a running session"
complete -c stay -n "__fish_use_subcommand" -a "rm" -d "Remove a stopped session"
complete -c stay -n "__fish_use_subcommand" -a "completions" -d "Print shell completions"
complete -c stay -n "__fish_use_subcommand" -a "(__stay_sessions)"
complete -c stay -n "__fish_seen_subcommand_from kill rm" -a "(__stay_sessions)"
complete -c stay -n "__fish_seen_subcommand_from completions completion" -a "bash zsh fish"
"#
}

fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn terminal_size() -> (u16, u16) {
    let mut winsize = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut winsize) } == 0;
    if ok && winsize.ws_row > 0 && winsize.ws_col > 0 {
        (winsize.ws_row, winsize.ws_col)
    } else {
        (24, 80)
    }
}

fn write_all_fd<Fd: std::os::fd::AsFd>(fd: Fd, mut bytes: &[u8]) -> StayResult<()> {
    while !bytes.is_empty() {
        let written =
            nix_write(fd.as_fd(), bytes).map_err(|err| StayError::new(err.to_string()))?;
        if written == 0 {
            return Err(StayError::new("write returned 0 bytes"));
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

#[allow(dead_code)]
fn _paths_are_private(paths: &Paths) -> bool {
    paths.state_dir.starts_with(Path::new("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_accumulates_output_for_replay() {
        let mut io = SessionIo::new();
        io.record(b"log line 1\n");
        io.record(b"log line 2\n");
        assert_eq!(io.buffer, b"log line 1\nlog line 2\n");
    }

    #[test]
    fn record_keeps_history_after_clear_scrollback() {
        let mut io = SessionIo::new();
        io.record(b"old output\n");
        io.record(strip_clear_scrollback(b"\x1b[3Jfresh output\n").as_ref());
        assert_eq!(io.buffer, b"old output\nfresh output\n");
    }

    #[test]
    fn strip_clear_scrollback_removes_all_sequences_in_chunk() {
        assert_eq!(
            strip_clear_scrollback(b"a\x1b[3Jb\x1b[3Jc").as_ref(),
            b"abc"
        );
    }

    #[test]
    fn record_trims_to_limit_on_a_line_boundary() {
        let mut io = SessionIo::new();
        let mut blob = Vec::new();
        while blob.len() <= SCROLLBACK_LIMIT {
            blob.extend_from_slice(b"a line of text\n");
        }
        io.record(&blob);
        assert!(io.buffer.len() <= SCROLLBACK_LIMIT);
        // Trimming keeps the most recent output and cuts on a line boundary,
        // so the buffer still begins and ends with a whole line.
        assert!(io.buffer.starts_with(b"a line of text\n"));
        assert!(io.buffer.ends_with(b"a line of text\n"));
    }

    #[test]
    fn strip_clear_scrollback_leaves_other_output() {
        assert_eq!(
            strip_clear_scrollback(b"no sequence here").as_ref(),
            b"no sequence here"
        );
        assert_eq!(strip_clear_scrollback(b"ab\x1b[3Jcd").as_ref(), b"abcd");
    }

    #[test]
    fn parses_sgr_mouse_reports() {
        assert_eq!(
            parse_mouse_report(b"32;10;3", b'M'),
            Some(MouseReport {
                code: 32,
                col: 9,
                row: 2,
                pressed: true
            })
        );
        assert_eq!(
            parse_mouse_report(b"0;1;1", b'm'),
            Some(MouseReport {
                code: 0,
                col: 0,
                row: 0,
                pressed: false
            })
        );
    }

    #[test]
    fn selection_spans_visible_rows() {
        let start = SelectionPoint { row: -2, col: 3 };
        let end = SelectionPoint { row: 0, col: 4 };
        assert_eq!(
            selected_span_for_visible_row(-2, 10, start, end),
            Some((3, 10))
        );
        assert_eq!(
            selected_span_for_visible_row(-1, 10, start, end),
            Some((0, 10))
        );
        assert_eq!(
            selected_span_for_visible_row(0, 10, start, end),
            Some((0, 5))
        );
        assert_eq!(selected_span_for_visible_row(1, 10, start, end), None);
    }

    #[test]
    fn base64_encodes_clipboard_payloads() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }
}
