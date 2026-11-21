use skykeep_mcp::terminal::hidden;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::time::Duration;

fn pair() -> (File, File) {
    let (mut master, mut slave) = (0, 0);
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) }
}

#[test]
fn hidden_input_never_echoes_synthetic_secret_and_restores_terminal() {
    let (mut master, mut slave) = pair();
    let worker = std::thread::spawn(move || {
        hidden(&mut slave, "Synthetic code: ", Duration::from_secs(2)).unwrap()
    });
    let mut prompt = [0; 64];
    let n = master.read(&mut prompt).unwrap();
    assert!(String::from_utf8_lossy(&prompt[..n]).contains("Synthetic code"));
    master.write_all(b"SYNTHETIC-SECRET\n").unwrap();
    assert_eq!(&**worker.join().unwrap(), "SYNTHETIC-SECRET");
    let n = master.read(&mut prompt).unwrap();
    assert!(!String::from_utf8_lossy(&prompt[..n]).contains("SYNTHETIC"));
}

#[test]
fn cancellation_and_timeout_return_no_secret() {
    let (mut master, mut slave) = pair();
    let worker = std::thread::spawn(move || hidden(&mut slave, "Cancel: ", Duration::from_secs(2)));
    let mut prompt = [0; 64];
    assert!(master.read(&mut prompt).unwrap() > 0);
    master.write_all(b"discard-this\x03").unwrap();
    assert_eq!(worker.join().unwrap().unwrap_err(), "input_cancelled");
    let (_master, mut slave) = pair();
    assert_eq!(
        hidden(&mut slave, "Timeout: ", Duration::from_millis(10)).unwrap_err(),
        "input_timeout"
    );
}

#[test]
fn redirected_regular_file_is_not_a_terminal() {
    let mut file = tempfile::tempfile().unwrap();
    assert_eq!(
        hidden(&mut file, "Secret: ", Duration::from_millis(1)).unwrap_err(),
        "interactive_terminal_required"
    );
}

/// A menu answer is shown as typed and returned; the terminal keeps its mode.
#[test]
fn visible_prompt_echoes_and_returns_the_line() {
    let (mut master, mut slave) = pair();
    let typist = std::thread::spawn(move || {
        let mut seen = Vec::new();
        while !seen.ends_with(b": ") {
            let mut buf = [0; 256];
            let n = master.read(&mut buf).unwrap();
            seen.extend_from_slice(&buf[..n]);
        }
        master.write_all(b"2\n").unwrap();
        (master, seen)
    });
    let answer = skykeep_mcp::terminal::visible(
        &mut slave,
        "Choose a number: ",
        std::time::Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(answer, "2");
    let (mut master, mut seen) = typist.join().unwrap();
    let mut buf = [0; 256];
    let n = master.read(&mut buf).unwrap();
    seen.extend_from_slice(&buf[..n]);
    let seen = String::from_utf8_lossy(&seen).to_string();
    assert!(seen.contains("Choose a number: 2"), "{seen:?}");
}

#[test]
fn escape_sequences_are_dropped_from_a_shown_answer() {
    use skykeep_mcp::terminal::without_escapes;
    assert_eq!(without_escapes(b"\x1b[A2"), b"2");
    assert_eq!(without_escapes(b"/srv/x\x1b[D\x1b[C"), b"/srv/x");
    assert_eq!(without_escapes(b"\x1bOA1\x1b[3~"), b"1");
    assert_eq!(without_escapes(b"a\x01b"), b"ab");
}

/// An arrow key doesn't cancel a menu; Escape alone on the line does.
#[test]
fn visible_prompt_ignores_arrow_keys_and_cancels_on_escape_alone() {
    for (typed, expected) in [
        (&b"\x1b[A\x1b[B2\n"[..], Ok("2".to_string())),
        (&b"\x1b\n"[..], Err("input_cancelled")),
    ] {
        let (mut master, mut slave) = pair();
        let typist = std::thread::spawn(move || {
            let mut seen = Vec::new();
            while !seen.ends_with(b": ") {
                let mut buf = [0; 256];
                let n = master.read(&mut buf).unwrap();
                seen.extend_from_slice(&buf[..n]);
            }
            master.write_all(typed).unwrap();
            master
        });
        let answer =
            skykeep_mcp::terminal::visible(&mut slave, "Choose a number: ", Duration::from_secs(5));
        let _master = typist.join().unwrap();
        assert_eq!(answer, expected);
    }
}
