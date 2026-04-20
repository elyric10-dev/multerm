use std::io::{Read, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;

use anyhow::Context;
use crossbeam_channel::Sender;
use parking_lot::Mutex;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// A running PTY subprocess.
pub struct PtyHandle {
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn portable_pty::MasterPty>,
    pub stop_flag: Arc<AtomicBool>,
}

impl PtyHandle {
    /// Resize the PTY to `rows × cols`.
    pub fn resize(&self, rows: u16, cols: u16) -> anyhow::Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("PTY resize")?;
        Ok(())
    }

    /// Write bytes to the PTY (keyboard input).
    pub fn write_all(&self, data: &[u8]) -> anyhow::Result<()> {
        self.writer.lock().write_all(data).context("PTY write")?;
        Ok(())
    }
}

impl Drop for PtyHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
    }
}

/// Spawn a PTY with the given shell.
///
/// `data_tx` receives raw PTY output bytes.
/// `wake_up` is called after each chunk is sent to let the event loop know
/// there is new data (avoids polling; implement as `proxy.send_event(…)`).
pub fn spawn_pty(
    shell: &str,
    rows: u16,
    cols: u16,
    data_tx: Sender<Vec<u8>>,
    wake_up: Box<dyn Fn() + Send + 'static>,
    working_dir: Option<&str>,
    startup_command: Option<&str>,
) -> anyhow::Result<PtyHandle> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty")?;

    // Spawn the shell
    let mut cmd = CommandBuilder::new(shell);
    if let Some(command) = startup_command {
        cmd.arg("-lc");
        cmd.arg(command);
    }
    cmd.env("TERM", "xterm-256color");
    if let Some(dir) = working_dir {
        cmd.cwd(dir);
    }
    let _child = pair.slave.spawn_command(cmd).context("spawn_command")?;

    // Close slave side in this process after spawn
    drop(pair.slave);

    let writer = pair.master.take_writer().context("take_writer")?;
    let mut reader = pair.master.try_clone_reader().context("try_clone_reader")?;

    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_flag);

    thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        let chunk = buf[..n].to_vec();
                        if data_tx.send(chunk).is_err() {
                            break;
                        }
                        wake_up();
                    }
                    Err(_) => break,
                }
            }
        })
        .context("spawn PTY reader thread")?;

    Ok(PtyHandle {
        writer: Arc::new(Mutex::new(writer)),
        master: pair.master,
        stop_flag,
    })
}
