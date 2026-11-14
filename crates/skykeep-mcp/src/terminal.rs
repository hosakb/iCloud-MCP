//! This input belongs to a user-operated terminal, never to MCP stdin.
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

/// Environment markers set by agent runtimes for commands they launch.
pub const AGENT_MARKERS: [&str; 5] = [
    "HERMES_AGENT",
    "AI_AGENT",
    "HERMES_SESSION_ID",
    "CODEX_SANDBOX",
    "CLAUDECODE",
];

pub fn agent_marked(get: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    AGENT_MARKERS
        .iter()
        .any(|name| get(name).is_some_and(|v| !v.is_empty()))
}

pub fn controlling() -> Result<File, &'static str> {
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|_| "interactive_terminal_required")?;
    if unsafe { libc::tcgetpgrp(tty.as_raw_fd()) } != unsafe { libc::getpgrp() } {
        return Err("foreground_terminal_required");
    }
    Ok(tty)
}

struct Restore(RawFd, libc::termios);
impl Drop for Restore {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.0, libc::TCSAFLUSH, &self.1);
        }
    }
}

pub fn hidden(
    tty: &mut File,
    label: &str,
    timeout: Duration,
) -> Result<Zeroizing<String>, &'static str> {
    let fd = tty.as_raw_fd();
    let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) } != 0 {
        return Err("interactive_terminal_required");
    }
    let saved = unsafe { saved.assume_init() };
    let mut mode = saved;
    mode.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG);
    mode.c_cc[libc::VMIN] = 1;
    mode.c_cc[libc::VTIME] = 0;
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &mode) } != 0 {
        return Err("terminal_unavailable");
    }
    let _restore = Restore(fd, saved);
    tty.write_all(label.as_bytes())
        .map_err(|_| "terminal_unavailable")?;
    tty.flush().map_err(|_| "terminal_unavailable")?;
    let deadline = Instant::now() + timeout;
    let mut bytes = Zeroizing::new(Vec::new());
    let result = loop {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            break Err("input_timeout");
        };
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready =
            unsafe { libc::poll(&mut poll, 1, left.as_millis().min(i32::MAX as u128) as i32) };
        if ready == 0 {
            break Err("input_timeout");
        }
        if ready < 0 {
            break Err("input_cancelled");
        }
        let mut byte = [0];
        if tty.read(&mut byte).unwrap_or(0) != 1 {
            break Err("input_cancelled");
        }
        match byte[0] {
            b'\r' | b'\n' => {
                break String::from_utf8(bytes.to_vec())
                    .map(Zeroizing::new)
                    .map_err(|_| "invalid_input")
            }
            3 | 4 | 27 => break Err("input_cancelled"),
            8 | 127 => {
                bytes.pop();
            }
            b if b >= 32 && bytes.len() < 1024 => bytes.push(b),
            _ => break Err("invalid_input"),
        }
    };
    let _ = tty.write_all(b"\n");
    result
}
