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
    // Room for the longest answer up front: a growing buffer would leave copies behind.
    let mut bytes = Zeroizing::new(Vec::with_capacity(1024));
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

/// A line the person may see as they type: a menu choice or a folder, never a secret.
/// Ctrl-D cancels and the caller's timeout ends the wait.
pub fn visible(tty: &mut File, label: &str, timeout: Duration) -> Result<String, &'static str> {
    let fd = tty.as_raw_fd();
    let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) } != 0 {
        return Err("interactive_terminal_required");
    }
    let saved = unsafe { saved.assume_init() };
    let mut mode = saved;
    // Ordinary line editing with echo. Ctrl-C stops the program as anywhere else; the mode
    // set here is the usual one, so nothing is left changed.
    mode.c_lflag |= libc::ECHO | libc::ICANON | libc::ECHOE | libc::ISIG;
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &mode) } != 0 {
        return Err("terminal_unavailable");
    }
    let _restore = Restore(fd, saved);
    tty.write_all(label.as_bytes())
        .map_err(|_| "terminal_unavailable")?;
    tty.flush().map_err(|_| "terminal_unavailable")?;
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    loop {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            return Err("input_timeout");
        };
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready =
            unsafe { libc::poll(&mut poll, 1, left.as_millis().min(i32::MAX as u128) as i32) };
        if ready == 0 {
            return Err("input_timeout");
        }
        if ready < 0 {
            return Err("input_cancelled");
        }
        let mut byte = [0];
        if tty.read(&mut byte).unwrap_or(0) != 1 {
            return Err("input_cancelled");
        }
        match byte[0] {
            b'\r' | b'\n' => {
                // Escape alone cancels; arrow keys and other escape sequences are dropped.
                if bytes == [27] {
                    return Err("input_cancelled");
                }
                return String::from_utf8(without_escapes(&bytes)).map_err(|_| "invalid_input");
            }
            3 | 4 => return Err("input_cancelled"),
            b if bytes.len() < 4096 => bytes.push(b),
            _ => return Err("invalid_input"),
        }
    }
}

/// A typed line without terminal escape sequences (arrow keys, Home, Delete) or other control
/// bytes.
pub fn without_escapes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            27 => {
                i += 1;
                if matches!(bytes.get(i), Some(b'[') | Some(b'O')) {
                    i += 1;
                    // Parameters, then one final byte from @ to ~.
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                }
                i += 1;
            }
            b if b < 32 || b == 127 => i += 1,
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}
