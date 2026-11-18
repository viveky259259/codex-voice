//! Child processes: argv only (never a shell), own process group, killed
//! (with the whole group) on drop.

use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::process::Command;
use tokio::task::JoinHandle;

use crate::error::Result;
use crate::error::VoiceError;

const STDERR_TAIL_BYTES: usize = 2048;

pub(crate) fn build_command(argv: &[String]) -> Result<Command> {
    let Some((program, args)) = argv.split_first() else {
        return Err(VoiceError::Config("empty command".to_string()));
    };
    let mut command = Command::new(program);
    command.args(args);
    command.stdin(Stdio::null());
    command.kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    Ok(command)
}

pub(crate) fn program_name(argv: &[String]) -> &str {
    argv.first().map(String::as_str).unwrap_or("")
}

/// A spawned child that is killed, with its process group, when dropped.
pub(crate) struct ProcessGuard {
    child: Child,
    pid: Option<u32>,
    reaped: bool,
    stderr: Option<StderrTail>,
}

impl ProcessGuard {
    pub(crate) fn spawn(mut command: Command, argv: &[String]) -> Result<Self> {
        let mut child = command.spawn().map_err(|err| {
            VoiceError::Command(format!("failed to start `{}`: {err}", program_name(argv)))
        })?;
        let stderr = child.stderr.take().map(StderrTail::collect);
        let pid = child.id();
        Ok(Self {
            child,
            pid,
            reaped: false,
            stderr,
        })
    }

    pub(crate) fn id(&self) -> Option<u32> {
        self.pid
    }

    pub(crate) fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    pub(crate) async fn wait(&mut self) -> Result<ExitStatus> {
        let status = self.child.wait().await?;
        self.reaped = true;
        Ok(status)
    }

    /// Kills the process group and reaps the child.
    pub(crate) async fn kill_and_wait(&mut self) {
        self.kill_now();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.wait()).await;
    }

    /// The last bytes the child wrote to stderr (waits briefly for EOF).
    pub(crate) async fn stderr_tail(&mut self) -> String {
        match self.stderr.as_mut() {
            Some(tail) => tail.finish().await,
            None => String::new(),
        }
    }

    fn kill_now(&mut self) {
        if self.reaped {
            return;
        }
        if let Ok(Some(_)) = self.child.try_wait() {
            self.reaped = true;
            return;
        }
        #[cfg(unix)]
        if let Some(pid) = self.pid
            && let Ok(pgid) = libc::pid_t::try_from(pid)
        {
            // SAFETY: plain syscall on a process group we created and have
            // not reaped yet, so the id cannot have been reused.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
        let _ = self.child.start_kill();
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        self.kill_now();
    }
}

struct StderrTail {
    buffer: Arc<Mutex<Vec<u8>>>,
    reader: Option<JoinHandle<()>>,
}

impl StderrTail {
    fn collect(stream: impl AsyncRead + Unpin + Send + 'static) -> Self {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buffer);
        let reader = tokio::spawn(async move {
            let mut stream = stream;
            let mut chunk = [0u8; 1024];
            loop {
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut buffer = sink.lock().unwrap_or_else(PoisonError::into_inner);
                        buffer.extend_from_slice(&chunk[..n]);
                        let excess = buffer.len().saturating_sub(STDERR_TAIL_BYTES);
                        buffer.drain(..excess);
                    }
                }
            }
        });
        Self {
            buffer,
            reader: Some(reader),
        }
    }

    async fn finish(&mut self) -> String {
        if let Some(reader) = self.reader.take() {
            let _ = tokio::time::timeout(Duration::from_millis(500), reader).await;
        }
        let buffer = self.buffer.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&buffer).trim().to_string()
    }
}

impl Drop for StderrTail {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
    }
}

/// Output of a command run to completion.
pub(crate) struct CommandOutput {
    pub stdout: Vec<u8>,
}

/// Runs `argv` to completion with a timeout. A non-zero exit is an error
/// that carries the stderr tail.
pub(crate) async fn run_command(
    argv: &[String],
    timeout_ms: u64,
    capture_stdout: bool,
) -> Result<CommandOutput> {
    let mut command = build_command(argv)?;
    command.stderr(Stdio::piped());
    command.stdout(if capture_stdout {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut guard = ProcessGuard::spawn(command, argv)?;
    let stdout = guard.child_mut().stdout.take();
    let run = async {
        let mut out = Vec::new();
        if let Some(mut stdout) = stdout {
            stdout.read_to_end(&mut out).await?;
        }
        let status = guard.wait().await?;
        Ok::<_, VoiceError>((out, status))
    };
    let outcome = tokio::time::timeout(Duration::from_millis(timeout_ms), run).await;
    let (stdout, status) = match outcome {
        Ok(result) => result?,
        Err(_) => {
            guard.kill_and_wait().await;
            return Err(VoiceError::Command(format!(
                "`{}` timed out after {timeout_ms} ms",
                program_name(argv)
            )));
        }
    };
    if !status.success() {
        let tail = guard.stderr_tail().await;
        return Err(VoiceError::Command(failure_message(argv, status, &tail)));
    }
    Ok(CommandOutput { stdout })
}

/// Runs `argv` until it exits or `cancelled` resolves, in which case the
/// process group is killed and reaped and `Interrupted` returned.
pub(crate) async fn run_cancellable(
    argv: &[String],
    cancelled: impl std::future::Future<Output = ()>,
    on_spawn: impl FnOnce(Option<u32>),
) -> Result<()> {
    let mut command = build_command(argv)?;
    command.stderr(Stdio::piped());
    command.stdout(Stdio::null());
    let mut guard = ProcessGuard::spawn(command, argv)?;
    on_spawn(guard.id());
    let status = tokio::select! {
        status = guard.wait() => status?,
        () = cancelled => {
            guard.kill_and_wait().await;
            return Err(VoiceError::Interrupted);
        }
    };
    if !status.success() {
        let tail = guard.stderr_tail().await;
        return Err(VoiceError::Command(failure_message(argv, status, &tail)));
    }
    Ok(())
}

pub(crate) fn failure_message(argv: &[String], status: ExitStatus, stderr_tail: &str) -> String {
    let mut message = format!("`{}` exited with {status}", program_name(argv));
    if !stderr_tail.is_empty() {
        message.push_str(": ");
        message.push_str(stderr_tail);
    }
    message
}
