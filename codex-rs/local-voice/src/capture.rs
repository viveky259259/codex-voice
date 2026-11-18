//! Microphone capture through a command that writes s16le mono PCM.

use std::process::Stdio;

use tokio::io::AsyncReadExt;
use tokio::process::ChildStdout;

use crate::config::CaptureConfig;
use crate::error::Result;
use crate::error::VoiceError;
use crate::process::ProcessGuard;
use crate::process::build_command;
use crate::process::failure_message;
use crate::process::program_name;
use crate::template::substitute_argv;

/// A running capture command. Killed (with its process group) on drop.
pub struct Capture {
    argv: Vec<String>,
    guard: ProcessGuard,
    stdout: ChildStdout,
    odd_byte: Option<u8>,
    buffer: Vec<u8>,
}

impl Capture {
    pub fn spawn(config: &CaptureConfig) -> Result<Self> {
        let argv = substitute_argv(&config.command, &[("device", config.device.as_str())]);
        let mut command = build_command(&argv)?;
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        let mut guard = ProcessGuard::spawn(command, &argv)?;
        let stdout = guard.child_mut().stdout.take().ok_or_else(|| {
            VoiceError::Command(format!("`{}` has no stdout", program_name(&argv)))
        })?;
        Ok(Self {
            argv,
            guard,
            stdout,
            odd_byte: None,
            // 100 ms of audio per read at most.
            buffer: vec![0; 3200],
        })
    }

    pub fn pid(&self) -> Option<u32> {
        self.guard.id()
    }

    /// The next block of samples, or `None` once the command's stdout ends.
    pub async fn next_samples(&mut self) -> Option<Vec<i16>> {
        loop {
            let read = match self.stdout.read(&mut self.buffer).await {
                Ok(0) | Err(_) => return None,
                Ok(read) => read,
            };
            let mut bytes = Vec::with_capacity(read + 1);
            if let Some(byte) = self.odd_byte.take() {
                bytes.push(byte);
            }
            bytes.extend_from_slice(&self.buffer[..read]);
            if bytes.len() % 2 == 1 {
                self.odd_byte = bytes.pop();
            }
            if bytes.is_empty() {
                continue;
            }
            return Some(
                bytes
                    .chunks_exact(2)
                    .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                    .collect(),
            );
        }
    }

    /// After stdout ended: why the command stopped, with its stderr tail.
    pub async fn exit_message(&mut self) -> String {
        let status =
            tokio::time::timeout(std::time::Duration::from_secs(2), self.guard.wait()).await;
        let tail = self.guard.stderr_tail().await;
        match status {
            Ok(Ok(status)) => format!(
                "capture stopped: {}",
                failure_message(&self.argv, status, &tail)
            ),
            _ => {
                self.guard.kill_and_wait().await;
                let mut message = format!(
                    "capture stopped: `{}` closed its output",
                    program_name(&self.argv)
                );
                if !tail.is_empty() {
                    message.push_str(": ");
                    message.push_str(&tail);
                }
                message
            }
        }
    }

    pub async fn stop(&mut self) {
        self.guard.kill_and_wait().await;
    }
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;
