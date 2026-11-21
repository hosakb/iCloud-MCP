//! Per-tool permissions from `permissions.yaml` in the private folder.
//!
//! The server reads the file before every tool call, so an edit counts at once. A missing,
//! unreadable or invalid file is ignored and the folder's approval mode decides. Every load
//! that changes the rules is recorded in the library, so `status` and `permissions show` can
//! say what became looser. No MCP tool reads or writes the file or these records.
use quince_core::Library;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

pub const FILE_NAME: &str = "permissions.yaml";
/// Append-only lines for deletions that ran without asking because the file allowed them.
pub const LOG_NAME: &str = "permissions-log.jsonl";
const MAX_FILE_BYTES: u64 = 65536;
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;
const LOADED_KEY: &str = "mcp.permissions.loaded";
const HISTORY_KEY: &str = "mcp.permissions.history";
const SEEN_KEY: &str = "mcp.permissions.seen";
const MAX_HISTORY: usize = 50;
/// Tools that only report Skykeep's own state; they are never gated.
pub(crate) const ALWAYS: [&str; 2] = ["auth_status", "sync_status"];

/// What a rule makes Skykeep do, loosest first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Effect {
    Allow,
    AskClient,
    AskTerminal,
    Deny,
}

impl Effect {
    pub fn as_str(self) -> &'static str {
        match self {
            Effect::Allow => "allow",
            Effect::AskClient => "ask: client",
            Effect::AskTerminal => "ask: terminal",
            Effect::Deny => "deny",
        }
    }
    fn from_str(text: &str) -> Option<Self> {
        Some(match text {
            "allow" => Effect::Allow,
            "ask: client" => Effect::AskClient,
            "ask: terminal" => Effect::AskTerminal,
            "deny" => Effect::Deny,
            _ => return None,
        })
    }
    /// What happens, in words for the person.
    pub fn sentence(self) -> &'static str {
        match self {
            Effect::Allow => "runs without asking",
            Effect::AskClient => "asks in your MCP client",
            Effect::AskTerminal => "waits for skykeep-mcp approve in your terminal",
            Effect::Deny => "is always refused",
        }
    }
}

pub type Rules = BTreeMap<String, Effect>;

/// One name a rule can use.
pub struct Target {
    pub name: String,
    pub tool: String,
    pub action: Option<String>,
}

/// Every valid name, from the tool list itself: each tool, and `tool.action` for each value
/// of a tool's `action` enum. A tool added to the list is covered without changes here.
pub fn targets() -> Vec<Target> {
    let mut out = Vec::new();
    for tool in crate::tools() {
        let Some(name) = tool["name"].as_str() else {
            continue;
        };
        if ALWAYS.contains(&name) {
            continue;
        }
        out.push(Target {
            name: name.into(),
            tool: name.into(),
            action: None,
        });
        if let Some(actions) = tool["inputSchema"]["properties"]["action"]["enum"].as_array() {
            for action in actions.iter().filter_map(Value::as_str) {
                out.push(Target {
                    name: format!("{name}.{action}"),
                    tool: name.into(),
                    action: Some(action.into()),
                });
            }
        }
    }
    out
}

pub fn names() -> Vec<String> {
    targets().into_iter().map(|t| t.name).collect()
}

/// A tool whose calls change things, so `skykeep-mcp approve` can run them.
fn changes(tool: &str) -> bool {
    targets()
        .iter()
        .any(|t| t.tool == tool && t.action.is_some())
}

/// How much a call removes. The one place Skykeep decides this, for every tool.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Removal {
    None,
    /// Moves something where it can be restored, such as Recently Deleted.
    Recoverable,
    /// Gone for good, or not known to be restorable.
    Permanent,
}

/// What an action removes, by its name alone. `trash` can be restored; `delete_forever`,
/// `purge` and any other delete are treated as permanent, except a Notes delete, which Apple
/// moves to Recently Deleted.
pub fn removal_of(tool: &str, action: &str) -> Removal {
    if ["forever", "permanent", "purge"]
        .iter()
        .any(|w| action.contains(w))
    {
        Removal::Permanent
    } else if action.contains("trash") || (tool == "notes_write" && action == "delete") {
        Removal::Recoverable
    } else if action.contains("delete") {
        Removal::Permanent
    } else {
        Removal::None
    }
}

/// What one call removes, with its arguments: a Notes save that empties the body erases it,
/// and a Notes move into Recently Deleted is a deletion.
pub fn removal(tool: &str, args: &Value) -> Removal {
    let Some(action) = args["action"].as_str() else {
        return Removal::None;
    };
    if tool == "notes_write" {
        if action == "save" && args["body"].as_str().is_some_and(|b| b.trim().is_empty()) {
            return Removal::Permanent;
        }
        if action == "move"
            && args["folder_guid"].as_str() == Some(quince_core::notes::TRASH_FOLDER)
        {
            return Removal::Recoverable;
        }
    }
    removal_of(tool, action)
}

/// An action that removes something.
pub fn is_deletion(tool: &str, action: &str) -> bool {
    removal_of(tool, action) != Removal::None
}

/// The name a rule for this call uses: `tool.action` for a tool with actions, else `tool`.
pub fn name_for(tool: &str, args: &Value) -> Option<String> {
    let all = targets();
    if let Some(action) = args["action"].as_str() {
        let name = format!("{tool}.{action}");
        if all.iter().any(|t| t.name == name) {
            return Some(name);
        }
        return None;
    }
    all.iter()
        .any(|t| t.name == tool && !all.iter().any(|o| o.tool == tool && o.action.is_some()))
        .then(|| tool.to_string())
}

/// Text from the file as it may be shown: printable ASCII only, bounded.
fn shown(text: &str) -> String {
    let mut out: String = text
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(80)
        .collect();
    if text.chars().count() > 80 {
        out.push_str("...");
    }
    out
}

fn unquote(text: &str) -> &str {
    let t = text.trim();
    for q in ['"', '\''] {
        if t.len() >= 2 && t.starts_with(q) && t.ends_with(q) {
            let inner = &t[1..t.len() - 1];
            if !inner.contains(['"', '\'', '\\']) {
                return inner;
            }
        }
    }
    t
}

/// The part of a line before a comment. Valid content never holds `#`.
fn uncommented(line: &str) -> &str {
    let bytes = line.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'#' && (i == 0 || bytes[i - 1] == b' ') {
            return &line[..i];
        }
    }
    line
}

fn ask_value(text: &str) -> Option<Effect> {
    match unquote(text) {
        "client" => Some(Effect::AskClient),
        "terminal" => Some(Effect::AskTerminal),
        _ => None,
    }
}

/// `allow`, `deny`, or the flow form `{ask: client}` / `{ask: terminal}`.
fn inline_value(text: &str) -> Option<Effect> {
    let text = unquote(text);
    match text {
        "allow" => return Some(Effect::Allow),
        "deny" => return Some(Effect::Deny),
        _ => {}
    }
    let inner = text.strip_prefix('{')?.strip_suffix('}')?;
    let (key, value) = inner.split_once(':')?;
    if unquote(key) != "ask" {
        return None;
    }
    ask_value(value)
}

const VALUE_HELP: &str = "use allow, deny, {ask: client} or {ask: terminal}";

/// The rules in a permissions file. A strict YAML subset: `version: 1` and a `rules:` map of
/// names to `allow`, `deny` or an `ask:` map, with spaces for indentation.
pub fn parse(text: &str) -> Result<Rules, String> {
    let mut rules = Rules::new();
    let mut lines_of: BTreeMap<String, usize> = BTreeMap::new();
    let mut in_rules = false;
    let mut indent: Option<usize> = None;
    // A name written as `name:` whose `ask:` line comes next.
    let mut open: Option<(String, usize, usize)> = None;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (n, raw) in text.lines().enumerate() {
        let line_no = n + 1;
        let at = |what: String| format!("line {line_no}: {what}");
        if raw.contains('\t') {
            return Err(at("tabs are not allowed; indent with spaces".into()));
        }
        let line = uncommented(raw).trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let depth = line.len() - line.trim_start().len();
        let content = line.trim_start();
        if let Some((name, name_depth, name_line)) = open.take() {
            if depth > name_depth {
                let (key, value) = content.split_once(':').ok_or_else(|| {
                    at(format!(
                        "expected ask: client or ask: terminal under {}",
                        shown(&name)
                    ))
                })?;
                if unquote(key) != "ask" {
                    return Err(at(format!(
                        "under {} only ask: client or ask: terminal can stand",
                        shown(&name)
                    )));
                }
                let effect =
                    ask_value(value).ok_or_else(|| at("ask takes client or terminal".into()))?;
                rules.insert(name.clone(), effect);
                lines_of.insert(name, line_no);
                continue;
            }
            return Err(format!(
                "line {name_line}: {} has no value; {VALUE_HELP}",
                shown(&name)
            ));
        }
        if depth == 0 {
            in_rules = false;
            let (key, value) = content
                .split_once(':')
                .ok_or_else(|| at("expected version: 1 or rules:".into()))?;
            match unquote(key) {
                "version" => {
                    if unquote(value) != "1" {
                        return Err(at("only version: 1 exists".into()));
                    }
                }
                "rules" => match value.trim() {
                    "" => in_rules = true,
                    "{}" => {}
                    _ => return Err(at("put each rule on its own line under rules:".into())),
                },
                other => {
                    return Err(at(format!(
                        "unknown setting {}; the file has version: and rules:",
                        shown(other)
                    )))
                }
            }
            continue;
        }
        if !in_rules {
            return Err(at("indented line outside rules:".into()));
        }
        match indent {
            None => indent = Some(depth),
            Some(d) if d != depth => {
                return Err(at("rules must all have the same indentation".into()))
            }
            _ => {}
        }
        let (key, value) = content
            .split_once(':')
            .ok_or_else(|| at("expected name: value".into()))?;
        let name = unquote(key).to_string();
        if lines_of.contains_key(&name) || open.as_ref().is_some_and(|o| o.0 == name) {
            return Err(at(format!("{} is listed twice", shown(&name))));
        }
        let value = value.trim();
        if value.is_empty() {
            open = Some((name, depth, line_no));
            continue;
        }
        let effect = inline_value(value).ok_or_else(|| {
            if value.starts_with("ask:") {
                at(format!(
                    "write {}: {{{}}}, or put ask: on the next line, indented",
                    shown(&name),
                    shown(value)
                ))
            } else {
                at(format!("{} is not a choice; {VALUE_HELP}", shown(value)))
            }
        })?;
        rules.insert(name.clone(), effect);
        lines_of.insert(name, line_no);
    }
    if let Some((name, _, line)) = open {
        return Err(format!(
            "line {line}: {} has no value; {VALUE_HELP}",
            shown(&name)
        ));
    }
    let valid = targets();
    for (name, effect) in &rules {
        let line = lines_of.get(name).copied().unwrap_or(0);
        let Some(target) = valid.iter().find(|t| &t.name == name) else {
            return Err(format!(
                "line {line}: {} is not a Skykeep tool or action. Valid names: {}",
                shown(name),
                names().join(", ")
            ));
        };
        if *effect == Effect::AskTerminal && !changes(&target.tool) {
            return Err(format!(
                "line {line}: {} can't use ask: terminal, because skykeep-mcp approve only runs changes; use allow, ask: client or deny",
                shown(name)
            ));
        }
    }
    Ok(rules)
}

/// The rule for one call: `tool.action` first, then the tool's own entry.
pub fn rule_for(rules: &Rules, tool: &str, action: Option<&str>) -> Option<Effect> {
    action
        .and_then(|a| rules.get(&format!("{tool}.{a}")))
        .or_else(|| rules.get(tool))
        .copied()
}

/// What the folder's approval mode does with a call that no rule names. `mode` is `None` for
/// a folder without an approval mode, where `require_all` is its earlier require-approval.
pub fn default_effect(
    mode: Option<&str>,
    require_all: bool,
    tool: &str,
    action: Option<&str>,
) -> Effect {
    match mode {
        Some(mode) => match crate::needs_consent(mode, tool) {
            Ok(true) => Effect::AskClient,
            _ => Effect::Allow,
        },
        None if action.is_some_and(|a| is_deletion(tool, a))
            || (action.is_some() && require_all) =>
        {
            Effect::AskTerminal
        }
        None => Effect::Allow,
    }
}

/// The file as the server reads it.
pub enum Source {
    Missing,
    Rules(Rules),
    Invalid(String),
}

/// The file's bytes from `dir`, `None` when there is none: a regular file owned by this
/// user, never through a link, no one else able to change it, at most 64 KiB.
fn read_bytes(dir: &Path) -> Result<Option<Vec<u8>>, &'static str> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(dir.join(FILE_NAME));
    let file = match file {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err("the file is a link; Skykeep reads only a real file in its folder")
        }
        Err(_) => return Err("the file can't be opened"),
    };
    let meta = file.metadata().map_err(|_| "the file can't be read")?;
    if !meta.is_file() {
        return Err("it is not a regular file");
    }
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err("the file belongs to another user");
    }
    if meta.mode() & 0o022 != 0 {
        return Err("other users may change the file; run chmod 600 on it");
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err("the file is larger than 64 KiB");
    }
    let mut bytes = Vec::new();
    if file
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_FILE_BYTES
    {
        return Err("the file can't be read");
    }
    Ok(Some(bytes))
}

/// Reads and checks the file in `dir`, with a fingerprint of what was read.
pub fn read(dir: &Path) -> (Source, String) {
    let bytes = match read_bytes(dir) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return (Source::Missing, "missing".into()),
        Err(problem) => return invalid(problem),
    };
    let print = fingerprint(&bytes);
    let Ok(text) = String::from_utf8(bytes) else {
        return (
            Source::Invalid("the file is not UTF-8 text".into()),
            format!("invalid:{print}"),
        );
    };
    match parse(&text) {
        Ok(rules) => (Source::Rules(rules), print),
        Err(problem) => (Source::Invalid(problem), format!("invalid:{print}")),
    }
}

fn invalid(problem: &str) -> (Source, String) {
    (
        Source::Invalid(problem.into()),
        format!("invalid:{problem}"),
    )
}

/// SHA-256 of the bytes, through the one hash helper the crates already share.
fn fingerprint(bytes: &[u8]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    quince_sync::queue::account_tag(&format!("skykeep-permissions:{hex}"))
}

/// The rules in force for one call.
pub struct Active {
    /// Empty when the file is missing or invalid.
    pub rules: Rules,
    /// Changes whenever the file's bytes change; binds approvals to the rules they saw.
    pub fingerprint: String,
    pub problem: Option<String>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn rules_value(rules: &Rules) -> Value {
    Value::Object(
        rules
            .iter()
            .map(|(k, v)| (k.clone(), json!(v.as_str())))
            .collect(),
    )
}

fn rules_from(value: &Value) -> Rules {
    value
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), Effect::from_str(v.as_str()?)?)))
                .collect()
        })
        .unwrap_or_default()
}

/// How each name's behaviour differs between two rule sets under the same approval mode.
fn diff(
    old: &Rules,
    new: &Rules,
    mode: Option<&str>,
    require_all: bool,
) -> (Vec<Value>, Vec<Value>) {
    let mut looser = Vec::new();
    let mut stricter = Vec::new();
    let all = targets();
    for t in &all {
        // A tool with actions is compared action by action.
        if t.action.is_none() && all.iter().any(|o| o.tool == t.tool && o.action.is_some()) {
            continue;
        }
        let action = t.action.as_deref();
        let default = default_effect(mode, require_all, &t.tool, action);
        let before = rule_for(old, &t.tool, action).unwrap_or(default);
        let after = rule_for(new, &t.tool, action).unwrap_or(default);
        let change = json!({"name":t.name,"from":before.as_str(),"to":after.as_str()});
        if after < before {
            looser.push(change);
        } else if after > before {
            stricter.push(change);
        }
    }
    (looser, stricter)
}

fn require_all(library: &Library) -> bool {
    library
        .state("mcp.require_write_approval")
        .ok()
        .flatten()
        .as_deref()
        == Some("true")
}

/// Reads the file and, when its bytes changed since the last load, records the load and what
/// it made looser or stricter. Fails only when the library can't record it.
pub fn load(library: &mut Library, dir: &Path, mode: Option<&str>) -> Result<Active, &'static str> {
    let (source, print) = read(dir);
    let (rules, problem) = match source {
        Source::Missing => (Rules::new(), None),
        Source::Rules(rules) => (rules, None),
        Source::Invalid(problem) => (Rules::new(), Some(problem)),
    };
    let stored: Value = library
        .state(LOADED_KEY)
        .map_err(|_| "library_unavailable")?
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(Value::Null);
    if stored["fingerprint"].as_str() != Some(print.as_str()) {
        let old = rules_from(&stored["rules"]);
        let (looser, stricter) = diff(&old, &rules, mode, require_all(library));
        let seq = stored["seq"].as_u64().unwrap_or(0);
        let at = now();
        let record = json!({"fingerprint":print,"rules":rules_value(&rules),"problem":problem,
            "loaded_at":at,"seq":seq + 1});
        let mut history: Vec<Value> = library
            .state(HISTORY_KEY)
            .map_err(|_| "library_unavailable")?
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        history.push(
            json!({"seq":seq + 1,"at":at,"looser":looser,"stricter":stricter,
            "problem":problem,"missing":print == "missing"}),
        );
        let keep = history.len().saturating_sub(MAX_HISTORY);
        history.drain(..keep);
        library
            .atomically(|library| -> quince_core::Result<()> {
                library.set_state(HISTORY_KEY, &Value::Array(history).to_string())?;
                library.set_state(LOADED_KEY, &record.to_string())
            })
            .map_err(|_| "library_unavailable")?;
    }
    Ok(Active {
        rules,
        fingerprint: print,
        problem,
    })
}

/// Records that a deletion ran without asking because the file allowed it: tool, action and
/// time, never content. Refuses when the log can't be written, so nothing goes unrecorded.
pub fn log_allowed(dir: &Path, tool: &str, action: &str) -> Result<(), &'static str> {
    append_log(
        dir,
        json!({"at":now(),"tool":tool,"action":action,"rule":"allow"}),
    )
}

/// The newest `limit` log lines, oldest first.
fn log_lines(dir: &Path, limit: usize) -> Vec<Value> {
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join(LOG_NAME))
    else {
        return Vec::new();
    };
    use std::io::{BufRead, Read};
    let lines: Vec<Value> = std::io::BufReader::new(file.take(MAX_LOG_BYTES + 4096))
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect();
    lines[lines.len().saturating_sub(limit)..].to_vec()
}

/// A Unix time as `2026-11-21 14:05 UTC`.
pub fn when(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rest = secs % 86400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rest / 3600,
        rest % 3600 / 60
    )
}

fn history(library: &Library) -> Vec<Value> {
    library
        .state(HISTORY_KEY)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn seen(library: &Library) -> u64 {
    library
        .state(SEEN_KEY)
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Loads recorded since `permissions show` last ran in the person's own terminal.
pub fn unseen(library: &Library) -> Vec<Value> {
    let seen = seen(library);
    history(library)
        .into_iter()
        .filter(|e| e["seq"].as_u64().is_some_and(|s| s > seen))
        .collect()
}

fn change_line(c: &Value) -> String {
    let effect = |key: &str| {
        Effect::from_str(c[key].as_str().unwrap_or_default())
            .map(Effect::sentence)
            .unwrap_or("unknown")
    };
    format!(
        "{}: {} before, now {}",
        c["name"].as_str().unwrap_or_default(),
        effect("from"),
        effect("to")
    )
}

/// Status lines for the permissions file. Loads it the way the server does.
pub fn status(
    library: &mut Library,
    dir: &Path,
    mode: Option<&str>,
) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let active = match load(library, dir, mode) {
        Ok(active) => active,
        Err(_) => {
            out.push((
                "Skykeep couldn't record the permissions file it read.".into(),
                Some("Run skykeep-mcp status again in a moment.".into()),
            ));
            return out;
        }
    };
    match (&active.problem, active.fingerprint.as_str()) {
        (Some(problem), _) => out.push((
            format!("The permissions file can't be used ({}), so Skykeep ignores all of it and follows the approval mode.", shown_problem(problem)),
            Some("Fix the file, then run skykeep-mcp permissions show.".into()),
        )),
        (None, "missing") => out.push((
            "Permissions file: none. The approval mode decides everything.".into(),
            None,
        )),
        (None, _) => out.push((
            format!(
                "Permissions file: {} rules in use. Everything else follows the approval mode.",
                active.rules.len()
            ),
            None,
        )),
    }
    let looser: Vec<Value> = unseen(library)
        .iter()
        .flat_map(|e| e["looser"].as_array().cloned().unwrap_or_default())
        .collect();
    if !looser.is_empty() {
        let first = change_line(&looser[0]);
        let more = if looser.len() > 1 {
            format!(" and {} more", looser.len() - 1)
        } else {
            String::new()
        };
        out.push((
            format!("The permissions changed since you last looked and some actions became looser: {first}{more}."),
            Some("Run skykeep-mcp permissions show to see every change.".into()),
        ));
    }
    out
}

fn shown_problem(problem: &str) -> String {
    problem
        .chars()
        .filter(|c| !c.is_control())
        .take(300)
        .collect()
}

/// `skykeep-mcp permissions show`. With `mark_seen`, the changes shown count as seen.
pub fn show(dir: &Path, mark_seen: bool) -> Result<String, &'static str> {
    let held = crate::state::PrivateState::open(dir)?;
    let dir = held.path.as_path();
    let mut library = crate::setup(dir)?;
    let (mode, mode_text) = match crate::policy(&library, dir) {
        Ok(Some((p, _))) => (
            Some(p.mode.clone()),
            crate::status::describe_mode(&p.mode).to_string(),
        ),
        Ok(None) => (
            None,
            format!(
                "no approval mode. {}",
                crate::status::no_mode_effect(&library)
            ),
        ),
        Err(_) => (None, "the approval mode can't be read".into()),
    };
    let active = load(&mut library, dir, mode.as_deref())?;
    let loaded: Value = library
        .state(LOADED_KEY)
        .map_err(|_| "library_unavailable")?
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(Value::Null);
    let mut out = format!("Permissions file: {}\n", dir.join(FILE_NAME).display());
    out.push_str("Skykeep reads it before every tool call, so a change counts at once.\n");
    if let Some(at) = loaded["loaded_at"].as_u64() {
        out.push_str(&format!("Last change loaded: {}\n", when(at)));
    }
    match (&active.problem, active.fingerprint.as_str()) {
        (Some(problem), _) => out.push_str(&format!(
            "\nThe file can't be used: {}\nSkykeep ignores the whole file and follows the approval mode until it is fixed.\n",
            shown_problem(problem)
        )),
        (None, "missing") => out.push_str("\nThere is no permissions file. The approval mode decides everything.\n"),
        (None, _) if active.rules.is_empty() => out.push_str("\nThe file lists no rules.\n"),
        (None, _) => {
            out.push_str("\nRules in use:\n");
            let cards = log_lines(dir, usize::MAX);
            for (name, effect) in &active.rules {
                let card = cards
                    .iter()
                    .rev()
                    .find(|l| l["source"] == "card" && l["name"].as_str() == Some(name));
                let from = match (effect, card) {
                    (Effect::Allow, Some(l)) => format!(
                        ", always allowed from a card on {}",
                        when(l["at"].as_u64().unwrap_or(0))
                    ),
                    _ => String::new(),
                };
                out.push_str(&format!("  {name}: {}, {}{from}\n", effect.as_str(), effect.sentence()));
            }
        }
    }
    out.push_str(&format!(
        "Everything not listed follows the approval mode: {mode_text}\n"
    ));
    let unseen = unseen(&library);
    if unseen.is_empty() {
        out.push_str("\nNo changes since you last looked.\n");
    } else {
        out.push_str("\nChanges since you last looked:\n");
        for event in &unseen {
            let at = when(event["at"].as_u64().unwrap_or(0));
            let looser = event["looser"].as_array().cloned().unwrap_or_default();
            let stricter = event["stricter"].as_array().cloned().unwrap_or_default();
            if let Some(problem) = event["problem"].as_str() {
                out.push_str(&format!(
                    "  {at}: the file became unusable ({}), so the approval mode decides.\n",
                    shown_problem(problem)
                ));
            } else if event["missing"] == true {
                out.push_str(&format!("  {at}: the file was removed.\n"));
            } else if looser.is_empty() && stricter.is_empty() {
                out.push_str(&format!(
                    "  {at}: the file changed; no action behaves differently.\n"
                ));
            }
            for c in &looser {
                out.push_str(&format!("  {at}: looser: {}\n", change_line(c)));
            }
            for c in &stricter {
                out.push_str(&format!("  {at}: stricter: {}\n", change_line(c)));
            }
        }
    }
    let log = log_lines(dir, 20);
    if log.is_empty() {
        out.push_str("\nNo deletion has run without asking because of this file, and no card has saved a rule.\n");
    } else {
        out.push_str(&format!("\nDeletions that ran without asking, and rules saved from a card, newest last (full list in {}):\n", dir.join(LOG_NAME).display()));
        for line in &log {
            let at = when(line["at"].as_u64().unwrap_or(0));
            if line["source"] == "card" {
                out.push_str(&format!(
                    "  {at} always allowed from a card: {}\n",
                    shown(line["name"].as_str().unwrap_or_default())
                ));
            } else {
                out.push_str(&format!(
                    "  {at} ran without asking: {}.{}\n",
                    shown(line["tool"].as_str().unwrap_or_default()),
                    shown(line["action"].as_str().unwrap_or_default())
                ));
            }
        }
    }
    if mark_seen {
        let last = history(&library)
            .last()
            .and_then(|e| e["seq"].as_u64())
            .unwrap_or(0);
        library
            .set_state(SEEN_KEY, &last.to_string())
            .map_err(|_| "library_unavailable")?;
    }
    Ok(out)
}

/// A first file for `dir`'s approval mode: deletions ask, the way the mode already asks or
/// more strictly, and every valid name is listed as a comment.
pub fn starter(mode: Option<&str>) -> String {
    let ask = if mode.is_some() { "client" } else { "terminal" };
    let mut out = String::from(
        "# Skykeep permissions\n\
         #\n\
         # This file decides, for each tool and action, what Skykeep does:\n\
         #   allow           run without asking\n\
         #   ask: client     ask in your MCP client, with approve and deny buttons\n\
         #   ask: terminal   wait until you run skykeep-mcp approve in your own terminal\n\
         #   deny            always refuse\n\
         #\n\
         # Skykeep reads this file before every tool call, so a change counts at once.\n\
         # Anything not listed follows the folder's approval mode.\n\
         # If the file has a mistake, Skykeep ignores all of it and follows the approval mode.\n\
         # To check it, run: skykeep-mcp permissions show\n\
         #\n\
         # Write ask on its own indented line, as below, or as {ask: terminal}.\n\
         version: 1\n\
         rules:\n",
    );
    for t in targets() {
        if t.action.as_deref().is_some_and(|a| is_deletion(&t.tool, a)) {
            out.push_str(&format!("  {}:\n    ask: {ask}\n", t.name));
        }
    }
    out.push_str("  # reminders_write.create: allow\n#\n# Names you can use:\n");
    for name in names() {
        out.push_str(&format!("#   {name}\n"));
    }
    out
}

/// Writes the starter file into `dir`, never over an existing one.
pub fn write_starter(dir: &Path, mode: Option<&str>) -> Result<(), &'static str> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join(FILE_NAME))
        .map_err(|_| "permissions_file_unavailable")?;
    file.write_all(starter(mode).as_bytes())
        .map_err(|_| "permissions_file_unavailable")
}

/// `text` with `name: allow` set: an existing entry for `name` is rewritten in place, keeping
/// its comment; otherwise the entry goes after the last rule. Everything else stays as it was.
fn with_allow(text: &str, name: &str) -> Option<String> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let content = |l: &str| uncommented(l).trim_end().to_string();
    let depth = |l: &str| l.len() - l.trim_start().len();
    let rules_at = lines.iter().position(|l| {
        let c = content(l);
        depth(&c) == 0
            && c.split_once(':')
                .is_some_and(|(k, _)| unquote(k) == "rules")
    });
    let Some(rules_at) = rules_at else {
        lines.push("rules:".into());
        lines.push(format!("  {name}: allow"));
        return Some(lines.join("\n") + "\n");
    };
    if content(&lines[rules_at])
        .split_once(':')
        .map(|(_, v)| v.trim())
        == Some("{}")
    {
        lines[rules_at] = "rules:".into();
        lines.insert(rules_at + 1, format!("  {name}: allow"));
        return Some(lines.join("\n") + "\n");
    }
    let mut end = lines.len();
    for (i, l) in lines.iter().enumerate().skip(rules_at + 1) {
        let c = content(l);
        if !c.trim().is_empty() && depth(&c) == 0 {
            end = i;
            break;
        }
    }
    let mut indent = None;
    let mut last = rules_at;
    let mut i = rules_at + 1;
    while i < end {
        let c = content(&lines[i]);
        if c.trim().is_empty() {
            i += 1;
            continue;
        }
        let d = depth(&c);
        let rule_indent = *indent.get_or_insert(d);
        if d == rule_indent {
            last = i;
            let (key, value) = c.trim_start().split_once(':')?;
            if unquote(key) == name {
                let comment = lines[i][c.len()..].to_string();
                lines[i] = format!("{}{}: allow{}", &c[..d], key, comment);
                if value.trim().is_empty() {
                    // The nested `ask:` line goes with the old value.
                    let mut j = i + 1;
                    while j < end {
                        let n = content(&lines[j]);
                        if n.trim().is_empty() {
                            j += 1;
                            continue;
                        }
                        if depth(&n) > d {
                            lines.remove(j);
                        }
                        break;
                    }
                }
                return Some(lines.join("\n") + "\n");
            }
        } else {
            last = i;
        }
        i += 1;
    }
    let indent = " ".repeat(indent.unwrap_or(2));
    lines.insert(last + 1, format!("{indent}{name}: allow"));
    Some(lines.join("\n") + "\n")
}

/// Writes `bytes` to `name` in `dir` through a private temporary file and a rename, so a
/// reader sees the old file or the new one, never half of either.
fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), &'static str> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = dir.join(format!(".{name}.tmp"));
    // A leftover from a crash, or something planted: removed, never followed.
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&tmp)
        .map_err(|_| "the file can't be written")?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if written.is_err() || std::fs::rename(&tmp, dir.join(name)).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Err("the file can't be written");
    }
    if let Ok(folder) = std::fs::File::open(dir) {
        let _ = folder.sync_all();
    }
    Ok(())
}

/// The backup of the file as it was before Skykeep last changed it.
pub const BACKUP_NAME: &str = "permissions.yaml.bak";

/// Saves `name: allow` because the person chose "Always allow" on a card. `expected` is the
/// fingerprint the card was shown under: a file changed since then is left alone. The old
/// file is kept as `permissions.yaml.bak`. Returns why the rule was not saved.
pub fn save_allow(dir: &Path, name: &str, expected: &str) -> Result<(), String> {
    if !names().iter().any(|n| n == name) {
        return Err("this action has no rule name".into());
    }
    let old = read_bytes(dir).map_err(|p| format!("the file can't be used: {p}"))?;
    let print = old
        .as_deref()
        .map_or_else(|| "missing".to_string(), fingerprint);
    let text = match &old {
        None => None,
        Some(bytes) => Some(
            String::from_utf8(bytes.clone())
                .map_err(|_| "the file has a mistake: it is not UTF-8 text".to_string())?,
        ),
    };
    let parsed = text.as_deref().map(parse).transpose();
    // The fingerprint `read` gives for these bytes, which the card was bound to.
    let current = match (&old, &parsed) {
        (Some(_), Err(_)) => format!("invalid:{print}"),
        _ => print.clone(),
    };
    if current != expected {
        return Err("the file changed while the card was open".into());
    }
    if let Err(problem) = parsed {
        return Err(format!(
            "the file has a mistake: {}",
            shown_problem(&problem)
        ));
    }
    let text = text.unwrap_or_else(|| String::from("version: 1\nrules:\n"));
    let mut want = parse(&text)?;
    want.insert(name.to_string(), Effect::Allow);
    let new = with_allow(&text, name).ok_or("the file's layout can't be edited safely")?;
    if parse(&new).ok().as_ref() != Some(&want) || new.len() as u64 > MAX_FILE_BYTES {
        return Err("the file's layout can't be edited safely".into());
    }
    if let Some(bytes) = &old {
        write_atomic(dir, BACKUP_NAME, bytes)?;
    }
    // Last check right before the rename: an edit that landed meanwhile wins.
    let now_print = read_bytes(dir)
        .map_err(String::from)?
        .as_deref()
        .map_or_else(|| "missing".to_string(), fingerprint);
    if now_print != print {
        return Err("the file changed while the card was open".into());
    }
    write_atomic(dir, FILE_NAME, new.as_bytes())?;
    let _ = append_log(
        dir,
        json!({"at":now(),"name":name,"rule":"allow","source":"card"}),
    );
    Ok(())
}

fn append_log(dir: &Path, entry: Value) -> Result<(), &'static str> {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join(LOG_NAME))
        .map_err(|_| "permissions_log_unavailable")?;
    let meta = file.metadata().map_err(|_| "permissions_log_unavailable")?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err("permissions_log_unavailable");
    }
    if meta.len() > MAX_LOG_BYTES {
        return Err("permissions_log_full");
    }
    let mut line = entry.to_string();
    line.push('\n');
    file.write_all(line.as_bytes())
        .map_err(|_| "permissions_log_unavailable")
}
