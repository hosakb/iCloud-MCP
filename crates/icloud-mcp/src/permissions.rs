//! Per-tool permissions from `permissions.yaml` in the private folder, and three settings
//! beside them: the approval mode (`approval_mode:`), the exchange folder
//! (`exchange_folder:`) and the services the server may use (`services:`). A setting in the file wins over the one stored by setup or the older
//! terminal commands; `icloud-mcp policy` and `icloud-mcp exchange` edit the file's line.
//!
//! The server reads the file before every tool call, so an edit counts at once. A missing
//! file leaves everything to the folder's approval mode. An unusable file never loosens
//! anything: the last valid rules and approval mode stay in use, and with none every deletion
//! asks. No exchange folder is used while the file can't be: uploads and downloads are
//! refused. A usable file without `exchange_folder:` names no exchange folder.
//! Every load that changes them is recorded in the library, so `status` and `permissions
//! show` can say what became looser. No MCP tool reads or writes the file or these records;
//! the server writes one rule only when the person picks "Always allow" on an approval card.
//! An agent that can write files can still edit it: the looser changes it makes are listed
//! until the person runs `permissions show` in their own terminal.
use quince_core::Library;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

pub const FILE_NAME: &str = "permissions.yaml";
/// The file's line for the approval mode.
pub const MODE_KEY: &str = "approval_mode";
/// The file's line for the exchange folder.
pub const EXCHANGE_KEY: &str = "exchange_folder";
/// The file's line for the services the server may use.
pub const SERVICES_KEY: &str = "services";
/// Every service a `services:` line can name, in the order the file and the texts use, with
/// its name for people.
pub const SERVICES: [(&str, &str); 6] = [
    ("notes", "Notes"),
    ("reminders", "Reminders"),
    ("contacts", "Contacts"),
    ("calendar", "Calendar"),
    ("photos", "Photos"),
    ("drive", "Drive"),
];
/// The approval modes the file accepts, strictest first.
const MODE_NAMES: [&str; 3] = ["reads_and_writes", "writes_only", "yolo"];
/// Append-only lines for rules saved from a card (older folders may also hold lines for
/// deletions that ran because the file allowed them).
pub const LOG_NAME: &str = "permissions-log.jsonl";
const MAX_FILE_BYTES: u64 = 65536;
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;
const LOADED_KEY: &str = "mcp.permissions.loaded";
const HISTORY_KEY: &str = "mcp.permissions.history";
const SEEN_KEY: &str = "mcp.permissions.seen";
/// Looser changes since `permissions show` last ran in the person's terminal, kept apart from
/// the history so that later edits can't push them out.
const LOOSER_KEY: &str = "mcp.permissions.looser_unseen";
const MAX_LOOSER: usize = 20;
const MAX_HISTORY: usize = 50;
/// Tools that only report iCloud-MCP's own state; they are never gated.
pub(crate) const ALWAYS: [&str; 2] = ["auth_status", "sync_status"];

/// What a rule makes iCloud-MCP do, loosest first.
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
            Effect::AskTerminal => "waits for icloud-mcp approve in your terminal",
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
    // Not an action: whether a Calendar change may reach other people's calendars.
    out.push(Target {
        name: crate::calendar_write::INVITATIONS_RULE.into(),
        tool: crate::calendar_write::TOOL.into(),
        action: Some(crate::calendar_write::INVITATIONS.into()),
    });
    out
}

/// Whether the rules in force let a Calendar change send updates or cancellations to other
/// people: only an explicit `calendar_write.send_invitations: allow`. A broken file with no
/// earlier valid rules has no rules, so it refuses.
pub fn invitations_allowed(active: &Active) -> bool {
    active.rules.get(crate::calendar_write::INVITATIONS_RULE) == Some(&Effect::Allow)
}

fn is_invitations(tool: &str, action: Option<&str>) -> bool {
    tool == crate::calendar_write::TOOL && action == Some(crate::calendar_write::INVITATIONS)
}

pub fn names() -> Vec<String> {
    targets().into_iter().map(|t| t.name).collect()
}

/// A tool whose calls change things, so `icloud-mcp approve` can run them.
fn changes(tool: &str) -> bool {
    targets()
        .iter()
        .any(|t| t.tool == tool && t.action.is_some())
}

/// How much a call removes. The one place iCloud-MCP decides this, for every tool.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Removal {
    None,
    /// Moves something where it can be restored, such as Recently Deleted.
    Recoverable,
    /// Gone for good, or not known to be restorable.
    Permanent,
}

/// What an action removes, by the words of its name (split at `_`, so `undelete` is not a
/// deletion). A `trash` word can be restored. `forever`, `permanent`, `purge` and any other
/// `delete` are treated as permanent, except a Notes delete, which Apple moves to Recently
/// Deleted.
pub fn removal_of(tool: &str, action: &str) -> Removal {
    let has = |words: &[&str]| action.split('_').any(|w| words.contains(&w));
    if has(&["forever", "permanent", "purge"]) {
        Removal::Permanent
    } else if has(&["trash"]) || (tool == "notes_write" && action == "delete") {
        Removal::Recoverable
    } else if has(&["delete"]) {
        Removal::Permanent
    } else {
        Removal::None
    }
}

/// The sentence a card shows for "Always allow", in plain words.
pub fn always_sentence(rule: &str) -> String {
    let what = match rule.split_once('.') {
        Some((tool, action)) => {
            let service = tool.split('_').next().unwrap_or(tool);
            let mut chars = service.chars();
            let service: String = chars
                .next()
                .map(|c| c.to_uppercase().chain(chars).collect())
                .unwrap_or_default();
            format!("every {service} {}", action.replace('_', " "))
        }
        None => format!("every {rule} request"),
    };
    format!("Always allow: {what} runs without asking from now on.")
}

/// A permissions show error in plain words.
pub fn plain(code: &'static str) -> &'static str {
    match code {
        "permissions_setup_required" => "This folder isn't set up yet. Run icloud-mcp setup first.",
        "state_busy_retry" => "The server is using the folder right now. Run icloud-mcp permissions show again in a moment.",
        "state_unavailable" => "iCloud-MCP can't open that folder. Check that it exists and belongs to you, or add --state-dir FOLDER.",
        "unsafe_state_contents" | "state_must_be_owned_and_private" | "unsafe_state_path" => "The folder isn't private to you, or holds a link or a shared file. Run icloud-mcp status to see what to fix.",
        "library_unavailable" => "iCloud-MCP couldn't read its records in that folder. Run icloud-mcp status to see why.",
        other => other,
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

/// What a permissions file says: its rules and the settings, as written.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub rules: Rules,
    /// `approval_mode:`, one of reads_and_writes, writes_only and yolo.
    pub mode: Option<String>,
    /// `exchange_folder:` as written: a full path, or one that starts with `~/`.
    pub exchange: Option<String>,
    /// `services:`, in the order of [`SERVICES`]. `None` when the file has no such line: every
    /// service is on.
    pub services: Option<Vec<String>>,
}

/// The service a tool belongs to, by the first word of its name. `sync` refreshes Notes and
/// Reminders and has none of its own; `auth_status` and `sync_status` belong to none.
pub fn service_of(tool: &str) -> Option<&'static str> {
    let first = tool.split('_').next()?;
    SERVICES.iter().map(|(s, _)| *s).find(|s| *s == first)
}

/// The services as people read them: `Notes, Contacts and Drive`.
pub fn service_names(services: &[&str]) -> String {
    let names: Vec<&str> = services
        .iter()
        .filter_map(|s| SERVICES.iter().find(|(k, _)| k == s).map(|(_, n)| *n))
        .collect();
    match names.as_slice() {
        [] => "none".into(),
        [one] => (*one).into(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

const SERVICES_HELP: &str = "write it on one line, such as services: [notes, calendar]";

/// A `services:` value: `[name, ...]` on one line, each name once, kept in [`SERVICES`] order.
fn services_value(text: &str) -> Result<Vec<String>, String> {
    let inner = unquote(text)
        .strip_prefix('[')
        .and_then(|t| t.strip_suffix(']'))
        .ok_or_else(|| format!("{SERVICES_KEY} lists the services that are on; {SERVICES_HELP}"))?;
    let mut named = Vec::new();
    for word in inner.split(',').map(unquote).filter(|w| !w.is_empty()) {
        if !SERVICES.iter().any(|(s, _)| *s == word) {
            return Err(format!(
                "{} is not a service; {SERVICES_KEY} takes notes, reminders, contacts, calendar, photos and drive",
                shown(word)
            ));
        }
        if named.contains(&word) {
            return Err(format!("{} is named twice in {SERVICES_KEY}", shown(word)));
        }
        named.push(word);
    }
    Ok(SERVICES
        .iter()
        .map(|(s, _)| *s)
        .filter(|s| named.contains(s))
        .map(str::to_string)
        .collect())
}

/// What a file that can't be used says about services, when no valid file was loaded before:
/// `None` (every service on) when it has no `services:` line, the line when it reads on its
/// own, and every service off when the line can't be read or the file can't be read at all.
fn services_when_broken(bytes: Option<&[u8]>) -> Option<Vec<String>> {
    let Some(text) = bytes.and_then(|b| std::str::from_utf8(b).ok()) else {
        return Some(Vec::new());
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| {
            uncommented(l)
                .split_once(':')
                .is_some_and(|(k, _)| unquote(&k.replace('\t', " ")) == SERVICES_KEY)
        })
        .collect();
    match lines.as_slice() {
        [] => None,
        [line] if !line.starts_with([' ', '\t']) => Some(
            uncommented(line)
                .split_once(':')
                .and_then(|(_, v)| services_value(v.trim_end()).ok())
                .unwrap_or_default(),
        ),
        _ => Some(Vec::new()),
    }
}

/// The rules in a permissions file (see [`parse_settings`]).
pub fn parse(text: &str) -> Result<Rules, String> {
    parse_settings(text).map(|s| s.rules)
}

/// A top-level name that looks like a secret. Passwords, codes and the passphrase are typed
/// only in the person's own terminal, never kept in this file.
fn secret_like(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("password")
        || key.contains("passphrase")
        || key
            .split(['_', '-'])
            .any(|part| ["passwd", "code", "token", "secret", "pin"].contains(&part))
}

/// The exchange folder as the file may name it: a full path or `~/...`, without `.` or `..`
/// and without control or hiding characters. Whether it is a safe folder is checked when it
/// is used, by [`crate::exchange::checked`].
fn folder_value(text: &str) -> Result<String, String> {
    let value = unquote(text).to_string();
    let full = value.starts_with('/') || value == "~" || value.starts_with("~/");
    if !full {
        return Err(format!(
            "{EXCHANGE_KEY} takes a full path, such as ~/iCloud-MCP-Exchange or /home/you/iCloud-MCP-Exchange"
        ));
    }
    if value
        .chars()
        .any(|c| c.is_control() || crate::exchange::hides_text(c))
    {
        return Err(format!(
            "{EXCHANGE_KEY} holds a control or hidden character; type the path again"
        ));
    }
    if value.len() > crate::exchange::MAX_LOCAL_BYTES {
        return Err(format!("{EXCHANGE_KEY} is longer than 1024 bytes"));
    }
    if value.split('/').any(|n| n == "." || n == "..") {
        return Err(format!(
            "{EXCHANGE_KEY} can't contain . or .. ; give the folder's full path"
        ));
    }
    Ok(value)
}

/// A permissions file. A strict YAML subset: `version: 1`, optionally `approval_mode:` and
/// `exchange_folder:`, and a `rules:` map of names to `allow`, `deny` or an `ask:` map, with
/// spaces for indentation.
pub fn parse_settings(text: &str) -> Result<Settings, String> {
    let mut settings = Settings::default();
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
                MODE_KEY => {
                    if settings.mode.is_some() {
                        return Err(at(format!("{MODE_KEY} is listed twice")));
                    }
                    let mode = unquote(value);
                    if !MODE_NAMES.contains(&mode) {
                        return Err(at(format!(
                            "{MODE_KEY} takes reads_and_writes, writes_only or yolo"
                        )));
                    }
                    settings.mode = Some(mode.to_string());
                }
                EXCHANGE_KEY => {
                    if settings.exchange.is_some() {
                        return Err(at(format!("{EXCHANGE_KEY} is listed twice")));
                    }
                    settings.exchange = Some(folder_value(value).map_err(at)?);
                }
                SERVICES_KEY => {
                    if settings.services.is_some() {
                        return Err(at(format!("{SERVICES_KEY} is listed twice")));
                    }
                    settings.services = Some(services_value(value).map_err(at)?);
                }
                other if secret_like(other) => {
                    return Err(at("passwords, codes and the passphrase never go in this file; type them only in your own terminal, when icloud-mcp setup, login or unlock asks".into()))
                }
                other if names().iter().any(|n| n == other) => {
                    return Err(at(format!(
                        "{} must be indented under rules:, with spaces in front of it",
                        shown(other)
                    )))
                }
                other => {
                    return Err(at(format!(
                        "unknown setting {}; the file has version:, {MODE_KEY}:, {EXCHANGE_KEY}:, {SERVICES_KEY}: and rules:",
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
                "line {line}: {} is not an iCloud-MCP tool or action; {}",
                shown(name),
                hint(name)
            ));
        };
        if is_invitations(&target.tool, target.action.as_deref())
            && !matches!(effect, Effect::Allow | Effect::Deny)
        {
            return Err(format!(
                "line {line}: {} takes allow or deny only",
                shown(name)
            ));
        }
        if *effect == Effect::AskTerminal && !changes(&target.tool) {
            return Err(format!(
                "line {line}: {} can't use ask: terminal, because icloud-mcp approve only runs changes; use allow, ask: client or deny",
                shown(name)
            ));
        }
    }
    settings.rules = rules;
    Ok(settings)
}

/// Single-character edits from `a` to `b`.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().take(200).enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let next = (row[j + 1] + 1)
                .min(row[j] + 1)
                .min(prev + usize::from(ca != *cb));
            prev = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// What to say about a name that is not valid: the tool's removing actions for a typo of a
/// deletion, the nearest action of the same tool, or that the tool does not exist.
fn hint(name: &str) -> String {
    let all = targets();
    let (tool, action) = match name.split_once('.') {
        Some((tool, action)) => (tool, Some(action)),
        None => (name, None),
    };
    let tools: Vec<&str> = all
        .iter()
        .filter(|t| t.action.is_none())
        .map(|t| t.tool.as_str())
        .collect();
    if !tools.contains(&tool) {
        let near = tools
            .iter()
            .copied()
            .min_by_key(|t| distance(tool, t))
            .filter(|t| distance(tool, t) <= 2);
        return match near {
            Some(near) => format!(
                "there is no tool named {}; did you mean {near}?",
                shown(tool)
            ),
            None => format!("there is no tool named {}.", shown(tool)),
        };
    }
    let actions: Vec<&str> = all
        .iter()
        .filter(|t| t.tool == tool)
        .filter_map(|t| t.action.as_deref())
        .collect();
    let Some(action) = action else {
        return format!("{tool} is a tool; this name can't be used.");
    };
    if actions.is_empty() {
        return format!("{tool} has no actions, so name it as {tool} alone.");
    }
    let removing = ["delete", "remove", "trash", "erase", "purge", "destroy"]
        .iter()
        .any(|w| action.to_ascii_lowercase().contains(w));
    if removing {
        let removals: Vec<String> = actions
            .iter()
            .filter(|a| is_deletion(tool, a))
            .map(|a| format!("{tool}.{a}"))
            .collect();
        return match removals.as_slice() {
            [] => format!("{tool} has no action that removes something."),
            [one] => format!("the action of {tool} that removes something is {one}."),
            many => format!(
                "the actions of {tool} that remove something are {}.",
                many.join(" and ")
            ),
        };
    }
    let near = actions
        .iter()
        .min_by_key(|a| distance(action, a))
        .copied()
        .unwrap_or_default();
    format!("the closest valid name is {tool}.{near}.")
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
    if is_invitations(tool, action) {
        return Effect::Deny;
    }
    match mode {
        Some(mode) => match crate::needs_consent(mode, tool) {
            Ok(true) => Effect::AskClient,
            _ => Effect::Allow,
        },
        None if action.is_some_and(|a| is_deletion(tool, a))
            || (action.is_some() && require_all) =>
        {
            Effect::AskClient
        }
        None => Effect::Allow,
    }
}

/// The file as the server reads it.
pub enum Source {
    Missing,
    Rules(Settings),
    /// The problem, and what the file says about services (see [`services_when_broken`]).
    Invalid(String, Option<Vec<String>>),
}

/// The file's bytes from `dir`, `None` when there is none: a regular file owned by this
/// user, never through a link, no one else able to change it, at most 64 KiB.
fn read_bytes(dir: &Path) -> Result<Option<Vec<u8>>, String> {
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
            return Err(
                "the file is a link; iCloud-MCP reads only a real file in its folder".into(),
            )
        }
        Err(_) => return Err("the file can't be opened".into()),
    };
    let meta = file
        .metadata()
        .map_err(|_| "the file can't be read".to_string())?;
    if !meta.is_file() {
        return Err("it is not a regular file".into());
    }
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err("the file belongs to another user".into());
    }
    if meta.mode() & 0o022 != 0 {
        return Err(format!(
            "other users may change the file; run: chmod 600 {}",
            crate::status::quoted(&dir.join(FILE_NAME))
        ));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err("the file is larger than 64 KiB".into());
    }
    let mut bytes = Vec::new();
    if file
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_FILE_BYTES
    {
        return Err("the file can't be read".into());
    }
    Ok(Some(bytes))
}

/// Reads and checks the file in `dir`, with a fingerprint of what was read.
pub fn read(dir: &Path) -> (Source, String) {
    let bytes = match read_bytes(dir) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return (Source::Missing, "missing".into()),
        Err(problem) => return invalid(&problem),
    };
    let print = fingerprint(&bytes);
    let services = services_when_broken(Some(&bytes));
    let Ok(text) = String::from_utf8(bytes) else {
        return (
            Source::Invalid("the file is not UTF-8 text".into(), services),
            format!("invalid:{print}"),
        );
    };
    match parse_settings(&text) {
        Ok(settings) => (Source::Rules(settings), print),
        Err(problem) => (
            Source::Invalid(problem, services),
            format!("invalid:{print}"),
        ),
    }
}

fn invalid(problem: &str) -> (Source, String) {
    (
        Source::Invalid(problem.into(), services_when_broken(None)),
        format!("invalid:{problem}"),
    )
}

/// SHA-256 of the bytes, through the one hash helper the crates already share.
fn fingerprint(bytes: &[u8]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    quince_sync::queue::account_tag(&format!("skykeep-permissions:{hex}"))
}

/// The rules and settings in force for one call.
#[derive(Clone)]
pub struct Active {
    /// The file's rules; while the file is unusable, the last valid rules loaded, if any.
    pub rules: Rules,
    /// The approval mode in force: the file's, else the one setup stored.
    pub mode: Option<String>,
    /// Whether [`Self::mode`] comes from the file (or the last valid file).
    pub mode_in_file: bool,
    /// The file's exchange folder as written. `None` while the file can't be used; with no
    /// file, `None` leaves the folder set up with `icloud-mcp exchange`, if any.
    pub exchange: Option<String>,
    /// A usable file is there, so its `exchange_folder:` line, or the lack of one, is the
    /// only exchange folder: a folder set up before is not used.
    pub file_names_exchange: bool,
    /// Changes whenever the file's bytes change; binds approvals to the rules they saw.
    pub fingerprint: String,
    pub problem: Option<String>,
    /// When the rules in use were loaded, while the file is unusable and older rules stand in.
    pub kept_from: Option<u64>,
    /// The file is unusable and there are no earlier valid rules: every deletion asks.
    pub strict: bool,
    /// The services that are on, in [`SERVICES`] order: the file's `services:` line (or the
    /// last valid file's), every service when there is none.
    pub services: Vec<&'static str>,
    /// Whether the file in use (or the last valid one) has a `services:` line.
    pub services_in_file: bool,
}

impl Active {
    /// Whether `service` is on.
    pub fn service_on(&self, service: &str) -> bool {
        self.services.contains(&service)
    }

    /// Whether a tool may be offered and run: its service is on, and `sync` needs Notes or
    /// Reminders.
    pub fn tool_on(&self, tool: &str) -> bool {
        tool_on(&self.services, tool)
    }

    /// The services that are switched off, in [`SERVICES`] order.
    pub fn services_off(&self) -> Vec<&'static str> {
        SERVICES
            .iter()
            .map(|(s, _)| *s)
            .filter(|s| !self.service_on(s))
            .collect()
    }

    /// One sentence on which services are on.
    pub fn services_sentence(&self) -> String {
        let off = self.services_off();
        if off.is_empty() {
            return "Services on: all six (Notes, Reminders, Contacts, Calendar, Photos and Drive).".into();
        }
        format!(
            "Services on: {}. Switched off: {}.",
            service_names(&self.services),
            service_names(&off)
        )
    }
    /// A sentence on what iCloud-MCP does while the file can't be used, for tool results and
    /// status. `permissions show` lists the valid names itself, so it passes `in_show`.
    pub fn fallback(&self) -> Option<String> {
        self.fallback_in(false)
    }

    fn fallback_in(&self, in_show: bool) -> Option<String> {
        let problem = self.problem.as_deref()?;
        let names = if !in_show && problem.contains("is not an iCloud-MCP tool or action") {
            " To see every valid name, run icloud-mcp permissions show."
        } else {
            ""
        };
        Some(match self.kept_from {
            Some(at) => format!(
                "The permissions file can't be used ({}). iCloud-MCP keeps the last valid rules, loaded {}.{names}",
                shown_problem(problem),
                when(at)
            ),
            None => format!(
                "The permissions file can't be used ({}). iCloud-MCP follows the approval mode and asks before every deletion until the file is fixed.{names}",
                shown_problem(problem)
            ),
        })
    }
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
        // The invitations rule counts only under its own name, as the server reads it.
        let rule = |rules: &Rules| {
            if is_invitations(&t.tool, action) {
                rules.get(&t.name).copied()
            } else {
                rule_for(rules, &t.tool, action)
            }
        };
        let before = rule(old).unwrap_or(default);
        let after = rule(new).unwrap_or(default);
        // Compared as the server really runs them.
        let really = |e: Effect| match action {
            Some(a) => runs_as(&t.tool, a, e).0,
            None => e,
        };
        let (before, after) = (really(before), really(after));
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

/// A load that changes the record: the new record, and the history entry it adds.
type Change = (Value, Option<Value>);

/// How strict an approval mode is, for telling a looser change from a stricter one. A folder
/// without a mode counts as between writes_only and yolo, or as writes_only after
/// `require-approval`.
pub(crate) fn mode_rank(mode: Option<&str>, require_all: bool) -> u8 {
    match mode {
        Some("reads_and_writes") => 4,
        Some("writes_only") => 3,
        None if require_all => 3,
        None => 2,
        _ => 1,
    }
}

/// The exchange folder `icloud-mcp exchange` stored, for comparing with the file's.
fn stored_exchange(library: &Library) -> Option<String> {
    crate::exchange::configured(library)
        .ok()
        .flatten()
        .map(|e| e.path.display().to_string())
}

/// The changes in the three settings: the approval mode, the exchange folder, then each
/// service. A different folder counts as looser, because its files become reachable; no folder
/// counts as stricter. A service switched on counts as looser. `exchange` is the folder in
/// force after the load, as written.
fn setting_changes(
    library: &Library,
    stored: &Value,
    settings: &Settings,
    exchange: &Option<String>,
    mode: Option<&str>,
    require_all: bool,
) -> (Vec<Value>, Vec<Value>) {
    let (mut looser, mut stricter) = (Vec::new(), Vec::new());
    let before = stored[MODE_KEY].as_str().or(mode);
    let after = settings.mode.as_deref().or(mode);
    if before != after {
        let change =
            json!({"name":MODE_KEY,"from":before.unwrap_or("none"),"to":after.unwrap_or("none")});
        if mode_rank(after, require_all) < mode_rank(before, require_all) {
            looser.push(change);
        } else {
            stricter.push(change);
        }
    }
    // The folder set up in the terminal counts only while there is no file.
    let legacy = stored_exchange(library);
    let before = stored[EXCHANGE_KEY]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            (stored.is_null() || stored["fingerprint"] == "missing")
                .then(|| legacy.clone())
                .flatten()
        });
    let after = exchange.clone();
    if before != after {
        let change = json!({"name":EXCHANGE_KEY,"from":before.as_deref().unwrap_or("none"),"to":after.as_deref().unwrap_or("none")});
        if after.is_some() {
            looser.push(change);
        } else {
            stricter.push(change);
        }
    }
    let before = settings_from(stored).services;
    service_changes(
        before.as_deref(),
        settings.services.as_deref(),
        &mut looser,
        &mut stricter,
    );
    (looser, stricter)
}

fn settings_value(settings: &Settings) -> Value {
    json!({"rules":rules_value(&settings.rules),MODE_KEY:settings.mode,EXCHANGE_KEY:settings.exchange,SERVICES_KEY:settings.services})
}

/// The services that are on for a `services:` value (`None`: every service).
fn services_on(services: Option<&[String]>) -> Vec<&'static str> {
    SERVICES
        .iter()
        .map(|(s, _)| *s)
        .filter(|s| services.is_none_or(|list| list.iter().any(|l| l == s)))
        .collect()
}

/// Each service switched on (looser) or off (stricter) between two `services:` values.
fn service_changes(
    before: Option<&[String]>,
    after: Option<&[String]>,
    looser: &mut Vec<Value>,
    stricter: &mut Vec<Value>,
) {
    let (was, now) = (services_on(before), services_on(after));
    for (service, _) in SERVICES {
        let change = |from: &str, to: &str| json!({"name":SERVICES_KEY,"service":service,"from":from,"to":to});
        match (was.contains(&service), now.contains(&service)) {
            (false, true) => looser.push(change("off", "on")),
            (true, false) => stricter.push(change("on", "off")),
            _ => {}
        }
    }
}

fn settings_from(value: &Value) -> Settings {
    Settings {
        rules: rules_from(&value["rules"]),
        mode: value[MODE_KEY]
            .as_str()
            .filter(|m| MODE_NAMES.contains(m))
            .map(str::to_string),
        exchange: value[EXCHANGE_KEY].as_str().map(str::to_string),
        // Kept as recorded: names are checked again where they are used.
        services: value[SERVICES_KEY].as_array().map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        }),
    }
}

/// What the file means right now, read without writing anything, and the record a load
/// would write when the file's bytes changed since the last load. `mode` is the approval mode
/// setup stored, which the file's `approval_mode:` replaces.
fn evaluate(
    library: &Library,
    dir: &Path,
    mode: Option<&str>,
) -> Result<(Active, Option<Change>), &'static str> {
    let (source, print) = read(dir);
    let stored: Value = library
        .state(LOADED_KEY)
        .map_err(|_| "library_unavailable")?
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(Value::Null);
    let changed = stored["fingerprint"].as_str() != Some(print.as_str());
    let at = now();
    // The last valid rules and settings from a file, kept while a later file can't be used.
    let last_valid = stored["valid"].clone();
    let (settings, problem, valid) = match source {
        // No file keeps the services in force: removing or renaming it switches none on.
        Source::Missing => (
            Settings {
                services: settings_from(&stored).services,
                ..Settings::default()
            },
            None,
            Value::Null,
        ),
        Source::Rules(settings) => {
            let valid = if changed {
                let mut v = settings_value(&settings);
                v["at"] = json!(at);
                v
            } else {
                last_valid
            };
            (settings, None, valid)
        }
        Source::Invalid(problem, services) => {
            let mut settings = settings_from(&last_valid);
            // No valid file since the last one went missing, or none ever: the services in
            // force until now when some were switched off, else what the broken file says.
            // Either way the order of deleting and breaking the file changes nothing.
            if last_valid.is_null() {
                settings.services = settings_from(&stored).services.or(services);
            }
            (settings, Some(problem), last_valid)
        }
    };
    let kept_from = problem.as_ref().and(valid["at"].as_u64());
    // The last valid file's folder is never kept: a broken file names none.
    let file_names_exchange = print != "missing";
    let exchange = if problem.is_some() {
        None
    } else {
        settings.exchange.clone()
    };
    let active = Active {
        strict: problem.is_some() && valid.is_null(),
        rules: settings.rules.clone(),
        mode: settings.mode.clone().or(mode.map(str::to_string)),
        mode_in_file: settings.mode.is_some(),
        exchange: exchange.clone(),
        file_names_exchange,
        fingerprint: print.clone(),
        problem,
        kept_from,
        services: services_on(settings.services.as_deref()),
        services_in_file: settings.services.is_some() && print != "missing",
    };
    if !changed {
        return Ok((active, None));
    }
    let first = stored.is_null();
    let old = rules_from(&stored["rules"]);
    let require_all = require_all(library);
    // The folder in force: with no file, the one set up in the terminal.
    let in_force = if file_names_exchange {
        exchange
    } else {
        stored_exchange(library)
    };
    let (mut looser, mut stricter) =
        setting_changes(library, &stored, &settings, &in_force, mode, require_all);
    // The folder set up with `icloud-mcp exchange` now named in the file: the same folder,
    // so neither looser nor stricter, but recorded so the person sees where it went.
    let legacy = stored_exchange(library);
    let copied = (file_names_exchange
        && (stored.is_null() || stored["fingerprint"] == "missing")
        && legacy.is_some()
        && in_force == legacy)
        .then_some(legacy)
        .flatten();
    let (rules_looser, rules_stricter) =
        diff(&old, &active.rules, active.mode.as_deref(), require_all);
    looser.extend(rules_looser);
    stricter.extend(rules_stricter);
    let seq = stored["seq"].as_u64().unwrap_or(0) + 1;
    // A folder that never had a file has nothing to report.
    let quiet = first && print == "missing";
    let mut record = settings_value(&settings);
    record[EXCHANGE_KEY] = json!(active.exchange);
    for (key, value) in [
        ("fingerprint", json!(print)),
        ("problem", json!(active.problem)),
        ("loaded_at", if quiet { Value::Null } else { json!(at) }),
        ("seq", json!(if quiet { seq - 1 } else { seq })),
        ("valid", valid),
    ] {
        record[key] = value;
    }
    let event = (!quiet).then(|| {
        let mut event = json!({"seq":seq,"at":at,"looser":looser,"stricter":stricter,
            "problem":active.problem,"kept":active.kept_from,"missing":print == "missing"});
        if let Some(copied) = &copied {
            event["exchange_copied"] = json!(copied);
        }
        event
    });
    Ok((active, Some((record, event))))
}

/// The rules and settings in force, read the way the server reads them, without recording
/// anything. `mode` is the approval mode setup stored.
pub fn peek(library: &Library, dir: &Path, mode: Option<&str>) -> Result<Active, &'static str> {
    evaluate(library, dir, mode).map(|(active, _)| active)
}

/// Reads the file and, when its bytes changed since the last load, records the load and what
/// it made looser or stricter. Fails only when the library can't record it.
pub fn load(library: &mut Library, dir: &Path, mode: Option<&str>) -> Result<Active, &'static str> {
    load_as(library, dir, mode, None)
}

/// [`load`] right after setup wrote the file in the person's own terminal. When the file is
/// still the one setup wrote (`print`, from [`set_setting_print`]), the change goes into the
/// history but raises no "became looser" notice: the person just chose it. Any other file
/// is loaded as usual.
pub fn load_own(
    library: &mut Library,
    dir: &Path,
    mode: Option<&str>,
    print: &str,
) -> Result<Active, &'static str> {
    load_as(library, dir, mode, Some(print))
}

fn load_as(
    library: &mut Library,
    dir: &Path,
    mode: Option<&str>,
    own: Option<&str>,
) -> Result<Active, &'static str> {
    let (active, change) = evaluate(library, dir, mode)?;
    if let Some((record, event)) = change {
        let mut pending = looser_unseen(library);
        if let Some(event) = event.as_ref().filter(|_| own != Some(&active.fingerprint)) {
            add_looser(&mut pending, event);
        }
        let mut history = history(library);
        history.extend(event);
        let keep = history.len().saturating_sub(MAX_HISTORY);
        history.drain(..keep);
        library
            .atomically(|library| -> quince_core::Result<()> {
                library.set_state(HISTORY_KEY, &Value::Array(history).to_string())?;
                library.set_state(LOOSER_KEY, &pending.to_string())?;
                library.set_state(LOADED_KEY, &record.to_string())
            })
            .map_err(|_| "library_unavailable")?;
    }
    Ok(active)
}

/// The looser changes not yet seen: `{"count": n, "newest": [change...]}`.
fn looser_unseen(library: &Library) -> Value {
    library
        .state(LOOSER_KEY)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"count":0,"newest":[]}))
}

fn add_looser(pending: &mut Value, event: &Value) {
    let looser = event["looser"].as_array().cloned().unwrap_or_default();
    if looser.is_empty() {
        return;
    }
    let count = pending["count"].as_u64().unwrap_or(0) + looser.len() as u64;
    let mut newest = pending["newest"].as_array().cloned().unwrap_or_default();
    for mut change in looser {
        change["at"] = event["at"].clone();
        newest.push(change);
    }
    let keep = newest.len().saturating_sub(MAX_LOOSER);
    newest.drain(..keep);
    *pending = json!({"count":count,"newest":newest});
}

/// The newest `limit` log lines, oldest first.
fn log_lines(dir: &Path, limit: usize) -> Vec<Value> {
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(dir.join(LOG_NAME))
    else {
        return Vec::new();
    };
    // Only a regular file is read: a pipe or device in its place is skipped.
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Vec::new();
    }
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
    let text = |key: &str| shown(c[key].as_str().unwrap_or("none"));
    if c["name"] == MODE_KEY {
        return format!("approval mode: {} before, now {}", text("from"), text("to"));
    }
    if c["name"] == EXCHANGE_KEY {
        return format!(
            "exchange folder: {} before, now {}",
            text("from"),
            text("to")
        );
    }
    if c["name"] == SERVICES_KEY {
        let service = c["service"].as_str().unwrap_or_default();
        let state = |key: &str| if c[key] == "on" { "on" } else { "switched off" };
        return format!(
            "{SERVICES_KEY}: {} was {} before, now {}",
            service_names(&[service]),
            state("from"),
            state("to")
        );
    }
    // Not an action: allow lets changes run, anything else refuses them.
    if c["name"] == crate::calendar_write::INVITATIONS_RULE {
        let runs = |key: &str| c[key] == Effect::Allow.as_str();
        let (was, now) = (
            if runs("from") {
                "may run"
            } else {
                "were refused"
            },
            if runs("to") {
                "they may run"
            } else {
                "they are refused"
            },
        );
        return format!(
            "{}: changes that would notify invited people {was} before, now {now}",
            crate::calendar_write::INVITATIONS_RULE
        );
    }
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

/// Status lines for the permissions file, read the way the server reads it. Writes nothing:
/// a change the server has not loaded yet is reported as if it had.
pub fn status(library: &Library, dir: &Path, mode: Option<&str>) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let Ok((active, change)) = evaluate(library, dir, mode) else {
        out.push((
            "iCloud-MCP couldn't read what it recorded about the permissions file.".into(),
            Some("Run icloud-mcp status again in a moment.".into()),
        ));
        return out;
    };
    match (active.fallback(), active.fingerprint.as_str()) {
        (Some(text), _) => out.push((
            text,
            Some("Fix the file, then run icloud-mcp permissions show.".into()),
        )),
        (None, "missing") => out.push((
            "Permissions file: none. The approval mode decides everything.".into(),
            None,
        )),
        (None, _) => out.push((
            format!(
                "Permissions file: {} in use. Everything else follows the approval mode.",
                match active.rules.len() {
                    1 => "1 rule".to_string(),
                    n => format!("{n} rules"),
                }
            ),
            None,
        )),
    }
    out.push(exchange_line(library, dir, &active));
    out.push((services_line(dir, &active), None));
    let mut pending = looser_unseen(library);
    if let Some(event) = change.and_then(|(_, event)| event) {
        add_looser(&mut pending, &event);
    }
    let count = pending["count"].as_u64().unwrap_or(0);
    if let Some(newest) = pending["newest"].as_array().and_then(|n| n.last()) {
        let more = if count > 1 {
            format!(" (and {} earlier)", count - 1)
        } else {
            String::new()
        };
        out.push((
            format!(
                "The permissions changed since you last looked and some actions became looser, most recently {}{more}.",
                change_line(newest)
            ),
            Some("Run icloud-mcp permissions show to see every change.".into()),
        ));
    }
    out
}

/// Which services are on, and how to change that.
fn services_line(dir: &Path, active: &Active) -> String {
    let how = if active.services_off().is_empty() && !active.services_in_file {
        "To switch one off, add a"
    } else {
        "To change this, edit the"
    };
    format!(
        "{} {how} {SERVICES_KEY}: line in {} or run icloud-mcp setup; a switched-off service's tools are not offered, and nothing from it is read, cached or synced.",
        active.services_sentence(),
        dir.join(FILE_NAME).display()
    )
}

/// Whether `tool` may be offered and run while `services` are on: its service is on, and
/// `sync` needs Notes or Reminders.
pub fn tool_on(services: &[&str], tool: &str) -> bool {
    match (tool, service_of(tool)) {
        ("sync", _) => services.contains(&"notes") || services.contains(&"reminders"),
        (_, Some(service)) => services.contains(&service),
        (_, None) => true,
    }
}

/// The services on according to the file alone, read without the folder or its records:
/// `None` when the file is missing or can't be used, since then only the records can say.
pub fn services_in_file(dir: &Path) -> Option<Vec<&'static str>> {
    match read(dir).0 {
        Source::Rules(settings) => Some(services_on(settings.services.as_deref())),
        _ => None,
    }
}

/// The status line for the exchange folder in force, with the fix when it can't be used.
fn exchange_line(library: &Library, dir: &Path, active: &Active) -> (String, Option<String>) {
    let file = dir.join(FILE_NAME);
    if active.problem.is_some() {
        return (
            "Exchange folder: none while permissions.yaml can't be used, so Drive and Photos uploads and Drive downloads are refused.".into(),
            Some(crate::exchange::file_unusable_step(dir, true)),
        );
    }
    if active.file_names_exchange && active.exchange.is_none() {
        if let Some(earlier) = crate::exchange::not_in_file(dir) {
            return (
                format!("Exchange folder: none, so Drive and Photos uploads and Drive downloads are refused. {earlier}"),
                Some(crate::exchange::not_in_file_fix(dir)),
            );
        }
    }
    let legacy = crate::exchange::configured(library)
        .ok()
        .flatten()
        .filter(|_| !active.file_names_exchange);
    let (written, from) = match (&active.exchange, &legacy) {
        (Some(written), _) => (written.clone(), "set in permissions.yaml"),
        (None, Some(e)) => (
            e.path.display().to_string(),
            "set up with icloud-mcp exchange",
        ),
        (None, None) => {
            return (
                format!(
                    "Exchange folder: not set up, so Drive and Photos uploads and Drive downloads are refused. To set one, add {EXCHANGE_KEY}: to {} or run icloud-mcp exchange.",
                    file.display()
                ),
                None,
            )
        }
    };
    match crate::exchange::in_force(library, dir) {
        Ok(Some(e)) => (
            format!(
                "Exchange folder: {}, {from}. Drive and Photos uploads read from it and Drive downloads write into it.",
                e.path.display()
            ),
            None,
        ),
        Ok(None) => (
            "Exchange folder: not set up, so Drive and Photos uploads and Drive downloads are refused.".into(),
            Some("Run icloud-mcp exchange.".into()),
        ),
        Err(code) => (
            format!(
                "Exchange folder: {} ({from}) {}, so Drive and Photos uploads and Drive downloads are refused.",
                shown_problem(&written),
                crate::exchange::problem(code)
            ),
            Some(crate::exchange::fix(code, dir, active.exchange.as_deref())),
        ),
    }
}

/// What the server really does for `effect` on one action, and why when that differs: a
/// deletion allowed by a rule still asks on the card, in every mode.
fn runs_as(tool: &str, action: &str, effect: Effect) -> (Effect, Option<&'static str>) {
    if is_deletion(tool, action) && effect == Effect::Allow {
        (Effect::AskClient, Some("a deletion always needs a person"))
    } else {
        (effect, None)
    }
}

/// A rule and what it does, as the server runs it (see [`runs_as`]).
fn described(name: &str, effect: Effect) -> String {
    let all = targets();
    let target = all.iter().find(|t| t.name == name);
    let own = target.and_then(|t| Some(runs_as(&t.tool, t.action.as_deref()?, effect)));
    let drive = target.is_some_and(|t| t.tool == crate::drive_write::TOOL);
    let action = target.and_then(|t| t.action.as_deref());
    let tool_floored = target.is_some_and(|t| {
        t.action.is_none()
            && all.iter().any(|o| {
                o.tool == t.tool
                    && o.action
                        .as_deref()
                        .is_some_and(|a| runs_as(&o.tool, a, effect).1.is_some())
            })
    });
    let plain = format!("{}, {}", effect.as_str(), effect.sentence());
    if target.is_some_and(|t| is_invitations(&t.tool, t.action.as_deref())) {
        return match effect {
            Effect::Allow => "allow, a Calendar change to an event with other people invited may run and send them an update or cancellation; the change still follows its own rule".into(),
            _ => "deny, a Calendar change to an event with other people invited is refused".into(),
        };
    }
    match own {
        Some((really, Some(why))) => {
            format!(
                "{}, but {}, because {why}",
                effect.as_str(),
                really.sentence()
            )
        }
        _ if tool_floored && target.is_some_and(|t| t.tool == crate::calendar_write::TOOL) => {
            format!("{plain}, except deletions, which still ask; changes that would notify invited people stay refused unless {} is allow", crate::calendar_write::INVITATIONS_RULE)
        }
        _ if tool_floored => format!("{plain}, except deletions, which still ask"),
        _ if drive
            && effect == Effect::Allow
            && (action.is_none() || matches!(action, Some("rename" | "move"))) =>
        {
            format!("{plain}, except renaming or moving a large folder, which still asks")
        }
        _ => plain,
    }
}

fn shown_problem(problem: &str) -> String {
    problem
        .chars()
        .filter(|c| !c.is_control())
        .take(300)
        .collect()
}

/// `icloud-mcp permissions show`. With `mark_seen`, the changes shown count as seen.
pub fn show(dir: &Path, mark_seen: bool) -> Result<String, &'static str> {
    // Never creates a folder or a library: a folder that isn't set up gets a pointer to setup.
    if !dir.join("library.db").is_file() {
        return Err("permissions_setup_required");
    }
    let held = crate::state::PrivateState::open(dir)?;
    let dir = held.path.as_path();
    let mut library = crate::setup(dir)?;
    let (mode, mode_text) = match crate::stored_mode(&library, dir) {
        Ok(Some(m)) => (
            Some(m.clone()),
            crate::status::describe_mode(&m).to_string(),
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
    let mode_text = match (&active.mode, active.mode_in_file) {
        (Some(m), true) => crate::status::describe_mode(m).to_string(),
        _ => mode_text,
    };
    let loaded: Value = library
        .state(LOADED_KEY)
        .map_err(|_| "library_unavailable")?
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(Value::Null);
    let mut out = format!("Permissions file: {}\n", dir.join(FILE_NAME).display());
    out.push_str("iCloud-MCP reads it before every tool call, so a change counts at once.\n");
    if let Some(at) = loaded["loaded_at"].as_u64() {
        out.push_str(&format!("Last change loaded: {}\n", when(at)));
    }
    if let (Some(text), Some(problem)) = (active.fallback_in(true), &active.problem) {
        out.push_str(&format!("\n{text}\n"));
        if problem.contains("is not an iCloud-MCP tool or action") {
            out.push_str(&format!("Valid names: {}\n", names().join(", ")));
        }
    }
    match (&active.problem, active.fingerprint.as_str()) {
        (Some(_), _) if active.rules.is_empty() => {}
        (Some(_), _) => {
            out.push_str("\nLast valid rules, still in use:\n");
            for (name, effect) in &active.rules {
                out.push_str(&format!("  {name}: {}\n", described(name, *effect)));
            }
        }
        (None, "missing") => {
            out.push_str("\nThere is no permissions file. The approval mode decides everything.\n")
        }
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
                out.push_str(&format!("  {name}: {}{from}\n", described(name, *effect)));
            }
        }
    }
    out.push_str(&format!(
        "Everything not listed follows the approval mode: {mode_text}\n"
    ));
    match (&active.mode, active.mode_in_file) {
        (Some(m), true) => out.push_str(&format!(
            "Approval mode: {m}, set in permissions.yaml ({MODE_KEY}:). It replaces the mode chosen in setup.\n"
        )),
        (Some(m), false) => out.push_str(&format!(
            "Approval mode: {m}, chosen in setup. A {MODE_KEY}: line in the file would replace it.\n"
        )),
        (None, _) => {}
    }
    let (exchange, fix) = exchange_line(&library, dir, &active);
    out.push_str(&format!("{exchange}\n"));
    if let Some(fix) = fix {
        out.push_str(&format!("  Next step: {fix}\n"));
    }
    out.push_str(&format!("{}\n", services_line(dir, &active)));
    let unseen = unseen(&library);
    let pending = looser_unseen(&library);
    let total = pending["count"].as_u64().unwrap_or(0);
    let listed: u64 = unseen
        .iter()
        .map(|e| e["looser"].as_array().map_or(0, |a| a.len() as u64))
        .sum();
    if unseen.is_empty() && total == 0 {
        out.push_str("\nNo changes since you last looked.\n");
    } else {
        out.push_str("\nChanges since you last looked:\n");
        if total > listed {
            if let Some(newest) = pending["newest"].as_array().and_then(|n| n.last()) {
                out.push_str(&format!(
                    "  {} looser changes are older than the history kept here; the newest looser change: {}\n",
                    total - listed,
                    change_line(newest)
                ));
            }
        }
        for event in &unseen {
            let at = when(event["at"].as_u64().unwrap_or(0));
            let looser = event["looser"].as_array().cloned().unwrap_or_default();
            let stricter = event["stricter"].as_array().cloned().unwrap_or_default();
            if let Some(problem) = event["problem"].as_str() {
                let then = if event["kept"].is_u64() {
                    "the last valid rules stayed in use"
                } else {
                    "the approval mode decided, and every deletion asked"
                };
                out.push_str(&format!(
                    "  {at}: the file became unusable ({}), so {then}.\n",
                    shown_problem(problem)
                ));
            } else if event["missing"] == true {
                out.push_str(&format!("  {at}: the file was removed.\n"));
            } else if let Some(copied) = event["exchange_copied"].as_str() {
                out.push_str(&format!(
                    "  {at}: the exchange folder set up with icloud-mcp exchange, {}, was written into the file as its {EXCHANGE_KEY}: line; only that line counts now.\n",
                    shown(copied)
                ));
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
        out.push_str("\nNo approval card has saved a rule yet.\n");
    } else {
        out.push_str(&format!(
            "\nRules saved from an approval card, newest last (full list in {}):\n",
            dir.join(LOG_NAME).display()
        ));
        for line in &log {
            let at = when(line["at"].as_u64().unwrap_or(0));
            if line["source"] == "card" {
                out.push_str(&format!(
                    "  {at} always allowed from a card: {}\n",
                    shown(line["name"].as_str().unwrap_or_default())
                ));
            } else {
                out.push_str(&format!(
                    "  {at} ran without asking, under an older version: {}.{}\n",
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
            .atomically(|library| -> quince_core::Result<()> {
                library.set_state(SEEN_KEY, &last.to_string())?;
                library.set_state(LOOSER_KEY, "")
            })
            .map_err(|_| "library_unavailable")?;
    }
    Ok(out)
}

/// A first file: every deletion asks on the card, and every valid name is listed as a comment.
pub fn starter(dir: &Path) -> String {
    // The command names the folder unless it is the default; a folder name with a line break
    // in it would end the comment, so then the person fills it in.
    let show = if dir.to_string_lossy().chars().any(char::is_control) {
        "icloud-mcp permissions show --state-dir FOLDER".to_string()
    } else {
        crate::status::with_dir("icloud-mcp permissions show", dir)
    };
    let mut out = String::from(
        "# iCloud-MCP permissions\n\
         #\n\
         # This file decides, for each tool and action, what iCloud-MCP does:\n\
         #   allow           run without asking\n\
         #   ask: client     ask on the approval card in your MCP client\n\
         #   ask: terminal   opt-in: wait until you run icloud-mcp approve in your own terminal\n\
         #   deny            always refuse\n\
         #\n\
         # iCloud-MCP reads this file before every tool call, so a change counts at once.\n\
         # Anything not listed follows the folder's approval mode.\n\
         # If the file has a mistake, iCloud-MCP keeps the last valid rules it loaded. If there are\n\
         # none, the approval mode decides and every deletion asks.\n\
         # To check it, run: SHOW_COMMAND\n\
         #\n\
         # Write ask on its own indented line, as below, or as {ask: client}.\n\
         #\n\
         # Three settings can stand beside the rules. The first two replace what setup or the\n\
         # older terminal commands stored; icloud-mcp policy, exchange and setup edit them:\n\
         #   approval_mode: reads_and_writes, writes_only or yolo. Deletions ask in every mode.\n\
         #   exchange_folder: the one folder Drive and Photos uploads read from and Drive\n\
         #     downloads write into. A private folder of your own, not a link, not a system\n\
         #     folder and not inside a hidden folder such as .ssh or the private folder.\n\
         #   services: the services iCloud-MCP may use, on one line. A service left out is\n\
         #     switched off: its tools are not offered, and nothing from it is read, cached or\n\
         #     synced. Without the line every service is on. Data cached before stays in the\n\
         #     folder, unread; to clear it, set up a fresh folder and delete this one. Apple signs in the whole account, so this is a limit inside iCloud-MCP,\n\
         #     not at Apple: someone with the sign-in could still reach every service.\n\
         # Passwords, codes and the passphrase never go in this file.\n\
         version: 1\n\
         # approval_mode: writes_only\n\
         # exchange_folder: ~/iCloud-MCP-Exchange\n\
         # services: [notes, reminders, contacts, calendar, photos, drive]\n\
         rules:\n",
    )
    .replace("SHOW_COMMAND", &show);
    for t in targets() {
        if t.action.as_deref().is_some_and(|a| is_deletion(&t.tool, a)) {
            out.push_str(&format!("  {}:\n    ask: client\n", t.name));
        }
    }
    out.push_str("  # reminders_write.create: allow\n");
    out.push_str(&format!(
        "#\n# {}: takes allow or deny only; a tool-wide calendar_write: allow does not enable it.\n",
        crate::calendar_write::INVITATIONS_RULE
    ));
    out.push_str("#\n# Names you can use:\n");
    for name in names() {
        out.push_str(&format!("#   {name}\n"));
    }
    out
}

/// Writes the starter file into `dir`, never over an existing one.
pub fn write_starter(dir: &Path) -> Result<(), &'static str> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let text = with_earlier_exchange(dir, starter(dir));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join(FILE_NAME))
        .map_err(|_| "permissions_file_unavailable")?;
    file.write_all(text.as_bytes())
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

/// The backup of the file as it was before iCloud-MCP last changed it.
pub const BACKUP_NAME: &str = "permissions.yaml.bak";

/// Saves `name: allow` because the person chose "Always allow" on a card. `expected` is the
/// fingerprint the card was shown under: a file changed since then is left alone. The old
/// file is kept as `permissions.yaml.bak`. Returns why the rule was not saved.
pub fn save_allow(dir: &Path, name: &str, expected: &str) -> Result<(), String> {
    save_allow_checked(dir, name, expected, &|| {})
}

/// [`save_allow`], with `meanwhile` run just before the last check, as an editor could.
fn save_allow_checked(
    dir: &Path,
    name: &str,
    expected: &str,
    meanwhile: &dyn Fn(),
) -> Result<(), String> {
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
    let text =
        text.unwrap_or_else(|| with_earlier_exchange(dir, String::from("version: 1\nrules:\n")));
    let mut want = parse(&text)?;
    want.insert(name.to_string(), Effect::Allow);
    let new = with_allow(&text, name).ok_or("the file's layout can't be edited safely")?;
    if parse(&new).ok().as_ref() != Some(&want) || new.len() as u64 > MAX_FILE_BYTES {
        return Err("the file's layout can't be edited safely".into());
    }
    if let Some(bytes) = &old {
        write_atomic(dir, BACKUP_NAME, bytes)?;
    }
    meanwhile();
    // Last check right before the rename: an edit that landed meanwhile wins.
    let now_print = read_bytes(dir)?
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

/// `text` with the top-level `key: value` set: an existing line is rewritten in place,
/// keeping its comment; a new one goes after the `version:` line, or before the first setting.
fn with_setting(text: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let content = |l: &str| uncommented(l).trim_end().to_string();
    let top_key = |l: &str| {
        let c = content(l);
        (!c.is_empty() && !c.starts_with(' '))
            .then(|| c.split_once(':').map(|(k, _)| unquote(k).to_string()))
            .flatten()
    };
    let line = format!("{key}: {value}");
    if let Some(i) = lines
        .iter()
        .position(|l| top_key(l).as_deref() == Some(key))
    {
        let comment = lines[i][content(&lines[i]).len()..].to_string();
        lines[i] = format!("{line}{comment}");
    } else if let Some(i) = lines
        .iter()
        .position(|l| top_key(l).as_deref() == Some("version"))
    {
        lines.insert(i + 1, line);
    } else {
        let first = lines
            .iter()
            .position(|l| top_key(l).is_some())
            .unwrap_or(lines.len());
        lines.insert(first, line);
    }
    lines.join("\n") + "\n"
}

/// `text`, the first file iCloud-MCP writes in `dir`, with the folder set up earlier with
/// `icloud-mcp exchange` as its `exchange_folder:` line, when that folder still passes every
/// check. Once the file exists only its line counts, so the folder is not lost.
fn with_earlier_exchange(dir: &Path, text: String) -> String {
    match crate::exchange::earlier_still_valid(dir) {
        Some(path) => {
            let new = with_setting(&text, EXCHANGE_KEY, &written(&path));
            // Only when the line reads back as that folder.
            match parse_settings(&new) {
                Ok(s) if s.exchange.as_deref() == Some(path.as_str()) => new,
                _ => text,
            }
        }
        None => text,
    }
}

/// A value as the file writes it: plain, or in double quotes when it would not read back.
fn written(value: &str) -> String {
    let plain = !value.contains(" #")
        && !value.contains(": ")
        && value.trim() == value
        && !value.starts_with(['"', '\'', '{', '[', '&', '*', '!', '|', '>', '%', '@']);
    if plain {
        value.to_string()
    } else {
        format!("\"{value}\"")
    }
}

/// The value of `key` in the file as it is now, when the file can be used.
pub fn setting_in_file(dir: &Path, key: &str) -> Option<String> {
    let bytes = read_bytes(dir).ok()??;
    let settings = parse_settings(&String::from_utf8(bytes).ok()?).ok()?;
    match key {
        MODE_KEY => settings.mode,
        EXCHANGE_KEY => settings.exchange,
        SERVICES_KEY => settings.services.map(|s| format!("[{}]", s.join(", "))),
        _ => None,
    }
}

/// Whether [`set_setting`] could edit the file now: it is missing, or a usable file.
pub fn editable(dir: &Path) -> Result<(), String> {
    match read_bytes(dir).map_err(|p| format!("the file can't be used: {p}"))? {
        None => Ok(()),
        Some(bytes) => {
            let text = String::from_utf8(bytes)
                .map_err(|_| "the file has a mistake: it is not UTF-8 text".to_string())?;
            parse_settings(&text)
                .map(|_| ())
                .map_err(|p| format!("the file has a mistake: {}", shown_problem(&p)))
        }
    }
}

/// Sets `approval_mode:`, `exchange_folder:` or `services:` in the file, for `icloud-mcp policy`,
/// `icloud-mcp exchange` and setup, in the person's own terminal. Every other line and
/// comment stays as it was; the old file is kept as `permissions.yaml.bak`, and the new one is
/// written whole and private. A missing file becomes a short one; a file with a mistake is
/// left alone. Returns why nothing was written.
pub fn set_setting(dir: &Path, key: &str, value: &str) -> Result<(), String> {
    set_setting_print(dir, key, value, None).map(|_| ())
}

/// [`set_setting`], returning the fingerprint of the file it wrote. With `expected`, the
/// fingerprint of the file as last loaded ([`Active::fingerprint`]), a file changed since then
/// is left alone, so an edit made meanwhile is never recorded as part of this write.
pub fn set_setting_print(
    dir: &Path,
    key: &str,
    value: &str,
    expected: Option<&str>,
) -> Result<String, String> {
    if ![MODE_KEY, EXCHANGE_KEY, SERVICES_KEY].contains(&key) {
        return Err(format!("{} is not a setting of this file", shown(key)));
    }
    editable(dir)?;
    let old = read_bytes(dir)?;
    let print = old
        .as_deref()
        .map_or_else(|| "missing".to_string(), fingerprint);
    if expected.is_some_and(|e| e != print) {
        return Err("the file changed while setup was saving; run icloud-mcp setup again".into());
    }
    let text = match &old {
        Some(bytes) => {
            String::from_utf8(bytes.clone()).map_err(|_| "the file is not UTF-8 text")?
        }
        None if key == EXCHANGE_KEY => String::from("version: 1\n"),
        None => with_earlier_exchange(dir, String::from("version: 1\n")),
    };
    let mut want = parse_settings(&text)?;
    match key {
        MODE_KEY => want.mode = Some(value.to_string()),
        SERVICES_KEY => want.services = Some(services_value(value)?),
        _ => want.exchange = Some(value.to_string()),
    }
    // A list is written as it is; it never needs quotes.
    let value = if key == SERVICES_KEY {
        value.to_string()
    } else {
        written(value)
    };
    let new = with_setting(&text, key, &value);
    match parse_settings(&new) {
        Ok(got) if got == want && new.len() as u64 <= MAX_FILE_BYTES => {}
        Ok(_) => return Err("the file's layout can't be edited safely".into()),
        Err(problem) => {
            return Err(format!(
                "{} can't be written there: {}",
                shown(&value),
                shown_problem(&problem)
            ))
        }
    }
    if let Some(bytes) = &old {
        write_atomic(dir, BACKUP_NAME, bytes)?;
    }
    // An edit that landed meanwhile wins.
    let now_print = read_bytes(dir)?
        .as_deref()
        .map_or_else(|| "missing".to_string(), fingerprint);
    if now_print != print {
        return Err("the file changed while it was being edited; run the command again".into());
    }
    write_atomic(dir, FILE_NAME, new.as_bytes())?;
    Ok(fingerprint(new.as_bytes()))
}

fn append_log(dir: &Path, entry: Value) -> Result<(), &'static str> {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_just_before_the_rename_wins_and_nothing_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "rules:\n  contacts_get: deny\n").unwrap();
        let print = read(dir.path()).1;
        let edited = "rules:\n  contacts_get: deny\n  notes_list: deny\n";
        let error = save_allow_checked(dir.path(), "notes_list", &print, &|| {
            std::fs::write(&path, edited).unwrap();
        })
        .unwrap_err();
        assert!(error.contains("changed while the card was open"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
        assert!(
            !dir.path().join(LOG_NAME).exists(),
            "nothing was saved, so nothing is logged"
        );
    }
}
