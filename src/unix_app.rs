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

#[derive(Clone)]
struct Paths {
    state_dir: PathBuf,
    sessions_dir: PathBuf,
    sockets_dir: PathBuf,
    daemon_socket: PathBuf,
}

struct ManagedSession {
    record: SessionRecord,
    master: Option<File>,
    attached: bool,
}

type Sessions = Arc<Mutex<HashMap<String, ManagedSession>>>;

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
        [flag] if flag == "--version" || flag == "-V" => {
            println!("stay {VERSION}");
            Ok(())
        }
        [cmd] if cmd == "ls" => {
            ensure_daemon()?;
            list_sessions()
        }
        [cmd, name] if cmd == "kill" => {
            validate_session_name(name).map_err(StayError::new)?;
            ensure_daemon()?;
            simple_request(Request::Kill { name: name.clone() })
        }
        [cmd, name] if cmd == "rm" => {
            validate_session_name(name).map_err(StayError::new)?;
            ensure_daemon()?;
            simple_request(Request::Remove { name: name.clone() })
        }
        _ => attach_command(args),
    }
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
        } => handle_attach(stream, sessions, paths, name, cwd, command, restart, rows, cols),
        Request::Kill { name } => {
            let message = kill_session(&name, &sessions, &paths)?;
            write_response(&mut stream, &Response::Ok { message })
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
            let message = remove_session(&name, &sessions, &paths)?;
            write_response(&mut stream, &Response::Ok { message })
        }
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
    let explicit_command = command.is_some();

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

                    if command.is_some()
                        && command.as_ref().is_some_and(|cmd| cmd != &existing.record.command)
                    {
                        message.push_str(&format!(
                            "Session {name} already exists.\nAttaching to existing session.\n"
                        ));
                    }
                    message.push_str(&format!("Attached to {name}.\nDetach: Ctrl+A\n"));
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
                    let (record, master) =
                        spawn_session(&name, &start_cwd, &start_command, rows, cols, &paths)?;
                    existing.record = record;
                    existing.master = Some(master);
                    existing.attached = false;
                    message.push_str(&new_session_message(&name, &start_command, explicit_command));
                }
            }
        } else {
            let (record, master) =
                spawn_session(&name, &start_cwd, &start_command, rows, cols, &paths)?;
            sessions_guard.insert(
                name.clone(),
                ManagedSession {
                    record,
                    master: Some(master),
                    attached: false,
                },
            );
            message.push_str(&new_session_message(&name, &start_command, explicit_command));
        }
    }

    write_response(&mut stream, &Response::AttachReady { message })?;
    attach_stream(name, stream, sessions)
}

fn attach_stream(name: String, mut stream: UnixStream, sessions: Sessions) -> StayResult<()> {
    let master = {
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
        master
    };

    let result = pump_terminal(&mut stream, &master);

    let mut sessions_guard = sessions.lock().expect("sessions lock poisoned");
    if let Some(session) = sessions_guard.get_mut(&name) {
        session.attached = false;
    }

    result
}

fn pump_terminal(stream: &mut UnixStream, master: &File) -> StayResult<()> {
    let mut from_client = [0_u8; 8192];
    let mut from_pty = [0_u8; 8192];

    loop {
        let (stream_ready, pty_ready) = {
            let mut fds = [
                PollFd::new(
                    stream.as_fd(),
                    PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR,
                ),
                PollFd::new(
                    master.as_fd(),
                    PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR,
                ),
            ];
            poll(&mut fds, PollTimeout::NONE)
                .map_err(|err| StayError::new(err.to_string()))?;
            (
                fds[0].revents().unwrap_or(PollFlags::empty()),
                fds[1].revents().unwrap_or(PollFlags::empty()),
            )
        };

        if stream_ready.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            break;
        }
        if pty_ready.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            break;
        }
        if stream_ready.contains(PollFlags::POLLIN) {
            let read = stream.read(&mut from_client)?;
            if read == 0 {
                break;
            }
            write_all_fd(master, &from_client[..read])?;
        }
        if pty_ready.contains(PollFlags::POLLIN) {
            let read = nix_read(master, &mut from_pty)
                .map_err(|err| StayError::new(err.to_string()))?;
            if read == 0 {
                break;
            }
            stream.write_all(&from_pty[..read])?;
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
) -> StayResult<(SessionRecord, File)> {
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

    Ok((record, master))
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
        Response::AttachReady { message } => {
            print!("{message}");
            io::stdout().flush()?;
            run_raw_client(stream, name)
        }
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

fn run_raw_client(mut stream: UnixStream, name: &str) -> StayResult<()> {
    let stdin = io::stdin();
    let stdin_fd = stdin.as_fd();
    let original = tcgetattr(stdin_fd).map_err(|err| StayError::new(err.to_string()))?;
    let guard = RawModeGuard {
        fd_termios: Some((stdin.as_raw_fd(), original)),
    };

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
            poll(&mut fds, PollTimeout::NONE)
                .map_err(|err| StayError::new(err.to_string()))?;
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
        println!("\nDetached from {name}.");
        println!("Reattach with: stay {name}");
    }

    Ok(())
}

struct RawModeGuard {
    fd_termios: Option<(i32, Termios)>,
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Some((fd, termios)) = self.fd_termios.take() {
            let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
            let _ = tcsetattr(borrowed, SetArg::TCSANOW, &termios);
        }
    }
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
                    session.state,
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
        sockets_dir,
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

fn new_session_message(name: &str, command: &[String], explicit_command: bool) -> String {
    let mut message = format!("Stay session: {name}\n");
    if explicit_command {
        message.push_str(&format!("Command: {}\n", display_command(command)));
    }
    message.push_str("Detach: Ctrl+A\n");
    message.push_str(&format!("Reattach: stay {name}\n"));
    message
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
        let written = nix_write(fd.as_fd(), bytes).map_err(|err| StayError::new(err.to_string()))?;
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
