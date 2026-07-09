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
const BOLD: &str = "\x1b[1m";
const ACCENT: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Delay between transition animation frames. Lower is faster.
const FRAME_DELAY_MS: u64 = 36;
/// Maximum bytes of per-session output kept for replay on re-attach.
const SCROLLBACK_LIMIT: usize = 2 * 1024 * 1024;

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

    /// Append a chunk of PTY output to the replay buffer.
    ///
    /// An explicit "clear scrollback" (ESC [ 3 J), which `clear` emits on
    /// terminals that support it, drops the earlier history so a cleared screen
    /// stays cleared on the next attach. The buffer is otherwise capped to the
    /// most recent `SCROLLBACK_LIMIT` bytes, trimmed at a line boundary.
    fn record(&mut self, chunk: &[u8]) {
        if let Some(pos) = last_clear_scrollback(chunk) {
            self.buffer.clear();
            self.buffer.extend_from_slice(&chunk[pos..]);
        } else {
            self.buffer.extend_from_slice(chunk);
        }

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

/// Index of the ESC starting the last `ESC [ 3 J` sequence in `chunk`, if any.
fn last_clear_scrollback(chunk: &[u8]) -> Option<usize> {
    const SEQ: &[u8] = b"\x1b[3J";
    if chunk.len() < SEQ.len() {
        return None;
    }
    (0..=chunk.len() - SEQ.len())
        .rev()
        .find(|&index| &chunk[index..index + SEQ.len()] == SEQ)
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

                    message.push_str(&space_message(&name));
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
                    message.push_str(&new_session_message(&name));
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
            message.push_str(&new_session_message(&name));
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
                let chunk = &buffer[..read];
                let mut io_guard = io.lock().expect("session io lock poisoned");
                io_guard.record(chunk);
                if let Some(subscriber) = io_guard.subscriber.as_mut() {
                    if subscriber.write_all(chunk).is_err() {
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

fn run_raw_client(mut stream: UnixStream, name: &str, message: &str) -> StayResult<()> {
    let stdin = io::stdin();
    let stdin_fd = stdin.as_fd();
    let original = tcgetattr(stdin_fd).map_err(|err| StayError::new(err.to_string()))?;
    let mut guard = TerminalGuard {
        fd_termios: Some((stdin.as_raw_fd(), original)),
        in_world: false,
        world_name: name.to_string(),
    };

    guard.in_world = true;
    enter_world(name, message)?;

    let mut raw = guard
        .fd_termios
        .as_ref()
        .expect("raw mode guard initialized")
        .1
        .clone();
    cfmakeraw(&mut raw);
    tcsetattr(stdin_fd, SetArg::TCSANOW, &raw).map_err(|err| StayError::new(err.to_string()))?;

    let mut input_buffer = [0_u8; 8192];
    let mut output_buffer = [0_u8; 8192];
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
            poll(&mut fds, PollTimeout::NONE).map_err(|err| StayError::new(err.to_string()))?;
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
            io::stdout().write_all(&output_buffer[..read])?;
            io::stdout().flush()?;
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

        if let Some(position) = input_buffer[..read].iter().position(|byte| *byte == 0x01) {
            if position > 0 {
                stream.write_all(&input_buffer[..position])?;
            }
            detached = true;
            let _ = stream.shutdown(Shutdown::Both);
            break;
        }

        stream.write_all(&input_buffer[..read])?;
    }

    drop(guard);
    if detached {
        println!("Returned from {name}.");
        println!("Reattach with: stay {name}");
    }

    Ok(())
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
            let _ = play_pixel_transition(&self.world_name, false);
            print!("{ALT_SCREEN_EXIT}{SHOW_CURSOR}");
            let _ = io::stdout().flush();
        }
    }
}

fn enter_world(name: &str, message: &str) -> StayResult<()> {
    print!("{ALT_SCREEN_ENTER}");
    play_pixel_transition(name, true)?;
    print!("{CLEAR_SCREEN}");
    println!("{DIM}stay:{name}  |  inside  |  Ctrl+A to return{RESET}");
    if !message.is_empty() {
        print!("{message}");
    } else {
        println!();
    }
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

fn new_session_message(name: &str) -> String {
    space_message(name)
}

fn space_message(name: &str) -> String {
    format!("{DIM}arrived in {name}{RESET}\n\n")
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
    fn record_drops_history_before_clear_scrollback() {
        let mut io = SessionIo::new();
        io.record(b"old output\n");
        io.record(b"\x1b[3Jfresh output\n");
        assert_eq!(io.buffer, b"\x1b[3Jfresh output\n");
    }

    #[test]
    fn record_keeps_last_clear_within_a_chunk() {
        let mut io = SessionIo::new();
        io.record(b"a\x1b[3Jb\x1b[3Jc");
        assert_eq!(io.buffer, b"\x1b[3Jc");
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
    fn last_clear_scrollback_finds_sequences() {
        assert_eq!(last_clear_scrollback(b"no sequence here"), None);
        assert_eq!(last_clear_scrollback(b"\x1b[3J"), Some(0));
        assert_eq!(last_clear_scrollback(b"ab\x1b[3Jcd"), Some(2));
        assert_eq!(last_clear_scrollback(b"\x1b[3Jx\x1b[3J"), Some(5));
    }
}
