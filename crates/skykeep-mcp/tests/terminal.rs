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
