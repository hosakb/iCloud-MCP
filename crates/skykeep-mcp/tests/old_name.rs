//! The product is called iCloud-MCP in everything a person reads. The command, the file names,
//! the crate names, the keyring service and stored keys keep `skykeep-mcp`, and the upstream
//! project is named where the fork credits it. Any other mention of the old name in a string
//! literal of the workspace's source, in the README or in the architecture diagram fails here.
use std::path::{Path, PathBuf};

/// Spellings that stay: names a person types or the system stores, and the upstream credit.
/// `skykeep-mcp` itself stays only where it is typed or stored as a name ([`typed`]).
const KEPT: &[&str] = &[
    // Error codes name the command they ask for.
    "skykeep_mcp",
    // The upstream project's address.
    "github.com/YashwanthKamireddi/Skykeep",
    // The id of an approval card request, which the client sees but shows no one.
    "skykeep-consent-",
    // A test address that never resolves.
    "refused.skykeep.invalid",
    // Files in the private folder and the exchange folder.
    "skykeep.lock",
    "skykeep.pid",
    ".skykeep-download-",
    // Stored fingerprints' inputs: changing them would make cached approvals stale.
    "skykeep-calendar-notes:",
    "skykeep-permissions:",
    // The client's old server key, named where people are told to move its rules.
    "old name skykeep",
    "old name `skykeep`",
    // The snap's name, as `snap connect` needs it typed.
    "skykeep:password-manager-service",
];

/// Whole literals that are names, not text: the snap package's name, and the text the
/// folder is added after in a command (`status::with_dir`).
const KEPT_LITERALS: &[&str] = &["skykeep", "skykeep-mcp "];

/// The words that can follow `skykeep-mcp ` when it is a command line.
const COMMANDS: &[&str] = &[
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
    "help",
    "COMMAND",
];

/// Whether the `skykeep-mcp` at byte `at` of `text` is typed or stored as a name: a command
/// line, a path, a quoted name, a prefix of a stored name, or "the command is skykeep-mcp".
/// Anywhere else it names the product, which is iCloud-MCP.
fn typed(text: &str, at: usize) -> bool {
    let before = &text[..at];
    let after = &text[at + "skykeep-mcp".len()..];
    if before.ends_with('/') || before.ends_with("command is ") || after.starts_with(" command") {
        return true;
    }
    match after.chars().next() {
        None | Some('`' | '"' | '-' | '\'' | '\\') => true,
        // A stored name such as `skykeep-mcp:{dev}`, or the prefix of an error line.
        Some(':') => !after[1..].starts_with(' ') || before.trim().is_empty(),
        Some(' ') => {
            let word: String = after[1..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '[' | '{'))
                .collect();
            COMMANDS.contains(&word.as_str())
                || word.starts_with("--")
                || word.starts_with('[')
                || word.starts_with('{')
        }
        _ => false,
    }
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
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

/// What is left of the old name in `text` once the kept spellings are taken out.
fn old_name_in(text: &str) -> bool {
    let mut rest = text.to_string();
    for kept in KEPT {
        rest = rest.replace(kept, "");
    }
    let mut left = String::new();
    let mut from = 0;
    while let Some(found) = rest[from..].find("skykeep-mcp") {
        let at = from + found;
        left.push_str(&rest[from..at]);
        if !typed(&rest, at) {
            left.push_str("skykeep-mcp");
        }
        from = at + "skykeep-mcp".len();
    }
    left.push_str(&rest[from..]);
    left.to_lowercase().contains("skykeep")
}

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, found);
        } else if path.extension().is_some_and(|e| e == "rs") {
            // Files that hold only tests.
            let name = path.file_name().unwrap().to_string_lossy();
            if !(name == "tests.rs" || name.ends_with("_tests.rs")) {
                found.push(path);
            }
        }
    }
}

#[test]
fn no_user_facing_string_holds_the_old_name() {
    let root = workspace();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(root.join("crates")).unwrap() {
        let src = entry.unwrap().path().join("src");
        if src.is_dir() {
            rust_files(&src, &mut files);
        }
    }
    assert!(files.len() > 50, "the scan found only {files:?}");
    let mut found = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        for literal in literals(&text) {
            if old_name_in(&literal) && !KEPT_LITERALS.contains(&literal.as_str()) {
                let shown: String = literal.chars().take(120).collect();
                found.push(format!(
                    "{}: {shown}",
                    file.strip_prefix(&root).unwrap().display()
                ));
            }
        }
    }
    assert!(
        found.is_empty(),
        "say iCloud-MCP instead, or add the spelling to KEPT with its reason:\n{}",
        found.join("\n")
    );
}

/// The diagram never names the old product.
#[test]
fn the_architecture_diagram_says_icloud_mcp() {
    let html =
        std::fs::read_to_string(workspace().join("crates/skykeep-mcp/docs/architecture.html"))
            .unwrap();
    let text: String = html
        .split("<text")
        .skip(1)
        .chain(html.split("<h1").skip(1))
        .chain(html.split("<title").skip(1))
        .filter_map(|s| s.split_once('>').and_then(|(_, t)| t.split('<').next()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("iCloud-MCP"), "{text}");
    assert!(!text.to_lowercase().contains("skykeep"), "{text}");
}

/// The README and the other docs name the old product only where they credit the fork's
/// upstream, and show commands as typed.
#[test]
fn the_readme_names_the_old_product_only_as_the_upstream() {
    let root = workspace();
    let mut docs: Vec<PathBuf> = ["README.md", "SECURITY.md", "PRIVACY.md", "ARCHITECTURE.md"]
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
        for line in text.lines().filter(|line| old_name_in(line)) {
            let credit = ["fork", "upstream", "Yashwanth Kamireddi"]
                .iter()
                .any(|credit| line.contains(credit));
            if !credit {
                stray.push(format!("{}: {line}", doc.display()));
            }
        }
    }
    assert!(stray.is_empty(), "{stray:#?}");
}

/// The scanner itself: comments and test modules are skipped, literals of each kind are read.
#[test]
fn the_scan_reads_rust_literals() {
    let text = "// \"Skykeep in a comment\"\nlet a = '\"'; let b = \"one \\\"q\\\" Skykeep\";\nlet c = r#\"raw \"x\" Skykeep\"#;\nfn f<'a>(x: &'a str) {}\n#[cfg(test)]\nmod t { const X: &str = \"Skykeep\"; }\n";
    let found = literals(text);
    assert_eq!(
        found,
        vec![
            "one \\\"q\\\" Skykeep".to_string(),
            "raw \"x\" Skykeep".into()
        ]
    );
    assert!(old_name_in("Run Skykeep"));
    assert!(!old_name_in("run skykeep-mcp setup"));
}

/// Production code after an early test module is still read, and only the test block is
/// skipped.
#[test]
fn the_scan_reads_production_code_after_a_test_module() {
    let text = "const A: &str = \"first\";\n#[cfg(test)]\nmod tests {\n    fn f() { let s = \"in test {\"; if true { g(\"Skykeep test\"); } }\n}\n\nfn late() -> &'static str { \"Skykeep late\" }\n#[cfg(test)]\nuse std::fmt;\nconst B: &str = \"after use\";\n";
    assert_eq!(literals(text), vec!["first", "Skykeep late", "after use"]);
}

/// `skykeep-mcp` stays where it is typed or stored as a name, not where it names the product.
#[test]
fn the_command_name_is_kept_only_where_it_is_typed() {
    for kept in [
        "run skykeep-mcp setup --state-dir X",
        "usage: skykeep-mcp exchange [--state-dir FOLDER]",
        "~/.local/share/skykeep-mcp",
        "skykeep-mcp: {error}",
        "skykeep-mcp:{dev}:{ino}",
        "the service name `skykeep-mcp`",
        "The command is skykeep-mcp.",
        "iCloud-MCP (the skykeep-mcp command)",
        "skykeep-mcp --help",
        "skykeep-mcp-memory",
    ] {
        assert!(!old_name_in(kept), "{kept}");
    }
    for product in [
        "The protected file was written by a newer skykeep-mcp: update skykeep-mcp.",
        "which skykeep-mcp can't use yet",
        "skykeep-mcp is a local server",
    ] {
        assert!(old_name_in(product), "{product}");
    }
}
