//! Local MCP adapter. The library and codecs remain owned by the upstream crates.
pub mod account;
pub mod login;
pub mod scheduler;
pub mod state;
pub mod sync;
pub mod terminal;
use quince_core::Library;
use quince_sync::queue::ACCOUNT_KEY;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;

pub struct Server {
    /// `None` until the first operation that holds the private directory's lock.
    library: Option<Library>,
    dir: std::path::PathBuf,
    initialized: bool,
}

impl Server {
    /// Starts even while another process holds the directory lock, for example during its
    /// sync. Setup is then deferred, and each call reports `state_busy_retry` until the lock
    /// is free.
    pub fn open(dir: &Path) -> Result<Self, &'static str> {
        match state::PrivateState::open(dir) {
            Ok(held) => Ok(Self {
                library: Some(setup(&held.path)?),
                dir: held.path.clone(),
                initialized: false,
            }),
            Err("state_busy_retry") => Ok(Self {
                library: None,
                dir: std::path::absolute(dir).map_err(|_| "state_unavailable")?,
                initialized: false,
            }),
            Err(error) => Err(error),
        }
    }

    /// Caller holds the directory lock through `held`.
    fn ready(&mut self, held: &state::PrivateState) -> Result<&mut Library, &'static str> {
        if self.library.is_none() {
            self.library = Some(setup(&held.path)?);
            self.dir = held.path.clone();
        }
        self.library.as_mut().ok_or("state_unavailable")
    }

    pub async fn refresh(&mut self) -> Result<Value, &'static str> {
        self.call("sync", json!({})).await
    }

    pub async fn handle(&mut self, request: Value) -> Option<Value> {
        // Batches and other non-object messages are not supported; say so instead of dropping them.
        if !request.is_object() {
            return Some(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid Request: batches are not supported"}}),
            );
        }
        let id = request.get("id")?.clone();
        let result = match request.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                self.initialized = true;
                Ok(
                    json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"skykeep-mcp","version":env!("CARGO_PKG_VERSION")}}),
                )
            }
            Some("ping") => Ok(json!({})),
            Some("tools/list") if self.initialized => Ok(json!({"tools":tools()})),
            Some("tools/call") if self.initialized => {
                let params = request.get("params").cloned().unwrap_or(Value::Null);
                let result = self
                    .call(
                        params["name"].as_str().unwrap_or(""),
                        params.get("arguments").cloned().unwrap_or(json!({})),
                    )
                    .await;
                let (value, failed) = match result {
                    Ok(value) => (value, false),
                    Err(error) => (json!({"error":error}), true),
                };
                Ok(json!({"content":[{"type":"text","text":value.to_string()}],"isError":failed}))
            }
            _ => Err((-32601, "Method not found")),
        };
        Some(match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err((code, message)) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
            }
        })
    }

    /// Trusted local CLI only. No MCP method calls this function.
    pub fn approval_details(&self, id: &str) -> Result<String, &'static str> {
        uuid::Uuid::parse_str(id).map_err(|_| "invalid_request_id")?;
        self.library
            .as_ref()
            .ok_or("state_busy_retry")?
            .state(&format!("mcp.approval.{id}"))
            .map_err(|_| "library_unavailable")?
            .filter(|s| !s.is_empty())
            .ok_or("approval_not_found")
    }

    pub fn approve(&mut self, id: &str) -> Result<Value, &'static str> {
        let held = state::PrivateState::open(&self.dir)?;
        uuid::Uuid::parse_str(id).map_err(|_| "invalid_request_id")?;
        let key = format!("mcp.approval.{id}");
        self.ready(&held)?
            .atomically(|library| {
                let raw = library.state(&key)?.ok_or_else(|| {
                    quince_core::Error::Io(std::io::Error::other("approval_not_found"))
                })?;
                let value: Value = serde_json::from_str(&raw).map_err(|_| {
                    quince_core::Error::Io(std::io::Error::other("approval_not_found"))
                })?;
                if value["account"] != json!(library.state(ACCOUNT_KEY)?) {
                    return Err(quince_core::Error::Io(std::io::Error::other(
                        "account_mismatch",
                    )));
                }
                let result = match value["tool"].as_str() {
                    Some("notes_write") => serde_json::from_value(value["args"].clone())
                        .map_err(|_| "invalid_arguments")
                        .and_then(|args| enqueue_note(library, args)),
                    Some("reminders_write") => serde_json::from_value(value["args"].clone())
                        .map_err(|_| "invalid_arguments")
                        .and_then(|args| enqueue_reminder(library, args)),
                    _ => Err("invalid_tool"),
                }
                .map_err(|e| quince_core::Error::Io(std::io::Error::other(e)))?;
                library.set_state(&key, "")?;
                Ok(result)
            })
            .map_err(|_| "approval_refused")
    }

    async fn call(&mut self, name: &str, args: Value) -> Result<Value, &'static str> {
        let held = state::PrivateState::open(&self.dir)?;
        let dir = held.path.clone();
        let library = self.ready(&held)?;
        match name {
            "sync" => {
                let _: Empty = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                if library
                    .state(ACCOUNT_KEY)
                    .map_err(|_| "library_unavailable")?
                    .is_none()
                {
                    return Err("login_required");
                }
                sync::run(&dir).await
            }
            "sync_status" => {
                let _: Empty = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                let health = library.sync_health().map_err(|_| "library_unavailable")?;
                let refused = library
                    .outbox_refused()
                    .map_err(|_| "library_unavailable")?;
                let mut services = Vec::new();
                for service in ["notes", "reminders"] {
                    let h = health.iter().find(|h| h.service == service);
                    let pending = library
                        .outbox_pending(service)
                        .map_err(|_| "library_unavailable")?
                        .len();
                    // Upstream error text may hold HTTP bodies; report only that one exists.
                    services.push(
                        json!({"service":service,"last_synced_at":h.and_then(|h| h.last_synced_at),
                        "last_error":h.is_some_and(|h| h.last_error.is_some()),"pending":pending,
                        "refused":refused.iter().filter(|r| r.change.service == service).count()}),
                    );
                }
                Ok(json!({"source":"local_cache","services":services,"remote_verified":false}))
            }
            "auth_status" => {
                let _: Empty = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                let bound = library
                    .state(ACCOUNT_KEY)
                    .map_err(|_| "library_unavailable")?
                    .is_some();
                Ok(
                    json!({"status":if bound {"session_unverified"} else {"login_required"},"instructions":"Run skykeep-mcp login --state-dir PRIVATE_DIRECTORY in your own trusted terminal, outside agent/tool capture. Never send passwords or codes to MCP or chat.","session_checked":false}),
                )
            }
            "reminders_get" => {
                let args: Get = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                let item = library
                    .reminder(&args.guid)
                    .map_err(|_| "library_unavailable")?
                    .ok_or("not_found")?;
                let detail = library
                    .reminder_detail(&args.guid)
                    .map_err(|_| "library_unavailable")?;
                Ok(
                    json!({"source":"local_cache","item":item,"detail":detail,"complete_representation":false}),
                )
            }
            "reminders_lists" => {
                let args: Page = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                page(
                    library
                        .reminder_lists()
                        .map_err(|_| "library_unavailable")?,
                    args.into(),
                )
            }
            "notes_folders" => {
                let args: Page = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                page(
                    library.note_folders().map_err(|_| "library_unavailable")?,
                    args.into(),
                )
            }
            "notes_get" => {
                let args: Get = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                let mut item = library
                    .note(&args.guid)
                    .map_err(|_| "library_unavailable")?
                    .ok_or("not_found")?;
                item.body = library
                    .note_body(&args.guid)
                    .map_err(|_| "library_unavailable")?;
                item.runs = library
                    .note_runs(&args.guid)
                    .map_err(|_| "library_unavailable")?;
                let runs: Option<Value> = item
                    .runs
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()
                    .map_err(|_| "invalid_cached_runs")?;
                Ok(
                    json!({"source":"local_cache","item":item,"runs":runs,"complete_representation":false}),
                )
            }
            "reminders_search" => {
                let args: Search = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                page(
                    library
                        .search_reminders(&args.query)
                        .map_err(|_| "search_unavailable")?,
                    List {
                        offset: args.offset,
                        limit: args.limit,
                        parent: None,
                    },
                )
            }
            "notes_search" => {
                let args: Search = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                page(
                    library
                        .search_notes(&args.query)
                        .map_err(|_| "search_unavailable")?,
                    List {
                        offset: args.offset,
                        limit: args.limit,
                        parent: None,
                    },
                )
            }
            "notes_list" => {
                let args: List = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                let items = library
                    .notes_in(args.parent.as_deref())
                    .map_err(|_| "library_unavailable")?;
                page(items, args)
            }
            "reminders_list" => {
                let args: List = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                let parent = args.parent.as_deref().ok_or("parent_required")?;
                let items = library
                    .reminders_in(parent, true)
                    .map_err(|_| "library_unavailable")?;
                page(items, args)
            }
            "reminders_write" => {
                let args: ReminderWrite =
                    serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                if library
                    .state(ACCOUNT_KEY)
                    .map_err(|_| "library_unavailable")?
                    .is_none()
                {
                    return Err("login_required");
                }
                if matches!(args, ReminderWrite::Delete { .. })
                    || library
                        .state("mcp.require_write_approval")
                        .map_err(|_| "library_unavailable")?
                        .as_deref()
                        == Some("true")
                {
                    let id = uuid::Uuid::new_v4().to_string();
                    let proposal = json!({"tool":name,"args":args,"account":library.state(ACCOUNT_KEY).map_err(|_| "library_unavailable")?});
                    library
                        .set_state(&format!("mcp.approval.{id}"), &proposal.to_string())
                        .map_err(|_| "library_unavailable")?;
                    return Ok(
                        json!({"status":"approval_required","request_id":id,"remote_verified":false}),
                    );
                }
                enqueue_reminder(library, args)
            }
            "notes_write" => {
                let args: NoteWrite =
                    serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                if library
                    .state(ACCOUNT_KEY)
                    .map_err(|_| "library_unavailable")?
                    .is_none()
                {
                    return Err("login_required");
                }
                if args.is_destructive()
                    || library
                        .state("mcp.require_write_approval")
                        .map_err(|_| "library_unavailable")?
                        .as_deref()
                        == Some("true")
                {
                    let id = uuid::Uuid::new_v4().to_string();
                    let proposal = json!({"tool":name,"args":args,"account":library.state(ACCOUNT_KEY).map_err(|_| "library_unavailable")?});
                    library
                        .set_state(&format!("mcp.approval.{id}"), &proposal.to_string())
                        .map_err(|_| "library_unavailable")?;
                    return Ok(
                        json!({"status":"approval_required","request_id":id,"remote_verified":false}),
                    );
                }
                enqueue_note(library, args)
            }
            _ => Err("unknown_tool"),
        }
    }
}

/// One-time setup under the directory lock: persistent document writer and private modes.
fn setup(path: &Path) -> Result<Library, &'static str> {
    let mut library = Library::open(path).map_err(|_| "library_unavailable")?;
    let id = library
        .state("documents.writer")
        .map_err(|_| "library_unavailable")?
        .and_then(|id| uuid::Uuid::parse_str(&id).ok())
        .unwrap_or_else(uuid::Uuid::new_v4);
    library
        .set_state("documents.writer", &id.to_string())
        .map_err(|_| "library_unavailable")?;
    quince_icloud::topotext::set_writer(*id.as_bytes());
    use std::os::unix::fs::PermissionsExt;
    for file in std::fs::read_dir(path).map_err(|_| "state_unavailable")? {
        let file = file.map_err(|_| "state_unavailable")?;
        if file.file_type().map_err(|_| "state_unavailable")?.is_file() {
            std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600))
                .map_err(|_| "state_unavailable")?;
        }
    }
    Ok(library)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Get {
    guid: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    query: String,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct List {
    parent: Option<String>,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
}

/// Arguments of list tools that have no parent.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
}

impl From<Page> for List {
    fn from(p: Page) -> Self {
        List {
            parent: None,
            offset: p.offset,
            limit: p.limit,
        }
    }
}

fn page<T: serde::Serialize>(items: Vec<T>, args: List) -> Result<Value, &'static str> {
    let total = items.len();
    let limit = args.limit.unwrap_or(100).clamp(1, 1000);
    let items: Vec<_> = items.into_iter().skip(args.offset).take(limit).collect();
    let next = args.offset.saturating_add(items.len());
    Ok(
        json!({"source":"local_cache","items":items,"total":total,"next_offset":(next < total).then_some(next),"complete_representation":false}),
    )
}

#[derive(Deserialize, serde::Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum NoteWrite {
    Move {
        guid: String,
        folder_guid: Option<String>,
        base_etag: Option<String>,
    },
    Pin {
        guid: String,
        pinned: bool,
        base_etag: Option<String>,
    },
    Save {
        guid: String,
        body: String,
        base_etag: Option<String>,
        runs: Option<Vec<quince_icloud::ckdocument::Run>>,
    },
    Create {
        body: String,
        folder_guid: Option<String>,
    },
    Delete {
        guid: String,
        base_etag: Option<String>,
    },
}

impl NoteWrite {
    /// Writes that remove content need terminal approval. Upstream deletes a note by moving
    /// it into the trash folder, so such a move is a deletion; so is erasing the whole body.
    fn is_destructive(&self) -> bool {
        match self {
            NoteWrite::Delete { .. } => true,
            NoteWrite::Save { body, .. } => body.trim().is_empty(),
            NoteWrite::Move { folder_guid, .. } => {
                folder_guid.as_deref() == Some(quince_core::notes::TRASH_FOLDER)
            }
            _ => false,
        }
    }
}

/// `None` files a note in the default folder; a named folder must be in the synced cache.
fn require_folder(library: &Library, folder: Option<&str>) -> Result<(), &'static str> {
    let Some(folder) = folder else { return Ok(()) };
    if library
        .note_folders()
        .map_err(|_| "library_unavailable")?
        .iter()
        .any(|f| f.guid == folder)
    {
        Ok(())
    } else {
        Err("folder_not_found")
    }
}

fn enqueue_note(library: &mut Library, args: NoteWrite) -> Result<Value, &'static str> {
    match args {
        NoteWrite::Move {
            guid,
            folder_guid,
            base_etag,
        } => {
            let note = library
                .note(&guid)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            if note.etag != base_etag {
                return Err("conflict");
            }
            if note.locked || !note.editable {
                return Err("read_only_note");
            }
            require_folder(library, folder_guid.as_deref())?;
            quince_sync::notes::record_note_move(library, &guid, folder_guid)
                .map_err(|_| "write_refused")?;
            Ok(json!({"guid":guid,"status":"queued","remote_verified":false}))
        }
        NoteWrite::Pin {
            guid,
            pinned,
            base_etag,
        } => {
            let note = library
                .note(&guid)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            if note.etag != base_etag {
                return Err("conflict");
            }
            if note.locked || !note.editable {
                return Err("read_only_note");
            }
            quince_sync::notes::record_note_pin(library, &guid, pinned)
                .map_err(|_| "write_refused")?;
            Ok(json!({"guid":guid,"status":"queued","remote_verified":false}))
        }
        NoteWrite::Save {
            guid,
            body,
            base_etag,
            runs,
        } => {
            let note = library
                .note(&guid)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            if note.etag != base_etag {
                return Err("conflict");
            }
            if note.locked || !note.editable {
                return Err("read_only_note");
            }
            let base = library
                .note_body(&guid)
                .map_err(|_| "library_unavailable")?
                .ok_or("note_body_unavailable_sync_first")?;
            let base_runs = library
                .note_runs(&guid)
                .map_err(|_| "library_unavailable")?
                .map(|raw| serde_json::from_str(&raw).map_err(|_| "invalid_cached_runs"))
                .transpose()?;
            quince_sync::notes::record_note_save(
                library,
                &guid,
                &body,
                Some(base),
                runs,
                base_runs,
            )
            .map_err(|_| "write_refused")?;
            Ok(json!({"guid":guid,"status":"queued","remote_verified":false}))
        }
        NoteWrite::Delete { guid, base_etag } => {
            let note = library
                .note(&guid)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            if note.etag != base_etag {
                return Err("conflict");
            }
            if note.locked || !note.editable {
                return Err("read_only_note");
            }
            quince_sync::notes::record_note_delete(library, &note).map_err(|_| "write_refused")?;
            Ok(json!({"guid":guid,"status":"queued","remote_verified":false}))
        }
        NoteWrite::Create { body, folder_guid } => {
            if folder_guid.as_deref() == Some(quince_core::notes::TRASH_FOLDER) {
                return Err("folder_not_writable");
            }
            require_folder(library, folder_guid.as_deref())?;
            let guid = quince_sync::notes::record_note_create(library, folder_guid, &body)
                .map_err(|_| "write_refused")?;
            Ok(json!({"guid":guid,"status":"queued","remote_verified":false}))
        }
    }
}

#[derive(Deserialize, serde::Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum ReminderWrite {
    Delete {
        guid: String,
    },
    Tick {
        guid: String,
        done: bool,
    },
    Edit {
        guid: String,
        patch: Value,
    },
    Create {
        list_guid: String,
        title: String,
        due_at: Option<i64>,
        all_day: Option<bool>,
        flagged: Option<bool>,
    },
}
fn enqueue_reminder(library: &mut Library, args: ReminderWrite) -> Result<Value, &'static str> {
    match args {
        ReminderWrite::Delete { guid } => {
            let reminder = library
                .reminder(&guid)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            quince_sync::reminders::record_reminder_delete(library, &reminder)
                .map_err(|_| "write_refused")?;
            Ok(json!({"guid":guid,"status":"queued","remote_verified":false}))
        }
        ReminderWrite::Tick { guid, done } => {
            let reminder = library
                .reminder(&guid)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            quince_sync::reminders::record_reminder_tick(
                library,
                &reminder,
                done,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| "clock_unavailable")?
                    .as_secs() as i64,
            )
            .map_err(|_| "write_refused")?;
            Ok(json!({"guid":guid,"status":"queued","remote_verified":false}))
        }
        ReminderWrite::Edit { guid, patch } => {
            let reminder = library
                .reminder(&guid)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            let mut fields = patch.as_object().cloned().ok_or("invalid_patch")?;
            if fields.is_empty()
                || fields.keys().any(|k| {
                    ![
                        "title",
                        "notes",
                        "due_at",
                        "all_day",
                        "flagged",
                        "completed",
                        "priority",
                        "list_guid",
                        "parent_guid",
                    ]
                    .contains(&k.as_str())
                })
            {
                return Err("invalid_patch");
            }
            fields.insert("from_list".into(), json!(reminder.list_guid));
            let e: quince_sync::reminders::Edit =
                serde_json::from_value(Value::Object(fields)).map_err(|_| "invalid_patch")?;
            let edit = quince_icloud::reminders::ReminderEdit {
                title: e.title,
                notes: e.notes,
                due_at: e.due_at,
                all_day: e.all_day,
                flagged: e.flagged,
                completed: e.completed,
                priority: e.priority,
                list_guid: e.list_guid,
                parent_guid: e.parent_guid,
            };
            quince_sync::reminders::record_reminder_edit(library, &reminder, &edit)
                .map_err(|_| "write_refused")?;
            Ok(json!({"guid":guid,"status":"queued","remote_verified":false}))
        }
        ReminderWrite::Create {
            list_guid,
            title,
            due_at,
            all_day,
            flagged,
        } => {
            if !library
                .reminder_lists()
                .map_err(|_| "library_unavailable")?
                .iter()
                .any(|l| l.guid == list_guid)
            {
                return Err("list_unavailable_or_read_only");
            }
            if title.trim().is_empty() {
                return Err("title_required");
            }
            let guid = quince_sync::reminders::record_reminder_create(
                library,
                &quince_sync::reminders::Create {
                    list_guid,
                    title,
                    due_at,
                    all_day,
                    flagged,
                },
            )
            .map_err(|_| "write_refused")?;
            Ok(json!({"guid":guid,"status":"queued","remote_verified":false}))
        }
    }
}

fn tools() -> Vec<Value> {
    // (name, parent property, parent required)
    let lists = [
        (
            "notes_list",
            Some("Folder GUID; omit for all notes."),
            false,
        ),
        ("reminders_list", Some("Reminder list GUID."), true),
        ("notes_folders", None, false),
        ("reminders_lists", None, false),
    ];
    let mut tools: Vec<_> = lists.iter().map(|(name, parent, required)| {
        let mut properties = json!({"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":1000}});
        if let Some(description) = parent {
            properties["parent"] = json!({"type":"string","description":description});
        }
        let mut schema = json!({"type":"object","properties":properties,"additionalProperties":false});
        if *required {
            schema["required"] = json!(["parent"]);
        }
        json!({
            "name":name,"description":"Read locally cached items. Sync requires an authenticated session.",
            "inputSchema":schema,
            "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}
        })
    }).collect();
    for name in ["notes_search", "reminders_search"] {
        tools.push(json!({"name":name,"description":"Search cached items through the upstream local index.","inputSchema":{"type":"object","properties":{"query":{"type":"string"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":1000}},"required":["query"],"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}));
    }
    tools.push(json!({"name":"notes_write","description":"Queue a supported native Notes mutation. Queued is not remote-verified.","inputSchema":{"type":"object","properties":{"action":{"enum":["create","save","move","pin","delete"]},"body":{"type":"string"},"pinned":{"type":"boolean"},"runs":{"type":["array","null"],"items":{"type":"object"}},"folder_guid":{"type":["string","null"]},"guid":{"type":"string"},"base_etag":{"type":["string","null"]}},"required":["action"],"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":true,"openWorldHint":true}}));
    tools.push(json!({"name":"reminders_write","description":"Queue native reminder create/edit/tick/delete. Deletions need local terminal approval. No field-level remote verification.","inputSchema":{"type":"object","properties":{"action":{"enum":["create","edit","tick","delete"]},"guid":{"type":"string"},"list_guid":{"type":"string"},"title":{"type":"string"},"due_at":{"type":["integer","null"]},"all_day":{"type":"boolean"},"flagged":{"type":"boolean"},"done":{"type":"boolean"},"patch":{"type":"object","description":"Supported fields: title, notes, due_at, all_day, flagged, completed, priority, list_guid, parent_guid. Absent leaves unchanged; null clears notes/due_at/parent_guid."}},"required":["action"],"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":true,"openWorldHint":true}}));
    tools.push(json!({"name":"reminders_get","description":"Read a cached reminder and supported native details. Not a complete archive.","inputSchema":{"type":"object","properties":{"guid":{"type":"string"}},"required":["guid"],"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}));
    tools.push(json!({"name":"notes_get","description":"Read a cached native note. Not a complete preservation archive.","inputSchema":{"type":"object","properties":{"guid":{"type":"string"}},"required":["guid"],"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}));
    tools.push(json!({"name":"sync","description":"Refresh Notes and Reminders and send approved queued changes. Queued/sent is not field-level remote verification. Never opens a login prompt.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":true,"openWorldHint":true}}));
    tools.push(json!({"name":"sync_status","description":"Local sync state: last refresh, whether the last pass failed, pending and refused queued changes. No network access.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}));
    tools.push(json!({"name":"auth_status","description":"Authentication setup instructions. Never accepts secrets or opens a prompt.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}));
    tools
}
