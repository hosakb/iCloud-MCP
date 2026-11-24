//! The program is called iCloud-MCP and its command is `icloud-mcp`. Nothing a person reads
//! from this crate (help, status, setup, error and tool texts) names the old name. The few
//! spellings that stay are in [`ALLOWED`], each with its reason. The inherited engine crates
//! are not scanned: they keep their upstream names.
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The old name, assembled so this file's own text is not a match.
fn old() -> String {
    ["sky", "keep"].concat()
}

/// Spellings of the old name that stay in this crate, and why.
const ALLOWED: &[&str] = &[
    // The default folder under the old name, still used when the new one does not exist.
    ".local/share/skykeep-mcp",
    // Partial downloads the program left under its old name, still cleaned up.
    ".skykeep-download-",
    // The kernel key name of an unlock under the old name, only ever forgotten.
    "skykeep-mcp:{}:{}",
    // Inputs of stored fingerprints: changing them would make saved approvals stale.
    "skykeep-calendar-notes:",
    "skykeep-permissions:",
];

/// Whether `text` holds the old name anywhere outside [`ALLOWED`].
fn holds_old_name(text: &str) -> bool {
    let mut rest = text.to_string();
    for allowed in ALLOWED {
        rest = rest.replace(allowed, "");
    }
    rest.to_lowercase().contains(&old())
}

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, found);
        } else if path.extension().is_some_and(|e| e == "rs") {
            found.push(path);
        }
    }
}

/// The string literals of one Rust file, with comments and test-only items (the block after
/// `#[cfg(test)]`, such as `mod tests { ... }`) left out. Production code after a test module
/// is read too. Raw strings and character literals are read as Rust reads them.
fn literals(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    // Brace depth, the depth a test-only block started at, and whether `#[cfg(test)]` was
    // seen and its item has not begun yet.
    let mut depth = 0usize;
    let mut skip: Option<usize> = None;
    let mut pending = false;
    let marker: Vec<char> = "#[cfg(test)]".chars().collect();
    while i < chars.len() {
        let c = chars[i];
        if chars[i..].starts_with(&marker) {
            pending = skip.is_none();
            i += marker.len();
            continue;
        }
        if c == '{' {
            if pending {
                skip = Some(depth);
                pending = false;
            }
            depth += 1;
            i += 1;
            continue;
        }
        if c == '}' {
            depth = depth.saturating_sub(1);
            if skip == Some(depth) {
                skip = None;
            }
            i += 1;
            continue;
        }
        if c == ';' {
            pending = false;
        }
        let keep = skip.is_none();
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if c == '\'' {
            // A character literal ('"', '\'', '\u{..}'), or a lifetime.
            let end = chars[i + 1..].iter().take(12).position(|c| *c == '\'');
            match end {
                Some(n) if n == 1 || chars[i + 1] == '\\' => i += n + 2,
                _ => i += 1,
            }
        } else if c == 'r'
            && (chars.get(i + 1) == Some(&'"') || chars.get(i + 1) == Some(&'#'))
            && (i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_'))
        {
            let mut j = i + 1;
            let mut hashes = 0;
            while chars.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            if chars.get(j) != Some(&'"') {
                i += 1;
                continue;
            }
            let close: String = std::iter::once('"')
                .chain(std::iter::repeat_n('#', hashes))
                .collect();
            let rest: String = chars[j + 1..].iter().collect();
            let end = rest.find(&close).unwrap_or(rest.len());
            if keep {
                out.push(rest[..end].to_string());
            }
            i = j + 1 + rest[..end].chars().count() + close.chars().count();
        } else if c == '"' {
            let mut j = i + 1;
            let mut s = String::new();
            while j < chars.len() && chars[j] != '"' {
                if chars[j] == '\\' {
                    s.push(chars[j]);
                    j += 1;
                }
                if j < chars.len() {
                    s.push(chars[j]);
                }
                j += 1;
            }
            if keep {
                out.push(s);
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// Every string literal of the crate's own code (tests left out) is free of the old name.
#[test]
fn no_text_of_this_crate_names_the_old_name() {
    let mut files = Vec::new();
    rust_files(&crate_dir().join("src"), &mut files);
    assert!(files.len() > 20, "the scan found only {files:?}");
    let mut found = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        for literal in literals(&text) {
            if holds_old_name(&literal) {
                let shown: String = literal.chars().take(120).collect();
                found.push(format!("{}: {shown}", file.display()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "say iCloud-MCP or icloud-mcp instead, or add the spelling to ALLOWED with its reason:\n{}",
        found.join("\n")
    );
}

/// The allow-list only lets through what it names.
#[test]
fn the_allow_list_is_narrow() {
    assert!(!holds_old_name("~/.local/share/skykeep-mcp"));
    assert!(!holds_old_name("icloud-mcp setup"));
    for named in [
        "run skykeep-mcp setup",
        "Skykeep",
        "/opt/skykeep/icloud-mcp",
        "SKYKEEP_LOG",
    ] {
        assert!(holds_old_name(named), "{named}");
    }
}

/// The scanner reads literals of each kind and skips comments and test modules.
#[test]
fn the_scan_reads_rust_literals() {
    let text = "// \"Skykeep in a comment\"\nlet a = '\"'; let b = \"one \\\"q\\\" Skykeep\";\nlet c = r#\"raw \"x\" Skykeep\"#;\nfn f<'a>(x: &'a str) {}\n#[cfg(test)]\nmod t { const X: &str = \"Skykeep\"; }\nfn late() -> &'static str { \"after\" }\n";
    assert_eq!(
        literals(text),
        vec!["one \\\"q\\\" Skykeep", "raw \"x\" Skykeep", "after"]
    );
}

fn program(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_icloud-mcp"));
    c.env("HOME", home)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent/icloud-mcp-test-bus",
        )
        .stdin(Stdio::null());
    for marker in icloud_mcp::terminal::AGENT_MARKERS {
        c.env_remove(marker);
    }
    c
}

fn private(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// A home folder of its own for one test, never the real one.
fn synthetic_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    private(home.path());
    home
}

/// The help of the program and of each command names icloud-mcp, and the old name only for
/// the old folder.
#[test]
fn help_names_the_new_command() {
    let home = synthetic_home();
    let out = program(home.path()).arg("--help").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("The command is icloud-mcp."), "{text}");
    assert!(text.contains("~/.local/share/icloud-mcp"), "{text}");
    assert!(!holds_old_name(&text), "{text}");
    for command in [
        "setup",
        "status",
        "unlock",
        "permissions",
        "exchange",
        "login",
        "logout",
        "policy",
        "approve",
        "quarantine",
        "require-approval",
        "allow-routine",
    ] {
        let out = program(home.path())
            .args([command, "--help"])
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(text.contains("icloud-mcp"), "{command}: {text}");
        assert!(!holds_old_name(&text), "{command}: {text}");
    }
}

/// With neither folder there, the new one is the default.
#[test]
fn a_new_home_uses_the_new_folder() {
    let home = synthetic_home();
    assert_eq!(
        icloud_mcp::setup::default_in(home.path()),
        home.path().join(".local/share/icloud-mcp")
    );
    let out = program(home.path()).arg("status").output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("No iCloud-MCP folder at"), "{text}");
    assert!(text.contains(".local/share/icloud-mcp"), "{text}");
    assert!(text.contains("Run icloud-mcp setup"), "{text}");
    assert!(!text.to_lowercase().contains(&old()), "{text}");
    assert!(!home.path().join(".local/share/icloud-mcp").exists());
}

/// Only the old folder there: it is used as it is, status says so, and nothing in it is moved,
/// copied or changed, and no new folder appears.
#[test]
fn an_old_folder_is_used_as_it_is() {
    let home = synthetic_home();
    let share = home.path().join(".local/share");
    let old_dir = share.join(format!("{}-mcp", old()));
    let new_dir = share.join("icloud-mcp");
    private(&old_dir);
    let marker = old_dir.join("synthetic-marker");
    std::fs::write(&marker, b"synthetic, not a credential").unwrap();
    std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(icloud_mcp::setup::default_in(home.path()), old_dir);

    let out = program(home.path()).arg("status").output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains(&format!("Folder: {} (private to you)", old_dir.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "iCloud-MCP uses {}, the folder from its old name",
            old_dir.display()
        )),
        "{text}"
    );
    assert!(text.contains("It can stay where it is"), "{text}");
    assert!(text.contains("icloud-mcp setup"), "{text}");
    assert!(!new_dir.exists(), "no new folder is made");
    assert!(old_dir.is_dir());
    assert_eq!(
        std::fs::read(&marker).unwrap(),
        b"synthetic, not a credential"
    );
}

/// Once the new folder exists it wins, and the old one is left alone.
#[test]
fn the_new_folder_wins_when_both_exist() {
    let home = synthetic_home();
    let share = home.path().join(".local/share");
    let old_dir = share.join(format!("{}-mcp", old()));
    let new_dir = share.join("icloud-mcp");
    private(&old_dir);
    private(&new_dir);
    assert_eq!(icloud_mcp::setup::default_in(home.path()), new_dir);
    let out = program(home.path()).arg("status").output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains(&format!("Folder: {} (private to you)", new_dir.display())),
        "{text}"
    );
    assert!(!text.contains("folder from its old name"), "{text}");
    assert!(old_dir.is_dir());
}

/// A folder given with --state-dir is never called the old folder, even under the old name.
#[test]
fn a_named_folder_is_not_the_old_default() {
    let home = synthetic_home();
    let elsewhere = home.path().join(format!("{}-mcp", old()));
    private(&elsewhere);
    let out = program(home.path())
        .args(["status", "--state-dir"])
        .arg(&elsewhere)
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("OK   Folder:"), "{text}");
    assert!(!text.contains("folder from its old name"), "{text}");
}

/// The diagram names iCloud-MCP and never the old name.
#[test]
fn the_architecture_diagram_says_icloud_mcp() {
    let html = std::fs::read_to_string(crate_dir().join("docs/architecture.html")).unwrap();
    assert!(html.contains("iCloud-MCP"));
    assert!(!html.to_lowercase().contains(&old()));
}

/// The README, the docs and the workflow name the old name only where they credit the
/// upstream project, for the old folder, for the old command, or for the keyring service the
/// upstream engine sets.
#[test]
fn the_docs_name_the_old_name_only_for_upstream_and_the_old_folder() {
    let root = crate_dir().join("../..");
    let mut docs: Vec<PathBuf> = [
        "README.md",
        "SECURITY.md",
        "PRIVACY.md",
        "ARCHITECTURE.md",
        ".github/workflows/ci.yml",
        "Cargo.toml",
    ]
    .iter()
    .map(|name| root.join(name))
    .collect();
    for entry in std::fs::read_dir(root.join(".github")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "md") {
            docs.push(path);
        }
    }
    let mut stray = Vec::new();
    for doc in &docs {
        let text = std::fs::read_to_string(doc).unwrap();
        // The README tells people who still run the old command to switch.
        let text = text.replace("old `skykeep-mcp` command", "");
        for line in text.lines().filter(|line| holds_old_name(line)) {
            let credit = ["fork", "upstream", "Upstream", "Yashwanth Kamireddi"]
                .iter()
                .any(|credit| line.contains(credit));
            if !credit {
                stray.push(format!("{}: {line}", doc.display()));
            }
        }
    }
    assert!(stray.is_empty(), "{stray:#?}");
}
