use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    thread,
};

use crossbeam_channel::{unbounded, Receiver, Sender};
use multerm_core::{pty::spawn_pty, PtyHandle};

const FRAME_ATTACH: u8 = 1;
const FRAME_ATTACH_ERROR: u8 = 2;
const FRAME_OUTPUT: u8 = 3;
const FRAME_INPUT: u8 = 4;
const FRAME_RESIZE: u8 = 5;

// AttachResult carries the history snapshot and the attach generation (used to
// ignore stale Detach commands from the previous connection on fast restarts).
type AttachResult = Result<(Vec<Vec<u8>>, u64), String>;

enum DaemonCmd {
    Attach {
        session_key: String,
        rows: u16,
        cols: u16,
        working_dir: Option<String>,
        client_out_tx: Sender<Vec<u8>>,
        resp_tx: Sender<AttachResult>,
    },
    Detach {
        session_key: String,
        /// The generation this Detach belongs to. Stale Detaches (gen < current)
        /// are silently ignored so a quick restart can't cancel a fresh attach.
        gen: u64,
    },
    Input {
        session_key: String,
        data: Vec<u8>,
    },
    Resize {
        session_key: String,
        rows: u16,
        cols: u16,
    },
}

struct Session {
    pty: PtyHandle,
    output_rx: Receiver<Vec<u8>>,
    history: VecDeque<Vec<u8>>,
    history_bytes: usize,
    attached_client: Option<Sender<Vec<u8>>>,
    /// Incremented on every attach so stale Detach commands can be ignored.
    attach_gen: u64,
}

fn read_frame(stream: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    if let Err(e) = stream.read_exact(&mut header) {
        return Err(e);
    }
    let frame_type = header[0];
    let len = u32::from_le_bytes(header[1..5].try_into().expect("len")) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok((frame_type, payload))
}

fn write_frame(stream: &mut TcpStream, frame_type: u8, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len() as u32;
    let mut header = [0u8; 5];
    header[0] = frame_type;
    header[1..5].copy_from_slice(&len.to_le_bytes());
    stream.write_all(&header)?;
    if !payload.is_empty() {
        stream.write_all(payload)?;
    }
    Ok(())
}

fn daemon_base_dir() -> anyhow::Result<PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var("LOCALAPPDATA")
            .or_else(|_| std::env::var("APPDATA"))
            .unwrap_or_else(|_| ".".into());
        Ok(PathBuf::from(base).join("multerm"))
    }
    #[cfg(not(windows))]
    {
        let base = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        Ok(PathBuf::from(base).join(".multerm"))
    }
}

pub fn daemon_port_file_path() -> anyhow::Result<PathBuf> {
    Ok(daemon_base_dir()?.join("daemon_port.txt"))
}

fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into())
    }
}

fn parse_attach_request(payload: &[u8]) -> anyhow::Result<(String, u16, u16, Option<String>)> {
    if payload.len() < 2 + 2 + 2 {
        anyhow::bail!("attach payload too small");
    }
    let key_len = u16::from_le_bytes(payload[0..2].try_into().expect("key_len")) as usize;
    let base_len = 2 + key_len + 2 + 2;
    if payload.len() < base_len {
        anyhow::bail!("attach payload too small for key + rows/cols");
    }
    let key = String::from_utf8(payload[2..2 + key_len].to_vec())
        .map_err(|e| anyhow::anyhow!("attach key utf8 decode error: {e}"))?;
    let rows_off = 2 + key_len;
    let rows = u16::from_le_bytes(payload[rows_off..rows_off + 2].try_into().expect("rows"));
    let cols = u16::from_le_bytes(
        payload[rows_off + 2..rows_off + 4]
            .try_into()
            .expect("cols"),
    );

    if payload.len() == base_len {
        return Ok((key, rows, cols, None));
    }

    let cwd_hdr_off = base_len;
    if payload.len() < cwd_hdr_off + 2 {
        anyhow::bail!("attach payload truncated (cwd length)");
    }
    let cwd_len =
        u16::from_le_bytes(payload[cwd_hdr_off..cwd_hdr_off + 2].try_into().unwrap()) as usize;
    let expected = cwd_hdr_off + 2 + cwd_len;
    if payload.len() != expected {
        anyhow::bail!("attach payload length mismatch (cwd)");
    }
    let working_dir = if cwd_len == 0 {
        None
    } else {
        Some(
            String::from_utf8(payload[cwd_hdr_off + 2..expected].to_vec())
                .map_err(|e| anyhow::anyhow!("attach cwd utf8 decode error: {e}"))?,
        )
    };
    Ok((key, rows, cols, working_dir))
}

/// Build the binary payload for [`FRAME_ATTACH`]. Returns `None` if the session key or cwd is too long.
pub fn attach_request_payload(
    session_key: &str,
    rows: u16,
    cols: u16,
    working_dir: Option<&str>,
) -> Option<Vec<u8>> {
    let key_bytes = session_key.as_bytes();
    if key_bytes.len() > u16::MAX as usize {
        return None;
    }
    let cwd_bytes = match working_dir {
        None | Some("") => None,
        Some(dir) => {
            let b = dir.as_bytes();
            if b.len() > u16::MAX as usize {
                return None;
            }
            Some(b)
        }
    };

    let mut payload = Vec::with_capacity(
        2 + key_bytes.len() + 2 + 2 + 2 + cwd_bytes.map(|b| b.len()).unwrap_or(0),
    );
    payload.extend_from_slice(&(key_bytes.len() as u16).to_le_bytes());
    payload.extend_from_slice(key_bytes);
    payload.extend_from_slice(&rows.to_le_bytes());
    payload.extend_from_slice(&cols.to_le_bytes());
    if let Some(b) = cwd_bytes {
        payload.extend_from_slice(&(b.len() as u16).to_le_bytes());
        payload.extend_from_slice(b);
    }
    Some(payload)
}

fn history_max_bytes() -> usize {
    std::env::var("MULTERM_DAEMON_HISTORY_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8 * 1024 * 1024)
}

/// Run the background session daemon (PTY owner).
///
/// Protocol overview:
/// - Client -> daemon: `FRAME_ATTACH` (session key + rows/cols, optional UTF-8 cwd)
/// - Daemon -> client: `FRAME_OUTPUT` frames (history first, then live output)
/// - Client -> daemon during session: `FRAME_INPUT` and `FRAME_RESIZE`
pub fn run_daemon() -> anyhow::Result<()> {
    let port_file = daemon_port_file_path()?;
    let base_dir = port_file.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(base_dir)?;

    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    fs::write(&port_file, port.to_string())?;

    tracing::info!("multerm session daemon listening on 127.0.0.1:{port}");

    let (cmd_tx, cmd_rx) = unbounded::<DaemonCmd>();
    let accept_cmd_tx = cmd_tx.clone();

    // Accept connections on a background thread; session I/O is handled on this thread.
    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let cmd_tx = accept_cmd_tx.clone();
                    thread::spawn(move || {
                        if let Err(e) = handle_client(stream, cmd_tx) {
                            tracing::warn!("daemon client handler error: {e}");
                        }
                    });
                }
                Err(e) => tracing::warn!("daemon accept error: {e}"),
            }
        }
    });

    let mut sessions: HashMap<String, Session> = HashMap::new();
    let max_bytes = history_max_bytes();

    loop {
        // 1) Handle incoming commands.
        match cmd_rx.recv_timeout(std::time::Duration::from_millis(10)) {
            Ok(cmd) => match cmd {
                DaemonCmd::Attach {
                    session_key,
                    rows,
                    cols,
                    working_dir,
                    client_out_tx,
                    resp_tx,
                } => {
                    let entry = sessions.entry(session_key.clone());
                    match entry {
                        std::collections::hash_map::Entry::Vacant(v) => {
                            let shell = default_shell();
                            let (pty_out_tx, pty_out_rx) = unbounded::<Vec<u8>>();
                            let wake_up: Box<dyn Fn() + Send + 'static> = Box::new(|| {});
                            let pty = spawn_pty(
                                &shell,
                                rows,
                                cols,
                                pty_out_tx,
                                wake_up,
                                working_dir.as_deref(),
                                None,
                            );

                            match pty {
                                Ok(pty) => {
                                    v.insert(Session {
                                        pty,
                                        output_rx: pty_out_rx,
                                        history: VecDeque::new(),
                                        history_bytes: 0,
                                        attached_client: Some(client_out_tx),
                                        attach_gen: 1,
                                    });

                                    let snapshot = Vec::<Vec<u8>>::new();
                                    let _ = resp_tx.send(Ok((snapshot, 1)));
                                }
                                Err(e) => {
                                    let _ = resp_tx.send(Err(format!("spawn_pty failed: {e}")));
                                }
                            }
                        }
                        std::collections::hash_map::Entry::Occupied(mut o) => {
                            let session = o.get_mut();
                            // Force-replace any existing client (handles the race where a quick
                            // app restart connects before the old Detach is processed).
                            session.attach_gen += 1;
                            let gen = session.attach_gen;
                            session.attached_client = Some(client_out_tx);
                            let _ = session.pty.resize(rows, cols);
                            let snapshot = session.history.iter().cloned().collect();
                            let _ = resp_tx.send(Ok((snapshot, gen)));
                        }
                    }
                }
                DaemonCmd::Detach { session_key, gen } => {
                    if let Some(session) = sessions.get_mut(&session_key) {
                        // Only clear if this Detach belongs to the current attach generation.
                        if session.attach_gen == gen {
                            session.attached_client = None;
                        }
                    }
                }
                DaemonCmd::Input { session_key, data } => {
                    if let Some(session) = sessions.get_mut(&session_key) {
                        let _ = session.pty.write_all(&data);
                    }
                }
                DaemonCmd::Resize {
                    session_key,
                    rows,
                    cols,
                } => {
                    if let Some(session) = sessions.get_mut(&session_key) {
                        let _ = session.pty.resize(rows, cols);
                    }
                }
            },
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        // Drain commands without blocking.
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                DaemonCmd::Attach {
                    session_key,
                    rows,
                    cols,
                    working_dir,
                    client_out_tx,
                    resp_tx,
                } => {
                    let entry = sessions.entry(session_key.clone());
                    match entry {
                        std::collections::hash_map::Entry::Vacant(v) => {
                            let shell = default_shell();
                            let (pty_out_tx, pty_out_rx) = unbounded::<Vec<u8>>();
                            let wake_up: Box<dyn Fn() + Send + 'static> = Box::new(|| {});
                            let pty = spawn_pty(
                                &shell,
                                rows,
                                cols,
                                pty_out_tx,
                                wake_up,
                                working_dir.as_deref(),
                                None,
                            );

                            match pty {
                                Ok(pty) => {
                                    v.insert(Session {
                                        pty,
                                        output_rx: pty_out_rx,
                                        history: VecDeque::new(),
                                        history_bytes: 0,
                                        attached_client: Some(client_out_tx),
                                        attach_gen: 1,
                                    });

                                    let snapshot = Vec::<Vec<u8>>::new();
                                    let _ = resp_tx.send(Ok((snapshot, 1)));
                                }
                                Err(e) => {
                                    let _ = resp_tx.send(Err(format!("spawn_pty failed: {e}")));
                                }
                            }
                        }
                        std::collections::hash_map::Entry::Occupied(mut o) => {
                            let session = o.get_mut();
                            // Force-replace any existing client (handles the race where a quick
                            // app restart connects before the old Detach is processed).
                            session.attach_gen += 1;
                            let gen = session.attach_gen;
                            session.attached_client = Some(client_out_tx);
                            let _ = session.pty.resize(rows, cols);
                            let snapshot = session.history.iter().cloned().collect();
                            let _ = resp_tx.send(Ok((snapshot, gen)));
                        }
                    }
                }
                DaemonCmd::Detach { session_key, gen } => {
                    if let Some(session) = sessions.get_mut(&session_key) {
                        // Only clear if this Detach belongs to the current attach generation.
                        if session.attach_gen == gen {
                            session.attached_client = None;
                        }
                    }
                }
                DaemonCmd::Input { session_key, data } => {
                    if let Some(session) = sessions.get_mut(&session_key) {
                        let _ = session.pty.write_all(&data);
                    }
                }
                DaemonCmd::Resize {
                    session_key,
                    rows,
                    cols,
                } => {
                    if let Some(session) = sessions.get_mut(&session_key) {
                        let _ = session.pty.resize(rows, cols);
                    }
                }
            }
        }

        // 2) Drain PTY outputs for every session.
        for session in sessions.values_mut() {
            while let Ok(chunk) = session.output_rx.try_recv() {
                session.history_bytes = session.history_bytes.saturating_add(chunk.len());
                session.history.push_back(chunk.clone());
                while session.history_bytes > max_bytes {
                    if let Some(front) = session.history.pop_front() {
                        session.history_bytes = session.history_bytes.saturating_sub(front.len());
                    } else {
                        break;
                    }
                }

                if let Some(attached) = &session.attached_client {
                    let _ = attached.send(chunk);
                }
            }
        }
    }

    Ok(())
}

fn handle_client(mut stream: TcpStream, cmd_tx: Sender<DaemonCmd>) -> anyhow::Result<()> {
    let (frame_type, payload) = read_frame(&mut stream)?;
    if frame_type != FRAME_ATTACH {
        anyhow::bail!("expected attach frame, got {frame_type}");
    }

    let (session_key, rows, cols, working_dir) = parse_attach_request(&payload)?;
    let session_key_for_cmd = session_key.clone();

    let (out_tx, out_rx) = unbounded::<Vec<u8>>();
    let (resp_tx, resp_rx) = unbounded::<AttachResult>();

    cmd_tx
        .send(DaemonCmd::Attach {
            session_key: session_key.clone(),
            rows,
            cols,
            working_dir,
            client_out_tx: out_tx,
            resp_tx,
        })
        .map_err(|e| anyhow::anyhow!("failed to send attach cmd: {e}"))?;

    let (attach_history, attach_gen) = match resp_rx.recv()? {
        Ok((h, gen)) => (h, gen),
        Err(msg) => {
            let mut s = stream.try_clone()?;
            write_frame(&mut s, FRAME_ATTACH_ERROR, msg.as_bytes())?;
            return Ok(());
        }
    };

    // Split streams: daemon -> client writer, client -> daemon reader.
    let mut cmd_stream = stream.try_clone()?;
    let mut out_stream = stream;

    // Spawn command reader thread.
    let cmd_tx_for_cmd = cmd_tx.clone();
    thread::spawn(move || loop {
        let frame = read_frame(&mut cmd_stream);
        let Ok((ft, payload)) = frame else {
            let _ = cmd_tx_for_cmd.send(DaemonCmd::Detach {
                session_key: session_key_for_cmd.clone(),
                gen: attach_gen,
            });
            break;
        };

        match ft {
            FRAME_INPUT => {
                let _ = cmd_tx_for_cmd.send(DaemonCmd::Input {
                    session_key: session_key_for_cmd.clone(),
                    data: payload,
                });
            }
            FRAME_RESIZE => {
                if payload.len() == 4 {
                    let rows = u16::from_le_bytes(payload[0..2].try_into().unwrap());
                    let cols = u16::from_le_bytes(payload[2..4].try_into().unwrap());
                    let _ = cmd_tx_for_cmd.send(DaemonCmd::Resize {
                        session_key: session_key_for_cmd.clone(),
                        rows,
                        cols,
                    });
                }
            }
            _ => {}
        }
    });

    // Send replay history first.
    for chunk in &attach_history {
        write_frame(&mut out_stream, FRAME_OUTPUT, chunk)?;
    }

    // Then forward live output until disconnected.
    while let Ok(chunk) = out_rx.recv() {
        if let Err(e) = write_frame(&mut out_stream, FRAME_OUTPUT, &chunk) {
            let _ = cmd_tx.send(DaemonCmd::Detach {
                session_key,
                gen: attach_gen,
            });
            return Err(e.into());
        }
    }

    let _ = cmd_tx.send(DaemonCmd::Detach {
        session_key,
        gen: attach_gen,
    });
    Ok(())
}
