//! Per-tool permissions from `permissions.yaml` in the private folder.
//!
//! The server reads the file before every tool call, so an edit counts at once. A missing
//! file leaves everything to the folder's approval mode. An unusable file never loosens
//! anything: the last valid rules stay in use, and with none every deletion asks. Every load
//! that changes the rules is recorded in the library, so `status` and `permissions show` can
//! say what became looser. No MCP tool reads or writes the file or these records; the server
//! writes one rule only when the person picks "Always allow" on an approval card.
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
/// Looser changes since `permissions show` last ran in the person's terminal, kept apart from
/// the history so that later edits can't push them out.
const LOOSER_KEY: &str = "mcp.permissions.looser_unseen";
const MAX_LOOSER: usize = 20;
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
        "permissions_setup_required" => "This folder isn't set up yet. Run skykeep-mcp setup first.",
        "state_busy_retry" => "The server is using the folder right now. Run skykeep-mcp permissions show again in a moment.",
        "state_unavailable" => "Skykeep can't open that folder. Check that it exists and belongs to you, or add --state-dir FOLDER.",
        "unsafe_state_contents" | "state_must_be_owned_and_private" | "unsafe_state_path" => "The folder isn't private to you, or holds a link or a shared file. Run skykeep-mcp status to see what to fix.",
        "library_unavailable" => "Skykeep couldn't read its records in that folder. Run skykeep-mcp status to see why.",
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
                other if names().iter().any(|n| n == other) => {
                    return Err(at(format!(
                        "{} must be indented under rules:, with spaces in front of it",
                        shown(other)
                    )))
                }
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
                "line {line}: {} is not a Skykeep tool or action; {}",
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
                "line {line}: {} can't use ask: terminal, because skykeep-mcp approve only runs changes; use allow, ask: client or deny",
                shown(name)
            ));
        }
    }
    Ok(rules)
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
            return Err("the file is a link; Skykeep reads only a real file in its folder".into())
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
    /// The file's rules; while the file is unusable, the last valid rules loaded, if any.
    pub rules: Rules,
    /// Changes whenever the file's bytes change; binds approvals to the rules they saw.
    pub fingerprint: String,
    pub problem: Option<String>,
    /// When the rules in use were loaded, while the file is unusable and older rules stand in.
    pub kept_from: Option<u64>,
    /// The file is unusable and there are no earlier valid rules: every deletion asks.
    pub strict: bool,
}

impl Active {
    /// A sentence on what Skykeep does while the file can't be used, for tool results and
    /// status. `permissions show` lists the valid names itself, so it passes `in_show`.
    pub fn fallback(&self) -> Option<String> {
        self.fallback_in(false)
    }

    fn fallback_in(&self, in_show: bool) -> Option<String> {
        let problem = self.problem.as_deref()?;
        let names = if !in_show && problem.contains("is not a Skykeep tool or action") {
            " To see every valid name, run skykeep-mcp permissions show."
        } else {
            ""
        };
        Some(match self.kept_from {
            Some(at) => format!(
                "The permissions file can't be used ({}). Skykeep keeps the last valid rules, loaded {}.{names}",
                shown_problem(problem),
                when(at)
            ),
            None => format!(
                "The permissions file can't be used ({}). Skykeep follows the approval mode and asks before every deletion until the file is fixed.{names}",
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
            Some(a) => runs_as(mode, &t.tool, a, e).0,
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

/// What the file means right now, read without writing anything, and the record a load
/// would write when the file's bytes changed since the last load.
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
    // The last valid rules from a file, kept while a later file can't be used.
    let last_valid = stored["valid"].clone();
    let (rules, problem, valid) = match source {
        Source::Missing => (Rules::new(), None, Value::Null),
        Source::Rules(rules) => {
            let valid = if changed {
                json!({"rules":rules_value(&rules),"at":at})
            } else {
                last_valid
            };
            (rules, None, valid)
        }
        Source::Invalid(problem) => (rules_from(&last_valid["rules"]), Some(problem), last_valid),
    };
    let kept_from = problem.as_ref().and(valid["at"].as_u64());
    let active = Active {
        strict: problem.is_some() && valid.is_null(),
        rules,
        fingerprint: print.clone(),
        problem,
        kept_from,
    };
    if !changed {
        return Ok((active, None));
    }
    let first = stored.is_null();
    let old = rules_from(&stored["rules"]);
    let (looser, stricter) = diff(&old, &active.rules, mode, require_all(library));
    let seq = stored["seq"].as_u64().unwrap_or(0) + 1;
    // A folder that never had a file has nothing to report.
    let quiet = first && print == "missing";
    let record = json!({"fingerprint":print,"rules":rules_value(&active.rules),
        "problem":active.problem,"loaded_at":if quiet { Value::Null } else { json!(at) },
        "seq":if quiet { seq - 1 } else { seq },"valid":valid});
    let event = (!quiet).then(|| {
        json!({"seq":seq,"at":at,"looser":looser,"stricter":stricter,
            "problem":active.problem,"kept":active.kept_from,"missing":print == "missing"})
    });
    Ok((active, Some((record, event))))
}

/// Reads the file and, when its bytes changed since the last load, records the load and what
/// it made looser or stricter. Fails only when the library can't record it.
pub fn load(library: &mut Library, dir: &Path, mode: Option<&str>) -> Result<Active, &'static str> {
    let (active, change) = evaluate(library, dir, mode)?;
    if let Some((record, event)) = change {
        let mut pending = looser_unseen(library);
        if let Some(event) = &event {
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

/// Records that a deletion ran without asking because the file allowed it: tool, action and
/// time, never content. Called after the deletion ran; when the log can't be written, the
/// caller says so in the tool result.
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
            "Skykeep couldn't read what it recorded about the permissions file.".into(),
            Some("Run skykeep-mcp status again in a moment.".into()),
        ));
        return out;
    };
    match (active.fallback(), active.fingerprint.as_str()) {
        (Some(text), _) => out.push((
            text,
            Some("Fix the file, then run skykeep-mcp permissions show.".into()),
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
            Some("Run skykeep-mcp permissions show to see every change.".into()),
        ));
    }
    out
}

/// A rule and what it does, after the minimum the server keeps: without an approval mode,
/// deletions always wait for the terminal.
/// What the server really does for `effect` on one action, and why when that differs: without
/// an approval mode every deletion waits for the terminal, and a Drive deletion allowed by a
/// rule still asks (the card, or the terminal under yolo). `ask: client` stays the card.
fn runs_as(
    mode: Option<&str>,
    tool: &str,
    action: &str,
    effect: Effect,
) -> (Effect, Option<&'static str>) {
    if !is_deletion(tool, action) {
        return (effect, None);
    }
    match mode {
        None if effect < Effect::AskTerminal => (
            Effect::AskTerminal,
            Some("a folder without an approval mode always asks before deleting"),
        ),
        Some(mode) if tool == crate::drive_write::TOOL && effect == Effect::Allow => (
            if mode == "yolo" {
                Effect::AskTerminal
            } else {
                Effect::AskClient
            },
            Some("a Drive deletion always needs a person"),
        ),
        Some(mode) if tool == crate::calendar_write::TOOL && effect == Effect::Allow => (
            if mode == "yolo" {
                Effect::AskTerminal
            } else {
                Effect::AskClient
            },
            Some("a Calendar delete always needs a person"),
        ),
        Some(mode) if tool == crate::contacts_write::TOOL && effect == Effect::Allow => (
            if mode == "yolo" {
                Effect::AskTerminal
            } else {
                Effect::AskClient
            },
            Some("a Contacts deletion always needs a person"),
        ),
        _ => (effect, None),
    }
}

/// A rule and what it does, as the server runs it (see [`runs_as`]).
fn described(name: &str, effect: Effect, mode: Option<&str>) -> String {
    let all = targets();
    let target = all.iter().find(|t| t.name == name);
    let own = target.and_then(|t| Some(runs_as(mode, &t.tool, t.action.as_deref()?, effect)));
    let drive = target.is_some_and(|t| t.tool == crate::drive_write::TOOL);
    let action = target.and_then(|t| t.action.as_deref());
    let tool_floored = target.is_some_and(|t| {
        t.action.is_none()
            && all.iter().any(|o| {
                o.tool == t.tool
                    && o.action
                        .as_deref()
                        .is_some_and(|a| runs_as(mode, &o.tool, a, effect).1.is_some())
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

/// `skykeep-mcp permissions show`. With `mark_seen`, the changes shown count as seen.
pub fn show(dir: &Path, mark_seen: bool) -> Result<String, &'static str> {
    // Never creates a folder or a library: a folder that isn't set up gets a pointer to setup.
    if !dir.join("library.db").is_file() {
        return Err("permissions_setup_required");
    }
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
    if let (Some(text), Some(problem)) = (active.fallback_in(true), &active.problem) {
        out.push_str(&format!("\n{text}\n"));
        if problem.contains("is not a Skykeep tool or action") {
            out.push_str(&format!("Valid names: {}\n", names().join(", ")));
        }
    }
    match (&active.problem, active.fingerprint.as_str()) {
        (Some(_), _) if active.rules.is_empty() => {}
        (Some(_), _) => {
            out.push_str("\nLast valid rules, still in use:\n");
            for (name, effect) in &active.rules {
                out.push_str(&format!(
                    "  {name}: {}\n",
                    described(name, *effect, mode.as_deref())
                ));
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
                out.push_str(&format!(
                    "  {name}: {}{from}\n",
                    described(name, *effect, mode.as_deref())
                ));
            }
        }
    }
    out.push_str(&format!(
        "Everything not listed follows the approval mode: {mode_text}\n"
    ));
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
            .atomically(|library| -> quince_core::Result<()> {
                library.set_state(SEEN_KEY, &last.to_string())?;
                library.set_state(LOOSER_KEY, "")
            })
            .map_err(|_| "library_unavailable")?;
    }
    Ok(out)
}

/// A first file for `dir`'s approval mode: deletions ask, the way the mode already asks or
/// more strictly, and every valid name is listed as a comment.
pub fn starter(mode: Option<&str>, dir: &Path) -> String {
    // The command names the folder unless it is the default; a folder name with a line break
    // in it would end the comment, so then the person fills it in.
    let show = if dir.to_string_lossy().chars().any(char::is_control) {
        "skykeep-mcp permissions show --state-dir FOLDER".to_string()
    } else {
        crate::status::with_dir("skykeep-mcp permissions show", dir)
    };
    // On the card where the mode already asks there; in the terminal under yolo and without
    // a mode, where Drive deletions already wait for it.
    let ask = if mode.is_some_and(|m| m != "yolo") {
        "client"
    } else {
        "terminal"
    };
    let mut out = String::from(
        "# Skykeep permissions\n\
         #\n\
         # This file decides, for each tool and action, what Skykeep does:\n\
         #   allow           run without asking\n\
         #   ask: client     ask on a card in your MCP client: approve once, deny or always allow\n\
         #   ask: terminal   wait until you run skykeep-mcp approve in your own terminal\n\
         #   deny            always refuse\n\
         #\n\
         # Skykeep reads this file before every tool call, so a change counts at once.\n\
         # Anything not listed follows the folder's approval mode.\n\
         # If the file has a mistake, Skykeep keeps the last valid rules it loaded. If there are\n\
         # none, the approval mode decides and every deletion asks.\n\
         # To check it, run: SKYKEEP_SHOW\n\
         #\n\
         # Write ask on its own indented line, as below, or as {ask: terminal}.\n\
         version: 1\n\
         rules:\n",
    )
    .replace("SKYKEEP_SHOW", &show);
    for t in targets() {
        if t.action.as_deref().is_some_and(|a| is_deletion(&t.tool, a)) {
            out.push_str(&format!("  {}:\n    ask: {ask}\n", t.name));
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
    file.write_all(starter(mode, dir).as_bytes())
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
