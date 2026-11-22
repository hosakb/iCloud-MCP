//! `calendar_write`: changes to Apple iCloud Calendar, one action per call: `create`, `update`,
//! `move` (to another calendar) and `delete`.
//!
//! Like Drive, Calendar has no durable outbox here. An action runs as soon as it is allowed,
//! and the reply says what happened: done and read back from Apple (`remote_verified: true`),
//! done but not read back, failed, or unknown when the connection ended before Apple's answer
//! was read. Nothing is retried on its own.
//!
//! Versions. update, move and delete name the event's `etag` as `calendar_get` shows it. The
//! call is refused when the cache holds another version, and again when Apple's copy is not
//! that version any more, so a change made on another device is never written over. Between
//! that last read and the save, the guard is Apple's own check of the etag sent in the save's
//! body (a 409 or 412 answer); that Apple enforces it there is not confirmed against Apple.
//! The attendee check below is a separate read before the save, so a guest added on another
//! device between that read and the save could still be sent an update.
//!
//! Repeating events. One occurrence (an id `<series>__<start>`) is changed only with `scope`
//! said: `this_occurrence` or `whole_series`. There is no default. An event that repeats but
//! is cached under its series id is checked at Apple before the change, and refused without
//! `whole_series`.
//!
//! Times need an explicit zone: an offset (`2026-03-29T09:30:00+02:00`, `...Z`) or a local
//! time with `timezone` (an IANA name such as Europe/Berlin). A new event given only offset
//! times, with no `timezone`, is kept in UTC. A local time the clocks skip
//! is refused, and so is one they pass twice unless an offset says which. All-day events take
//! dates, the end being the day after the last.
//!
//! Attendees. Skykeep never adds or removes attendees. Before an update, move or delete it
//! reads the event's attendees from Apple. When other people are invited, Apple would email
//! them the change or a cancellation, so the action is refused unless `permissions.yaml` sets
//! `calendar_write.send_invitations: allow`. Only `allow` lets it run, and a broken file
//! with no earlier valid rules refuses it.
//!
//! Who approves. A delete (`permissions::removal_of` decides it, as for every tool) always
//! needs the person's approval, on the card (or with `skykeep-mcp approve` under an
//! `ask: terminal` rule), and the card never
//! offers "Always allow" for it. Other actions follow the approval mode and the rules.
use crate::account::{binding, Binding};
use crate::calendar;
use crate::drive_write::{shown, MakeClient};
use crate::exchange::hides_text;
use quince_auth::{Client, Endpoints};
use quince_core::calendar::{floats, instant_of, wall_clock, Calendar, Event};
use quince_core::Library;
use quince_icloud::calendar::{is_occurrence, series_of, CalendarClient, Scope};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const TOOL: &str = "calendar_write";
/// The actions, as the tool's schema names them.
pub const ACTIONS: [&str; 4] = ["create", "update", "move", "delete"];
/// The rule name that lets a change reach other people's calendars. Not an action.
pub const INVITATIONS: &str = "send_invitations";
pub const INVITATIONS_RULE: &str = "calendar_write.send_invitations";

/// The Calendar actions that remove something, as the permissions rules decide it for every
/// tool (`permissions::removal_of`): `delete`.
pub fn is_destructive(action: &str) -> bool {
    crate::permissions::is_deletion(TOOL, action)
}

pub const UNKNOWN: &str = "calendar_write_outcome_unknown_run_calendar_refresh_before_retrying";
pub const CHANGED: &str = "calendar_event_changed_run_calendar_refresh";
pub const ATTENDEES: &str = "calendar_event_has_other_attendees_refused";
pub const SCOPE_REQUIRED: &str = "scope_required_for_repeating_event";
pub const INVALID_START: &str = "invalid_start";
pub const INVALID_END: &str = "invalid_end";
const NOT_CONFIRMED: &str = "calendar_write_not_confirmed_run_calendar_refresh";
const READ_FAILED: &str = "calendar_read_failed_nothing_sent";
const GONE: &str = "not_found_at_apple_run_calendar_refresh";
const SECOND_PASS: &str = "time_in_the_repeated_hour_apple_keeps_only_the_first";
const TIMEZONE_REQUIRED: &str = "timezone_required";
const START_REQUIRED: &str = "start_required";
const MOVE_SCOPE: &str = "move_needs_scope_whole_series";
const NOT_REPEATING: &str = "not_a_repeating_event_leave_out_scope";
const EVENT_NOT_FOUND: &str = "event_not_found_run_calendar_refresh";
/// Characters of the notes a preview shows.
const NOTES_SHOWN: usize = 200;
const FLOATING: &str = "floating_event_needs_timezone";
const CALENDAR_CHANGED: &str = "calendar_changed_run_calendar_refresh_and_try_again";
const UNKNOWN_ACTION: &str = "calendar_write_unknown_action";
const NOTES_TOO_LONG: &str = "calendar_notes_too_long_for_the_terminal_change_it_in_apple_calendar";
const UNKNOWN_FIELD: &str = "calendar_write_unknown_field";
/// Every field the tool takes.
const FIELDS: [&str; 14] = [
    "action",
    "guid",
    "etag",
    "calendar_guid",
    "title",
    "location",
    "notes",
    "url",
    "start",
    "end",
    "all_day",
    "timezone",
    "alerts",
    "scope",
];

const MAX_TITLE_BYTES: usize = 1024;
const MAX_LOCATION_BYTES: usize = 1024;
const MAX_NOTES_BYTES: usize = 16_384;
const MAX_URL_BYTES: usize = 2048;
const MAX_ETAG_BYTES: usize = 256;
const MAX_TIME_BYTES: usize = 40;
const MAX_ALERTS: usize = 8;
/// Four weeks, in minutes.
const MAX_ALERT_MINUTES: i64 = 40_320;
/// The longest event, in days.
const MAX_EVENT_DAYS: i64 = 366;
const DAY: i64 = 86_400;
const SHOWN_CHARS: usize = 255;

/// The accepted forms, said in every date error.
pub const TIME_FORMAT: &str = "Give a time with its offset, such as 2026-03-29T09:30:00+02:00 or 2026-03-29T07:30:00Z, or a local time such as 2026-03-29T09:30:00 together with timezone as an IANA name such as Europe/Berlin. Minutes are the smallest unit. For an all-day event (all_day: true) give dates such as 2026-03-29; its end is the day after the last day.";

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    Create,
    Update,
    Move,
    Delete,
}

/// Which events a change to a repeating event is for.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Repeat {
    ThisOccurrence,
    WholeSeries,
}

impl Repeat {
    fn scope(self) -> Scope {
        match self {
            Repeat::ThisOccurrence => Scope::This,
            Repeat::WholeSeries => Scope::All,
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Repeat::ThisOccurrence => "this_occurrence",
            Repeat::WholeSeries => "whole_series",
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CalendarWrite {
    action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    guid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    calendar_guid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    end: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    all_day: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alerts: Option<Vec<i64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<Repeat>,
}

impl CalendarWrite {
    pub(crate) fn action(&self) -> &'static str {
        match self.action {
            Action::Create => "create",
            Action::Update => "update",
            Action::Move => "move",
            Action::Delete => "delete",
        }
    }

    fn changes_fields(&self) -> bool {
        self.title.is_some()
            || self.location.is_some()
            || self.notes.is_some()
            || self.url.is_some()
            || self.changes_time()
            || self.alerts.is_some()
    }

    fn changes_time(&self) -> bool {
        self.start.is_some()
            || self.end.is_some()
            || self.all_day.is_some()
            || self.timezone.is_some()
    }
}

// --- Arguments, checked before any approval prompt, cache or network use.

fn text_ok(text: &str, max: usize, lines: bool) -> bool {
    text.len() <= max
        && !text.chars().any(|c| {
            hides_text(c)
                || matches!(c, '\u{2028}' | '\u{2029}')
                || (c.is_control() && !(lines && (c == '\n' || c == '\t')))
        })
}

fn id_ok(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && text.chars().all(|c| c.is_ascii_graphic())
}

/// A time as given: a day, an instant (it had an offset), or a wall-clock time to be placed
/// in the event's zone, counted in seconds as if it were UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum When {
    Day(i64),
    Instant(i64),
    Local(i64),
}

/// `YYYY-MM-DD` for an all-day event; `YYYY-MM-DDTHH:MM[:00]` with an optional `Z` or
/// `±HH:MM` for a timed one.
fn parse_when(text: &str, all_day: bool) -> Option<When> {
    if text.len() > MAX_TIME_BYTES || !text.is_ascii() {
        return None;
    }
    if all_day {
        return calendar::parse_day(text).ok().map(When::Day);
    }
    let day = calendar::parse_day(text.get(..10)?).ok()?;
    let rest = text.get(10..)?;
    let rest = rest.strip_prefix('T')?;
    let num = |s: &str| -> Option<i64> {
        (s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok())?
    };
    let (h, m) = (num(rest.get(0..2)?)?, num(rest.get(3..5)?)?);
    if rest.as_bytes().get(2) != Some(&b':') || h > 23 || m > 59 {
        return None;
    }
    let mut tail = rest.get(5..)?;
    if let Some(after) = tail.strip_prefix(':') {
        if after.get(..2)? != "00" {
            return None;
        }
        tail = after.get(2..)?;
    }
    let wall = day * DAY + h * 3600 + m * 60;
    match tail {
        "" => Some(When::Local(wall)),
        "Z" => Some(When::Instant(wall)),
        _ => {
            let sign = match tail.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if tail.len() != 6 || tail.as_bytes()[3] != b':' {
                return None;
            }
            let (oh, om) = (num(&tail[1..3])?, num(&tail[4..6])?);
            if oh > 18 || om > 59 {
                return None;
            }
            Some(When::Instant(wall - sign * (oh * 3600 + om * 60)))
        }
    }
}

/// Checks the arguments before any approval prompt, cache or network use.
pub(crate) fn validate(args: &Value) -> Result<CalendarWrite, &'static str> {
    if !args["action"]
        .as_str()
        .is_some_and(|a| ACTIONS.contains(&a))
    {
        return Err(UNKNOWN_ACTION);
    }
    if args
        .as_object()
        .is_some_and(|m| m.keys().any(|k| !FIELDS.contains(&k.as_str())))
    {
        return Err(UNKNOWN_FIELD);
    }
    let op: CalendarWrite =
        serde_json::from_value(args.clone()).map_err(|_| "invalid_arguments")?;
    let guid_ok = |g: &Option<String>| {
        g.as_deref()
            .is_some_and(|g| id_ok(g, calendar::MAX_GUID_BYTES))
    };
    if op.guid.is_some() && !guid_ok(&op.guid)
        || op.calendar_guid.is_some() && !guid_ok(&op.calendar_guid)
    {
        return Err("invalid_arguments");
    }
    if op
        .etag
        .as_deref()
        .is_some_and(|e| !id_ok(e, MAX_ETAG_BYTES))
    {
        return Err("invalid_arguments");
    }
    match op.action {
        Action::Create => {
            if op.guid.is_some() || op.etag.is_some() || op.scope.is_some() {
                return Err("create_takes_no_guid_etag_or_scope");
            }
            if op.calendar_guid.is_none() {
                return Err("calendar_guid_required");
            }
            if op.title.is_none() {
                return Err("title_required");
            }
            if op.start.is_none() {
                return Err(START_REQUIRED);
            }
        }
        Action::Update | Action::Move | Action::Delete => {
            if op.guid.is_none() {
                return Err("guid_required");
            }
            if op.etag.is_none() {
                return Err("etag_required_from_calendar_get");
            }
            let guid = op.guid.as_deref().unwrap_or_default();
            if op.scope == Some(Repeat::ThisOccurrence) && !is_occurrence(guid) {
                return Err("this_occurrence_needs_an_occurrence_guid");
            }
            match op.action {
                Action::Update => {
                    if op.calendar_guid.is_some() {
                        return Err("invalid_arguments_use_move_for_another_calendar");
                    }
                    if !op.changes_fields() {
                        return Err("nothing_to_change");
                    }
                }
                Action::Move => {
                    if op.calendar_guid.is_none() {
                        return Err("calendar_guid_required");
                    }
                    if op.changes_fields() {
                        return Err("invalid_arguments_move_changes_only_the_calendar");
                    }
                    if op.scope == Some(Repeat::ThisOccurrence)
                        || (is_occurrence(guid) && op.scope.is_none())
                    {
                        return Err(MOVE_SCOPE);
                    }
                }
                _ => {
                    if op.calendar_guid.is_some() || op.changes_fields() {
                        return Err("delete_takes_only_guid_etag_and_scope");
                    }
                }
            }
            if is_occurrence(guid) && op.scope.is_none() {
                return Err(SCOPE_REQUIRED);
            }
        }
    }
    if op
        .title
        .as_deref()
        .is_some_and(|t| t.trim().is_empty() || !text_ok(t, MAX_TITLE_BYTES, false))
    {
        return Err("invalid_title");
    }
    if op
        .location
        .as_deref()
        .is_some_and(|t| !text_ok(t, MAX_LOCATION_BYTES, false))
        || op
            .notes
            .as_deref()
            .is_some_and(|t| !text_ok(t, MAX_NOTES_BYTES, true))
        || op
            .url
            .as_deref()
            .is_some_and(|t| !text_ok(t, MAX_URL_BYTES, false) || t.contains(' '))
    {
        return Err("invalid_arguments");
    }
    if let Some(zone) = &op.timezone {
        if zone.is_empty() || zone.len() > calendar::MAX_ZONE_BYTES {
            return Err("unknown_timezone");
        }
        calendar::zone(zone).ok_or("unknown_timezone")?;
    }
    if let Some(alerts) = &op.alerts {
        let mut seen = alerts.clone();
        seen.sort_unstable();
        seen.dedup();
        if alerts.len() > MAX_ALERTS
            || seen.len() != alerts.len()
            || alerts.iter().any(|m| !(0..=MAX_ALERT_MINUTES).contains(m))
        {
            return Err("invalid_alerts");
        }
    }
    // The forms of the times; the cache says the rest for an update.
    if op.action == Action::Create {
        let all_day = op.all_day.unwrap_or(false);
        times_for(&op, all_day, None)?;
    }
    Ok(op)
}

/// Whether this action needs the person's approval whatever the rules and the mode say.
pub(crate) fn needs_person(args: &Value) -> bool {
    validate(args).is_ok_and(|op| is_destructive(op.action()))
}

// --- Times.

fn system_zone() -> quince_icloud::zones::Zone {
    let tz = jiff::tz::TimeZone::system();
    quince_icloud::zones::Zone {
        name: tz.iana_name().map(str::to_string),
        tz,
    }
}

/// The instant a wall-clock time is in `zone`; a time the clocks skip, or pass twice, is
/// refused.
fn placed(wall: i64, zone: &quince_icloud::zones::Zone) -> Result<i64, &'static str> {
    use jiff::tz::AmbiguousOffset;
    let dt = jiff::Timestamp::from_second(wall)
        .map_err(|_| INVALID_START)?
        .to_zoned(jiff::tz::TimeZone::UTC)
        .datetime();
    match zone.tz.to_ambiguous_timestamp(dt).offset() {
        AmbiguousOffset::Unambiguous { offset } => Ok(wall - i64::from(offset.seconds())),
        AmbiguousOffset::Gap { .. } => Err("time_does_not_exist_in_timezone_clocks_skip_it"),
        AmbiguousOffset::Fold { .. } => Err("time_is_ambiguous_in_timezone_give_an_offset"),
    }
}

/// An event's new times: `(starts_at, ends_at, timezone)` as the engine takes them. A timed
/// event is two instants and its zone's name; an all-day one is its days at this computer's
/// midnight (as the engine writes and reads them) with no zone. `before` is the cached
/// event an update starts from.
fn times_for(
    op: &CalendarWrite,
    all_day: bool,
    before: Option<&Event>,
) -> Result<(i64, i64, Option<String>), &'static str> {
    let start = op
        .start
        .as_deref()
        .map(|s| parse_when(s, all_day).ok_or(INVALID_START))
        .transpose()?;
    let end = op
        .end
        .as_deref()
        .map(|s| parse_when(s, all_day).ok_or(INVALID_END))
        .transpose()?;
    let local = jiff::tz::TimeZone::system();
    if all_day {
        let day_of = |t: i64| (wall_clock(t, &local) + DAY / 2).div_euclid(DAY);
        let (first, last) = match (start, end, before) {
            (Some(When::Day(s)), Some(When::Day(e)), _) => (s, e),
            (Some(When::Day(s)), None, Some(b)) if b.all_day => {
                (s, s + (day_of(b.ends_at) - day_of(b.starts_at)).max(1))
            }
            (Some(When::Day(s)), None, _) => (s, s + 1),
            (None, Some(When::Day(e)), Some(b)) if b.all_day => (day_of(b.starts_at), e),
            _ => return Err(START_REQUIRED),
        };
        if last <= first {
            return Err("end_must_be_after_start");
        }
        if last - first > MAX_EVENT_DAYS {
            return Err("event_too_long");
        }
        return Ok((
            instant_of(first * DAY, &local),
            instant_of(last * DAY, &local),
            None,
        ));
    }
    // Offsets alone fix the instants: with no zone given or held, the event is kept in UTC.
    let offsets_only = [start, end]
        .iter()
        .all(|w| !matches!(w, Some(When::Local(_))));
    let zone_name = match (&op.timezone, before) {
        (Some(z), _) => Some(z.clone()),
        (None, Some(b)) if !b.all_day && b.timezone.is_some() => b.timezone.clone(),
        // An event without a zone is never pinned to one silently.
        (None, Some(_)) => return Err(FLOATING),
        _ if offsets_only => Some("UTC".to_string()),
        _ => None,
    };
    let zone = match &zone_name {
        Some(name) => Some(calendar::zone(name).ok_or("unknown_timezone")?),
        None => None,
    };
    let instant = |w: When, bad: &'static str| -> Result<i64, &'static str> {
        match w {
            When::Instant(at) => Ok(at),
            When::Local(wall) => placed(wall, zone.as_ref().ok_or(TIMEZONE_REQUIRED)?)
                .map_err(|e| if e == INVALID_START { bad } else { e }),
            When::Day(_) => Err(bad),
        }
    };
    let timed_before = before.filter(|b| !b.all_day);
    let (starts, ends) = match (start, end, timed_before) {
        (Some(s), Some(e), _) => (instant(s, INVALID_START)?, instant(e, INVALID_END)?),
        (Some(s), None, Some(b)) => {
            let s = instant(s, INVALID_START)?;
            (s, s + (b.ends_at - b.starts_at).max(0))
        }
        (Some(s), None, None) => {
            let s = instant(s, INVALID_START)?;
            (s, s + 3600)
        }
        (None, Some(e), Some(b)) => (b.starts_at, instant(e, INVALID_END)?),
        _ => return Err(START_REQUIRED),
    };
    if ends < starts {
        return Err("end_must_be_after_start");
    }
    if ends - starts > MAX_EVENT_DAYS * DAY {
        return Err("event_too_long");
    }
    // Apple keeps a time as the clock reads in the zone. In the hour the clocks pass twice
    // that reading is the first one, so the second can't be kept: refused, not shifted.
    if let Some(zone) = &zone {
        for at in [starts, ends] {
            if instant_of(wall_clock(at, &zone.tz), &zone.tz) != at {
                return Err(SECOND_PASS);
            }
        }
    }
    // A timed event is written with its zone: without one Apple would keep it as floating.
    let name = zone_name.ok_or(TIMEZONE_REQUIRED)?;
    let name = zone.and_then(|z| z.name).unwrap_or(name);
    Ok((starts, ends, Some(name)))
}

// --- The cache.

struct Plan {
    /// The calendar the event is in, or goes into.
    calendar: Calendar,
    /// A move's calendar of origin.
    from: Option<Calendar>,
    /// The event as cached, its times as the engine takes them.
    before: Option<Event>,
    /// The event as it should end up (no id yet for a create).
    after: Option<Event>,
}

fn writable(calendars: &[Calendar], guid: &str) -> Result<Calendar, &'static str> {
    let calendar = calendars
        .iter()
        .find(|c| c.guid == guid)
        .ok_or("calendar_not_found")?;
    if !calendar.writable {
        return Err("calendar_read_only");
    }
    Ok(calendar.clone())
}

/// Whether an update leaves every field it can change as it was.
fn same_fields(a: &Event, b: &Event) -> bool {
    a.title == b.title
        && a.location == b.location
        && a.notes == b.notes
        && a.url == b.url
        && a.starts_at == b.starts_at
        && a.ends_at == b.ends_at
        && a.all_day == b.all_day
        && a.timezone == b.timezone
}

fn changed_text(new: &Option<String>, old: &Option<String>) -> Option<String> {
    match new {
        Some(t) if t.is_empty() => None,
        Some(t) => Some(t.clone()),
        None => old.clone(),
    }
}

fn plan(library: &Library, op: &CalendarWrite) -> Result<Plan, &'static str> {
    let calendars: Vec<Calendar> = library
        .calendars()
        .map_err(|_| "library_unavailable")?
        .into_iter()
        .filter(|c| calendar::icloud(&c.guid))
        .collect();
    if op.action == Action::Create {
        let calendar = writable(&calendars, op.calendar_guid.as_deref().unwrap_or_default())?;
        let all_day = op.all_day.unwrap_or(false);
        let (starts_at, ends_at, timezone) = times_for(op, all_day, None)?;
        let after = Event {
            guid: String::new(),
            calendar_guid: calendar.guid.clone(),
            title: op.title.clone().unwrap_or_default(),
            location: changed_text(&op.location, &None),
            starts_at,
            ends_at,
            all_day,
            timezone,
            url: changed_text(&op.url, &None),
            notes: changed_text(&op.notes, &None),
            etag: None,
        };
        return Ok(Plan {
            calendar,
            from: None,
            before: None,
            after: Some(after),
        });
    }
    let guid = op.guid.as_deref().unwrap_or_default();
    let before = library
        .event(guid)
        .map_err(|_| "library_unavailable")?
        .ok_or(EVENT_NOT_FOUND)?;
    if !calendars.iter().any(|c| c.guid == before.calendar_guid) {
        return Err(EVENT_NOT_FOUND);
    }
    let calendar = writable(&calendars, &before.calendar_guid)?;
    match before.etag.as_deref() {
        None => return Err("event_version_unknown_run_calendar_refresh"),
        Some(cached) if Some(cached) != op.etag.as_deref() => {
            return Err("etag_not_current_run_calendar_get")
        }
        _ => {}
    }
    let after = match op.action {
        Action::Update => {
            let all_day = op.all_day.unwrap_or(before.all_day);
            if all_day != before.all_day && op.start.is_none() {
                return Err("start_required_to_change_all_day");
            }
            // A zone the same as the event's, or all_day as it is, changes nothing.
            let canonical = |z: &str| {
                calendar::zone(z)
                    .and_then(|z| z.name)
                    .unwrap_or_else(|| z.to_string())
            };
            let zone_changes = !all_day
                && op.timezone.as_deref().is_some_and(|z| {
                    Some(canonical(z)) != before.timezone.as_deref().map(canonical)
                });
            if zone_changes && op.start.is_none() && op.end.is_none() {
                return Err("give_start_to_change_the_timezone");
            }
            let time_changes =
                op.start.is_some() || op.end.is_some() || all_day != before.all_day || zone_changes;
            let (starts_at, ends_at, timezone) = if time_changes {
                times_for(op, all_day, Some(&before))?
            } else {
                (before.starts_at, before.ends_at, before.timezone.clone())
            };
            Some(Event {
                guid: before.guid.clone(),
                calendar_guid: before.calendar_guid.clone(),
                title: op.title.clone().unwrap_or_else(|| before.title.clone()),
                location: changed_text(&op.location, &before.location),
                starts_at,
                ends_at,
                all_day,
                timezone,
                url: changed_text(&op.url, &before.url),
                notes: changed_text(&op.notes, &before.notes),
                etag: before.etag.clone(),
            })
            .filter(|after| !same_fields(after, &before) || op.alerts.is_some())
            .map(Some)
            .ok_or("nothing_to_change")?
        }
        Action::Move => {
            let destination =
                writable(&calendars, op.calendar_guid.as_deref().unwrap_or_default())?;
            if destination.guid == before.calendar_guid {
                return Err("already_in_this_calendar");
            }
            let mut after = before.clone();
            after.calendar_guid = destination.guid.clone();
            return Ok(Plan {
                calendar: destination,
                from: Some(calendar),
                before: Some(before),
                after: Some(after),
            });
        }
        _ => None,
    };
    Ok(Plan {
        calendar,
        from: None,
        before: Some(before),
        after,
    })
}

/// An event's times as shown: dates for an all-day event, UTC and the event's own zone for a
/// timed one, the local clock time for a floating one.
fn when_view(e: &Event) -> Value {
    let local = jiff::tz::TimeZone::system();
    if e.all_day {
        let day = |t: i64| (wall_clock(t, &local) + DAY / 2).div_euclid(DAY);
        return json!({"all_day":true,"first_day":calendar::day_text(day(e.starts_at)),
            "end_day_exclusive":calendar::day_text(day(e.ends_at).max(day(e.starts_at) + 1))});
    }
    match e.timezone.as_deref().and_then(calendar::zone) {
        Some(zone) => json!({
            "all_day": false,
            "timezone": zone.name.clone().or(e.timezone.clone()),
            "start": {"utc":calendar::utc_text(e.starts_at),"in_event_zone":calendar::zoned_text(e.starts_at, &zone)},
            "end": {"utc":calendar::utc_text(e.ends_at),"in_event_zone":calendar::zoned_text(e.ends_at, &zone)},
        }),
        None => json!({
            "all_day": false,
            "floating": floats(e),
            "start_local": calendar::zoned_text(e.starts_at, &system_zone()),
            "end_local": calendar::zoned_text(e.ends_at, &system_zone()),
        }),
    }
}

/// Text as shown: as [`shown`], and the line and paragraph separators U+2028 and U+2029,
/// which a terminal may draw as a line break, made visible as U+FFFD too.
fn seen(text: &str, chars: usize) -> String {
    shown(text, chars).replace(['\u{2028}', '\u{2029}'], "\u{FFFD}")
}

/// Notes as shown, with a fingerprint of all of them, so an approval is bound to the whole
/// text. On the card, the first `NOTES_SHOWN` characters and how many more there are; in the
/// terminal (`full`), all of them, line breaks kept.
fn notes_view(notes: &str, full: bool) -> Value {
    let count = notes.chars().count();
    let text = if full {
        notes
            .split('\n')
            .map(|line| seen(line, usize::MAX))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        let mut text = seen(notes, NOTES_SHOWN);
        if count > NOTES_SHOWN {
            text.push_str(&format!(" ({} more characters)", count - NOTES_SHOWN));
        }
        text
    };
    json!({"text":text,"characters":count,
        "fingerprint":quince_sync::queue::account_tag(&format!("skykeep-calendar-notes:{notes}"))})
}

fn event_view(e: &Event, full: bool) -> Value {
    let mut v = json!({
        "title": seen(&e.title, MAX_TITLE_BYTES),
        "location": e.location.as_deref().map(|l| seen(l, MAX_LOCATION_BYTES)),
        "notes": e.notes.as_deref().map(|n| notes_view(n, full)),
        "url": e.url.as_deref().map(|u| seen(u, MAX_URL_BYTES)),
        "when": when_view(e),
    });
    if !e.guid.is_empty() {
        v["guid"] = json!(e.guid);
        v["etag"] = json!(e.etag);
    }
    if is_occurrence(&e.guid) {
        v["series_guid"] = json!(series_of(&e.guid));
    }
    v
}

/// Each field of an update: `unchanged`, `removed`, or the new value.
fn changes_view(before: &Value, after: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for field in ["title", "location", "notes", "url", "when"] {
        let (old, new) = (&before[field], &after[field]);
        out.insert(
            field.into(),
            if old == new {
                json!("unchanged")
            } else if new.is_null() {
                json!("removed")
            } else {
                json!({"new": new})
            },
        );
    }
    Value::Object(out)
}

fn calendar_view(c: &Calendar) -> Value {
    json!({"guid":c.guid,"title":seen(&c.title, SHOWN_CHARS)})
}

/// What the approval shows and is bound to: the action and what it reaches as cached, and
/// whether the rules in force let it reach invited people (`invitations`). The same
/// arguments over the same cache and rules give the same preview, so a change in between
/// makes an approval stale. `full_notes` is for the terminal, which shows notes whole.
pub(crate) fn preview(
    library: &Library,
    args: &Value,
    invitations: bool,
    full_notes: bool,
) -> Result<Value, &'static str> {
    let op = validate(args)?;
    let plan = plan(library, &op)?;
    let long = |e: &Option<Event>| {
        e.as_ref()
            .and_then(|e| e.notes.as_deref())
            .is_some_and(|n| n.chars().count() > MAX_NOTES_BYTES)
    };
    if full_notes && (long(&plan.before) || long(&plan.after)) {
        return Err(NOTES_TOO_LONG);
    }
    let mut value = json!({
        "action": op.action(),
        "destructive": is_destructive(op.action()),
        "calendar": calendar_view(&plan.calendar),
    });
    if let Some(from) = &plan.from {
        value["from_calendar"] = calendar_view(from);
    }
    if let Some(before) = &plan.before {
        value["event"] = event_view(before, full_notes);
    }
    if let (Some(after), true) = (
        &plan.after,
        matches!(op.action, Action::Create | Action::Update),
    ) {
        value["result"] = event_view(after, full_notes);
        if plan.before.is_some() {
            value["changes"] = changes_view(&value["event"], &value["result"]);
        }
    }
    if op.action == Action::Create && op.end.is_none() {
        value["end_default"] = json!(if op.all_day == Some(true) {
            "one day: the event ends at the start of the next day"
        } else {
            "one hour after the start"
        });
    }
    if let Some(scope) = op.scope {
        value["scope"] = json!(scope.as_str());
    }
    if matches!(op.action, Action::Create | Action::Update) {
        value["alerts"] = match &op.alerts {
            Some(alerts) if alerts.is_empty() => json!("all removed"),
            Some(alerts) => json!({"minutes_before_start": alerts}),
            None if op.action == Action::Create => json!("none"),
            None => json!("kept as Apple has them"),
        };
    }
    let what = match op.action {
        Action::Delete => "a cancellation",
        _ => "an update",
    };
    value["invitations"] = json!(match (op.action, invitations) {
        (Action::Create, _) => "calendar_write adds no attendees. If the calendar is shared, its members see the new event; the cache does not say whether it is shared.".to_string(),
        (_, true) => format!("Skykeep reads the event's attendees from Apple just before the change. If other people are invited, Apple emails each of them {what}, because permissions.yaml sets {INVITATIONS_RULE}: allow. Members of a shared calendar see the change too."),
        (_, false) => format!("Skykeep reads the event's attendees from Apple just before the change. If other people are invited, the change is refused before anything is sent, because permissions.yaml does not set {INVITATIONS_RULE}: allow. Members of a shared calendar see the change."),
    });
    value["effect"] = json!(match (op.action, op.scope) {
        (Action::Create, _) => "adds one new event",
        (Action::Update, Some(Repeat::WholeSeries)) =>
            "changes every occurrence of the series, past and future",
        (Action::Update, Some(Repeat::ThisOccurrence)) =>
            "changes this one occurrence; the rest of the series stays as it is",
        (Action::Update, None) => "changes the event; fields not given stay as they are",
        (Action::Move, _) => "moves the event, as it is, into the other calendar",
        (Action::Delete, Some(Repeat::WholeSeries)) =>
            "deletes every occurrence of the series, past and future; Skykeep cannot undo it",
        (Action::Delete, Some(Repeat::ThisOccurrence)) =>
            "deletes this one occurrence; the rest of the series stays; Skykeep cannot undo it",
        (Action::Delete, None) => "deletes the event; Skykeep cannot undo it",
    });
    Ok(value)
}

/// A preview in plain lines, for the terminal approval.
pub(crate) fn plain_preview(preview: &Value) -> String {
    let text = |v: &Value| v.as_str().unwrap_or("?").to_string();
    let when = |w: &Value| {
        if w["all_day"] == true {
            format!(
                "all day, from {} until before {}",
                text(&w["first_day"]),
                text(&w["end_day_exclusive"])
            )
        } else if w["start"].is_object() {
            format!(
                "{} to {} ({})",
                text(&w["start"]["in_event_zone"]),
                text(&w["end"]["in_event_zone"]),
                text(&w["timezone"])
            )
        } else {
            format!(
                "{} to {} (floating)",
                text(&w["start_local"]),
                text(&w["end_local"])
            )
        }
    };
    let field = |e: &Value, name: &str| -> String {
        match name {
            "when" => when(&e["when"]),
            "notes" => text(&e["notes"]["text"]).replace('\n', "\n    "),
            _ => text(&e[name]),
        }
    };
    let event = |lines: &mut Vec<String>, label: &str, e: &Value| {
        lines.push(format!("{label}:"));
        for (name, shown) in [
            ("title", "Title"),
            ("when", "When"),
            ("location", "Location"),
            ("notes", "Notes"),
            ("url", "URL"),
        ] {
            if !e[name].is_null() {
                lines.push(format!("  {shown}: {}", field(e, name)));
            }
        }
    };
    let mut lines = vec![format!(
        "Calendar change: {}{}",
        text(&preview["action"]),
        if preview["destructive"] == true {
            ", a deletion"
        } else {
            ""
        }
    )];
    lines.push(format!("What it does: {}", text(&preview["effect"])));
    if preview["event"].is_object() {
        event(&mut lines, "Event now", &preview["event"]);
    }
    if let Some(changes) = preview["changes"].as_object() {
        lines.push("Changes:".into());
        for (name, change) in changes {
            let shown = match change.as_str() {
                Some(word) => word.to_string(),
                None => match name.as_str() {
                    "when" => when(&change["new"]),
                    "notes" => text(&change["new"]["text"]).replace('\n', "\n    "),
                    _ => text(&change["new"]),
                },
            };
            lines.push(format!("  {name}: {shown}"));
        }
    } else if preview["result"].is_object() {
        event(&mut lines, "New event", &preview["result"]);
    }
    if preview["end_default"].is_string() {
        lines.push(format!("End: {}", text(&preview["end_default"])));
    }
    if preview["from_calendar"].is_object() {
        lines.push(format!(
            "From calendar: {}",
            text(&preview["from_calendar"]["title"])
        ));
    }
    lines.push(format!("Calendar: {}", text(&preview["calendar"]["title"])));
    if preview["scope"].is_string() {
        lines.push(format!(
            "Repeating event, scope: {}",
            text(&preview["scope"])
        ));
    }
    if let Some(alerts) = preview["alerts"].as_str() {
        lines.push(format!("Alerts: {alerts}"));
    } else if let Some(m) = preview["alerts"]["minutes_before_start"].as_array() {
        let m: Vec<String> = m.iter().map(|v| v.to_string()).collect();
        lines.push(format!("Alerts: {} minutes before the start", m.join(", ")));
    }
    lines.push(format!("Invitations: {}", text(&preview["invitations"])));
    lines.join("\n")
}

/// What the person can do after `error`, when there is something. `args` are the call's.
pub(crate) fn next_step(error: &str, args: &Value) -> Option<String> {
    Some(match error {
        UNKNOWN_FIELD => {
            let unknown: Vec<String> = args
                .as_object()
                .map(|m| {
                    m.keys()
                        .filter(|k| !FIELDS.contains(&k.as_str()))
                        .take(5)
                        .map(|k| seen(k, 64))
                        .collect()
                })
                .unwrap_or_default();
            format!(
                "calendar_write does not take {}. Its fields are {}. It never adds or removes attendees; make such changes in Apple Calendar.",
                unknown.join(", "),
                FIELDS.join(", ")
            )
        }
        FLOATING => "This event has no time zone: a floating event, or an all-day one becoming timed, is shown at the same clock time everywhere. New times would tie it to a zone, so give timezone with them, such as timezone: Europe/Berlin, or timezone: UTC to keep offset times as they are.".into(),
        UNKNOWN_ACTION => format!("action must be one of {}.", ACTIONS.join(", ")),
        "calendar_change_refused_by_apple_nothing_changed" => "Apple refused the change, and nothing changed. Run calendar_refresh and look at the event with calendar_get before trying again, or change it in Apple Calendar.".into(),
        CALENDAR_CHANGED => "The calendar changed at Apple while the event was being added, and nothing was saved. Run calendar_refresh, then try the create again.".into(),
        NOTES_TOO_LONG => format!("The event's notes are longer than {MAX_NOTES_BYTES} characters, too long to show whole in the terminal approval. Make this change in Apple Calendar, or approve it on the card in an MCP client."),
        "event_too_long" => format!("An event may last at most {MAX_EVENT_DAYS} days. Give an earlier end."),
        "already_in_this_calendar" => "The event is already in that calendar. Run calendar_calendars to pick another calendar_guid, or leave it where it is.".into(),
        "unknown_timezone" => "Use an IANA zone name such as Europe/Berlin, America/New_York or UTC.".into(),
        "invalid_alerts" => "alerts is a list of minutes before the start, 0 at the start. Duplicates, more than 8 alerts, and more than 40320 minutes (four weeks) are refused.".into(),
        "this_occurrence_needs_an_occurrence_guid" => "this_occurrence needs the guid of one occurrence, such as SERIES__20261109T090000Z from calendar_events. For the series itself use scope: whole_series, or leave scope out for an event that does not repeat.".into(),
        "calendar_read_only" => "That calendar is read-only (subscribed, or shared with you for reading). Run calendar_calendars and pick one whose read_only is false.".into(),
        "end_must_be_after_start" => "end must be after start; for an all-day event, end is the day after the last day.".into(),
        "etag_required_from_calendar_get" => "update, move and delete need the event's etag. Read the event with calendar_get and pass its etag.".into(),
        UNKNOWN => "The connection ended before Apple answered, so the change may or may not have been made. Run calendar_refresh over the event's days and look at it with calendar_events before trying again; a create tried again could make a second event.".into(),
        CHANGED | "etag_not_current_run_calendar_get" | "event_version_unknown_run_calendar_refresh" => "The event is not the version this change was made for. Run calendar_refresh over its days, read it again with calendar_get, and decide again with its current etag.".into(),
        ATTENDEES => format!("Other people are invited to this event, and the change would send them an update or a cancellation. Nothing was sent. To let such changes run, the person sets {INVITATIONS_RULE}: allow in permissions.yaml in their own terminal; otherwise make the change in Apple Calendar."),
        SCOPE_REQUIRED => "This is a repeating event. Ask again with scope: this_occurrence (one occurrence, by its occurrence guid) or scope: whole_series (every occurrence).".into(),
        MOVE_SCOPE => "Only a whole series can move to another calendar. Ask again with scope: whole_series.".into(),
        NOT_REPEATING => "This event does not repeat. Ask again without scope.".into(),
        INVALID_START | INVALID_END => format!("The date or time could not be read. {TIME_FORMAT}"),
        TIMEZONE_REQUIRED => "A local time without an offset needs timezone, an IANA name such as Europe/Berlin, to say where that clock time is. Either add timezone, or give start and end with offsets such as 2026-03-29T09:30:00+02:00; with offsets alone the event is kept in UTC.".into(),
        START_REQUIRED => format!("Give start as well. {TIME_FORMAT}"),
        "start_required_to_change_all_day" => "Switching between an all-day and a timed event needs a new start: a date such as 2026-03-29 for all_day: true, or a time with an offset or timezone for all_day: false.".into(),
        "time_does_not_exist_in_timezone_clocks_skip_it" => format!("The clocks skip that local time in that zone (a daylight saving change), so no such time exists there. Pick another time. {TIME_FORMAT}"),
        "time_is_ambiguous_in_timezone_give_an_offset" => "The clocks pass that local time twice in that zone (a daylight saving change). Only the first pass can be kept: give it with that pass's offset, such as 2026-10-25T02:30:00+02:00 in Europe/Berlin.".into(),
        SECOND_PASS => "Apple keeps an event's time as the clock reads in its zone, and in the hour the clocks pass twice that reading means the first pass. A time in the second pass can't be kept. Pick a time outside that hour, or give the event in another zone such as UTC.".into(),
        "give_start_to_change_the_timezone" => "A new timezone changes when the event happens. Give start (and end, if it changes) in the new zone.".into(),
        "create_takes_no_guid_etag_or_scope" => "A create makes a new event, so it takes no guid, etag or scope. Leave them out, or use update for an existing event.".into(),
        "delete_takes_only_guid_etag_and_scope" => "A delete takes only guid, etag and, for a repeating event, scope. Leave the other fields out.".into(),
        "calendar_not_found" => "No iCloud calendar with that guid is in the cache. Run calendar_calendars to see the guids; if the calendar is new, run calendar_refresh first.".into(),
        EVENT_NOT_FOUND => "No event with that guid is in the cache. Run calendar_refresh over its days, then find it with calendar_events or calendar_search and read it with calendar_get.".into(),
        "nothing_to_change" => "The update gives no field that differs from the event as it is. Give the fields to change.".into(),
        _ => return None,
    })
}

// --- Running an action.

/// Time limits of one action.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct WriteLimits {
    /// The reads before the change and the change itself.
    pub change_timeout: Duration,
    /// The read after the change, apart: when it is too slow the change is still done.
    pub read_back_timeout: Duration,
}

impl Default for WriteLimits {
    fn default() -> Self {
        Self {
            change_timeout: Duration::from_secs(60),
            read_back_timeout: Duration::from_secs(20),
        }
    }
}

/// Test seam: synthetic endpoints and short limits. Production uses neither.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct Seam {
    pub client: Option<MakeClient>,
    pub limits: WriteLimits,
}

fn client_for(dir: &Path, seam: &Seam) -> Result<Client, &'static str> {
    if let Some(make) = &seam.client {
        return make(dir);
    }
    let store = crate::storage::open_store(dir)?;
    let session = store
        .try_load()
        .map_err(|_| "login_required_unlock_secure_storage")?;
    if !session.is_resumable() {
        return Err("login_required");
    }
    Client::with(dir, Endpoints::default(), store).map_err(|_| "login_required")
}

struct Conn {
    client: Client,
    dsid: String,
    root: String,
}

async fn connect(dir: &Path, seam: &Seam) -> Result<Conn, &'static str> {
    let mut client = client_for(dir, seam)?;
    if !client
        .resume()
        .await
        .map_err(|_| "login_required_or_network_unavailable")?
    {
        return Err("login_required");
    }
    if client.waits_on_icloud_com() {
        return Err("login_required_complete_account_setup");
    }
    let dsid = client.session().dsid.clone().ok_or("login_required")?;
    let library = Library::open(dir).map_err(|_| "library_unavailable")?;
    if binding(&library, &dsid)? != Binding::Same {
        return Err("account_mismatch_use_new_state_directory");
    }
    let root = client
        .session()
        .service_url("calendar")
        .ok_or("calendar_service_unavailable")?;
    Ok(Conn { client, dsid, root })
}

/// A read made before anything is sent.
fn read_error(e: &quince_icloud::Error) -> &'static str {
    match e {
        quince_icloud::Error::ReauthRequired => "login_required",
        _ => READ_FAILED,
    }
}

/// The change itself: once it may have reached Apple, a lost answer is unknown.
fn write_error(e: &quince_icloud::Error) -> &'static str {
    use quince_icloud::Error as E;
    match e {
        E::ReauthRequired => "login_required",
        E::Network(_) | E::Busy(_) => UNKNOWN,
        E::Conflict => CHANGED,
        E::NotFound => GONE,
        E::Refused(_) => "calendar_change_refused_by_apple_nothing_changed",
        _ => NOT_CONFIRMED,
    }
}

/// Apple's copy shows what was asked: the same title, place in time and calendar.
fn same(apple: &Event, asked: &Event) -> bool {
    apple.title == asked.title
        && apple.calendar_guid == asked.calendar_guid
        && apple.all_day == asked.all_day
        && apple.starts_at == asked.starts_at
        && apple.ends_at == asked.ends_at
}

/// What a finished action leaves for the cache.
#[derive(Default)]
struct Effects {
    put: Vec<Event>,
    forget: Vec<String>,
    /// A series whose cached occurrences are gone.
    forget_series: Option<String>,
}

fn settle(dir: &Path, dsid: &str, effects: &Effects) {
    let Ok(mut library) = Library::open(dir) else {
        return;
    };
    if binding(&library, dsid) != Ok(Binding::Same) {
        return;
    }
    let mut forget = effects.forget.clone();
    if let Some(series) = &effects.forget_series {
        if let Ok(rows) = library.events_starting_between(i64::MIN / 4, i64::MAX / 4) {
            forget.extend(
                rows.into_iter()
                    .map(|(guid, _)| guid)
                    .filter(|g| series_of(g) == series),
            );
        }
    }
    let _ = library.forget_events(&forget);
    let _ = library.put_events(&effects.put);
}

/// Runs one action. `approved` says a person approved this exact action (in the form or the
/// terminal); `invitations` that the permissions file sets `calendar_write.send_invitations:
/// allow`. Caller holds the private directory's lock.
pub(crate) async fn run(
    dir: &Path,
    seam: &Seam,
    op: CalendarWrite,
    approved: bool,
    invitations: bool,
) -> Result<Value, &'static str> {
    let library = Library::open(dir).map_err(|_| "library_unavailable")?;
    if library
        .state(quince_sync::queue::ACCOUNT_KEY)
        .map_err(|_| "library_unavailable")?
        .is_none()
    {
        return Err("login_required");
    }
    if !approved {
        let mode = crate::mode_in_force(&library, dir)?;
        if mode.as_deref().is_some_and(|m| m != "yolo") || is_destructive(op.action()) {
            return Err("approval_required");
        }
    }
    let plan = plan(&library, &op)?;
    drop(library);
    let conn = connect(dir, seam).await?;
    let sent = AtomicBool::new(false);
    let mut effects = Effects::default();
    let outcome = tokio::time::timeout(
        seam.limits.change_timeout,
        act(&conn, &op, &plan, invitations, &sent, &mut effects),
    )
    .await;
    let result = match outcome {
        Ok(Ok(acted)) => finish(&conn, acted, seam.limits.read_back_timeout, &mut effects).await,
        Ok(Err(e)) => Err(e),
        Err(_) if !sent.load(Ordering::SeqCst) => Err("calendar_timed_out_nothing_sent"),
        Err(_) => Err(UNKNOWN),
    };
    settle(dir, &conn.dsid, &effects);
    result.map(|mut value| {
        value["action"] = json!(op.action());
        value
    })
}

/// What is left to check once Apple accepted a change.
enum Check {
    /// Read the event back under its own id.
    ReadBack(Event),
    /// Read the deleted event: it must be gone.
    Gone(Event),
}

/// A change Apple accepted: the reply so far, and what to read back.
struct Acted {
    value: Value,
    check: Option<Check>,
}

/// The calendar's ctag as Apple holds it now, or the cached one where Apple names none: an
/// earlier save moved it on, and a stale one may be refused.
async fn current_ctag(
    api: &CalendarClient<'_>,
    c: &Calendar,
) -> Result<Option<String>, &'static str> {
    Ok(api
        .current_ctag(&c.guid)
        .await
        .map_err(|e| read_error(&e))?
        .or_else(|| c.ctag.clone()))
}

async fn act(
    conn: &Conn,
    op: &CalendarWrite,
    plan: &Plan,
    invitations: bool,
    sent: &AtomicBool,
    effects: &mut Effects,
) -> Result<Acted, &'static str> {
    let api = CalendarClient::new(conn.client.http(), &conn.root, conn.client.service_params());
    let scope = op.scope.map(Repeat::scope);
    let alerts = op.alerts.as_deref();
    let ctag = current_ctag(&api, &plan.calendar).await?;
    let ctag = ctag.as_deref();
    if op.action == Action::Create {
        let mut event = plan.after.clone().ok_or("invalid_arguments")?;
        event.guid = uuid::Uuid::new_v4().to_string().to_uppercase();
        sent.store(true, Ordering::SeqCst);
        api.put_event_with_alerts(&event, ctag, Some(alerts.unwrap_or(&[])))
            .await
            .map_err(|e| match write_error(&e) {
                // A new event has no version: a conflict here is the calendar's.
                CHANGED => CALENDAR_CHANGED,
                other => other,
            })?;
        return Ok(Acted {
            value: json!({}),
            check: Some(Check::ReadBack(event)),
        });
    }
    let before = plan.before.as_ref().ok_or(EVENT_NOT_FOUND)?;
    // Apple's copy now: there, the version cached, and how it repeats.
    let now = api
        .current_event(before)
        .await
        .map_err(|e| read_error(&e))?
        .ok_or(GONE)?;
    if now.etag != before.etag {
        return Err(CHANGED);
    }
    if !is_occurrence(&before.guid) {
        let repeats = api
            .repeat_of(before)
            .await
            .map_err(|e| read_error(&e))?
            .is_some();
        match (repeats, scope.is_some()) {
            (true, false) => return Err(SCOPE_REQUIRED),
            (false, true) => return Err(NOT_REPEATING),
            _ => {}
        }
    }
    let guests = api.guests(before).await.map_err(|e| read_error(&e))?;
    let others = guests.iter().filter(|g| !g.is_me).count();
    if others > 0 && !invitations {
        return Err(ATTENDEES);
    }
    let from_ctag = match &plan.from {
        Some(from) => current_ctag(&api, from).await?,
        None => None,
    };
    let series = op.scope == Some(Repeat::WholeSeries);
    sent.store(true, Ordering::SeqCst);
    let (mut value, check) = match op.action {
        Action::Update | Action::Move => {
            let after = plan.after.as_ref().ok_or("invalid_arguments")?;
            match &plan.from {
                Some(from) => {
                    api.put_event_moved(
                        after,
                        ctag,
                        None,
                        None,
                        scope,
                        Some((from.guid.as_str(), from_ctag.as_deref())),
                    )
                    .await
                }
                None => api.put_event_full(after, ctag, alerts, None, scope).await,
            }
            .map_err(|e| write_error(&e))?;
            if series {
                // Every cached occurrence of the series is out of date now.
                effects.forget_series = Some(series_of(&before.guid).to_string());
                effects.forget.push(before.guid.clone());
                (
                    json!({
                        "outcome": "done",
                        "event": {"guid": after.guid, "title": seen(&after.title, SHOWN_CHARS), "calendar_guid": after.calendar_guid},
                        "checked": "Apple accepted the change to the whole series; it was not read back. Run calendar_refresh over the series' days to see it",
                        "remote_verified": false,
                    }),
                    None,
                )
            } else {
                (json!({}), Some(Check::ReadBack(after.clone())))
            }
        }
        Action::Delete => {
            api.delete_event_scoped(before, ctag, scope)
                .await
                .map_err(|e| write_error(&e))?;
            if series {
                effects.forget_series = Some(series_of(&before.guid).to_string());
            }
            effects.forget.push(before.guid.clone());
            if is_occurrence(&before.guid) {
                (
                    json!({
                        "outcome": "done",
                        "event": {"guid": before.guid, "title": seen(&before.title, SHOWN_CHARS)},
                        "checked": "Apple accepted the deletion; Apple answers for an occurrence with its series, so it was not read back",
                        "remote_verified": false,
                    }),
                    None,
                )
            } else {
                (json!({}), Some(Check::Gone(before.clone())))
            }
        }
        Action::Create => unreachable!("handled above"),
    };
    if others > 0 {
        value["attendees_notified"] = json!(others);
    }
    if let Some(scope) = op.scope {
        value["scope"] = json!(scope.as_str());
    }
    Ok(Acted { value, check })
}

/// Reads back what Apple accepted, under its own time limit: a read that takes too long
/// leaves the change done but not verified, never unknown.
async fn finish(
    conn: &Conn,
    acted: Acted,
    limit: Duration,
    effects: &mut Effects,
) -> Result<Value, &'static str> {
    let api = CalendarClient::new(conn.client.http(), &conn.root, conn.client.service_params());
    let mut value = match acted.check {
        None => return Ok(acted.value),
        Some(Check::ReadBack(asked)) => read_back(&api, &asked, effects, limit).await,
        Some(Check::Gone(before)) => {
            let event = json!({"guid": before.guid, "title": seen(&before.title, SHOWN_CHARS)});
            match tokio::time::timeout(limit, api.current_event(&before)).await {
                Ok(Ok(None)) => json!({
                    "outcome": "done",
                    "event": event,
                    "checked": "the event was read back from Apple and is gone",
                    "remote_verified": true,
                }),
                Ok(Ok(Some(_))) => {
                    effects.forget.clear();
                    effects.forget_series = None;
                    return Err(NOT_CONFIRMED);
                }
                Ok(Err(_)) => json!({
                    "outcome": "done",
                    "event": event,
                    "checked": "Apple accepted the deletion; reading it back failed",
                    "remote_verified": false,
                }),
                Err(_) => json!({
                    "outcome": "done",
                    "event": event,
                    "checked": "Apple accepted the deletion; reading it back took too long",
                    "remote_verified": false,
                }),
            }
        }
    };
    if let Some(extra) = acted.value.as_object() {
        for (k, v) in extra {
            value[k] = v.clone();
        }
    }
    Ok(value)
}

/// The day Apple is asked about for an event, as the engine asks: in the event's own zone,
/// or for an all-day or floating one in this computer's.
fn day_asked(e: &Event) -> String {
    let zone = match e.timezone.as_deref() {
        Some(name) if !e.all_day => calendar::zone(name).map_or(jiff::tz::TimeZone::UTC, |z| z.tz),
        _ => jiff::tz::TimeZone::system(),
    };
    calendar::day_text(wall_clock(e.starts_at, &zone).div_euclid(DAY))
}

/// Reads the event back from Apple after a save and says what was checked. Only an event
/// Apple returns under the very id asked about counts: for one occurrence Apple may answer
/// with its series alone, which says nothing about the occurrence. The cache gets Apple's
/// copy, or, when it can't be confirmed, the event without a version, so the next change asks
/// for a refresh first.
async fn read_back(
    api: &CalendarClient<'_>,
    asked: &Event,
    effects: &mut Effects,
    limit: Duration,
) -> Value {
    let event = json!({"guid": asked.guid, "title": seen(&asked.title, SHOWN_CHARS), "calendar_guid": asked.calendar_guid});
    let found = tokio::time::timeout(limit, api.event_detail_body(asked, &day_asked(asked)))
        .await
        .map(|read| {
            read.map(|body| {
                body.and_then(|mut body| {
                    // Apple may send one event as an object rather than a list.
                    if body["Event"].is_object() {
                        body["Event"] = Value::Array(vec![body["Event"].take()]);
                    }
                    quince_icloud::calendar::parse_snapshot(&body)
                        .events
                        .into_iter()
                        .find(|e| e.guid == asked.guid)
                })
            })
        });
    match found {
        Ok(Ok(Some(apple))) if same(&apple, asked) => {
            let etag = apple.etag.clone();
            effects.put.push(apple);
            json!({
                "outcome": "done",
                "event": event,
                "etag": etag,
                "checked": "the event was read back from Apple with this title, these times and this calendar",
                "remote_verified": true,
            })
        }
        other => {
            let mut unversioned = asked.clone();
            unversioned.etag = None;
            effects.put.push(unversioned);
            json!({
                "outcome": "done",
                "event": event,
                "checked": match other {
                    Ok(Ok(Some(_))) => "Apple accepted the change, but its copy read back differs; run calendar_refresh and look at it",
                    Ok(Ok(None)) => "Apple accepted the change, but reading it back did not show this event under its own id; run calendar_refresh and look at it",
                    Ok(Err(_)) => "Apple accepted the change; reading it back failed",
                    Err(_) => "Apple accepted the change; reading it back took too long. Run calendar_refresh and look at it",
                },
                "remote_verified": false,
            })
        }
    }
}

/// The tool's entry in `tools/list`.
pub(crate) fn tool() -> Value {
    let time = |what: &str| json!({"description": format!("{what} 2026-03-29T09:30:00+02:00, 2026-03-29T07:30:00Z, or 2026-03-29T09:30:00 with timezone. All-day: a date such as 2026-03-29."), "type":"string","minLength":10,"maxLength":MAX_TIME_BYTES});
    json!({
        "name": TOOL,
        "description": "Change the user's Apple iCloud Calendar: create (calendar_guid, title, start, optional end, all_day, timezone, location, notes, url, alerts), update (guid and etag from calendar_get, and the fields to change), move (guid, etag, calendar_guid of another calendar) and delete (guid, etag). Times need an offset or timezone (an IANA name); a create with offset times and no timezone is kept in UTC. A local time the clocks skip is refused; in the hour they pass twice, only the first pass can be kept, given with its offset. All-day events take dates, end being the day after the last (default one day). calendar_write adds no attendees; members of a shared calendar see changes. A repeating event needs scope: this_occurrence or whole_series; there is no default. A change to an event whose version changed since calendar_get is refused. An event with other people invited is refused unless the user has allowed, in their own terminal, changes that send those people an update. Deletes always need the user's approval. The reply says done and whether Apple's copy was read back (remote_verified), or an error; an unknown outcome means run calendar_refresh and look before trying again.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "action": {"enum": ACTIONS},
                "guid": {"description": "Event guid from calendar_events, calendar_search or calendar_get. For one occurrence of a repeating event, its occurrence guid.", "type":"string","minLength":1,"maxLength":calendar::MAX_GUID_BYTES},
                "etag": {"description": "The event's etag from calendar_get: the version this change is for.", "type":"string","minLength":1,"maxLength":MAX_ETAG_BYTES},
                "calendar_guid": {"description": "create: the calendar to add the event to; move: the calendar to move it into. From calendar_calendars.", "type":"string","minLength":1,"maxLength":calendar::MAX_GUID_BYTES},
                "title": {"description": "The event's name. Required for create.", "type":"string","minLength":1,"maxLength":MAX_TITLE_BYTES},
                "location": {"description": "Empty string removes it.", "type":"string","maxLength":MAX_LOCATION_BYTES},
                "notes": {"description": "Empty string removes them.", "type":"string","maxLength":MAX_NOTES_BYTES},
                "url": {"description": "Empty string removes it.", "type":"string","maxLength":MAX_URL_BYTES},
                "start": time("Start, such as"),
                "end": time("End (default: one hour after start, one day for all-day, or for update the same length as before), such as"),
                "all_day": {"description": "true for an all-day event: start and end are dates, and end defaults to the day after start. Default false, or for update the event's own.", "type":"boolean"},
                "timezone": {"description": "IANA zone name such as Europe/Berlin: the zone a timed event lives in and local times are read in. create: needed for a local time; without it, offset times keep the event in UTC. update: default the event's own.", "type":"string","minLength":1,"maxLength":calendar::MAX_ZONE_BYTES},
                "alerts": {"description": "Alerts in minutes before the start, 0 at the start. update: replaces all alerts; [] removes them; omit to keep them.", "type":"array","maxItems":MAX_ALERTS,"items":{"type":"integer","minimum":0,"maximum":MAX_ALERT_MINUTES}},
                "scope": {"description": "For a repeating event: this_occurrence (only with an occurrence guid) or whole_series. Required for a repeating event; no default.", "enum":["this_occurrence","whole_series"]},
            },
            "required": ["action"],
            "additionalProperties": false,
        },
        "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_delete_is_destructive_and_it_is_permanent() {
        for action in ACTIONS {
            assert_eq!(is_destructive(action), action == "delete", "{action}");
        }
        assert_eq!(
            crate::permissions::removal_of(TOOL, "delete"),
            crate::permissions::Removal::Permanent
        );
    }

    #[test]
    fn times_take_only_the_stated_forms() {
        assert_eq!(parse_when("2026-03-29", true), Some(When::Day(20_541)));
        assert_eq!(
            parse_when("2026-03-29T09:30:00+02:00", false),
            Some(When::Instant(20_541 * DAY + 7 * 3600 + 1800))
        );
        assert_eq!(
            parse_when("2026-03-29T07:30Z", false),
            Some(When::Instant(20_541 * DAY + 7 * 3600 + 1800))
        );
        assert_eq!(
            parse_when("2026-03-29T09:30", false),
            Some(When::Local(20_541 * DAY + 9 * 3600 + 1800))
        );
        for bad in [
            "2026-03-29 09:30",
            "2026-03-29T9:30",
            "2026-03-29T09:30:15",
            "2026-03-29T24:00",
            "2026-03-29T09:30+0200",
            "2026-03-29T09:30:00.000Z",
            "2026-02-30T09:30Z",
            "tomorrow",
        ] {
            assert_eq!(parse_when(bad, false), None, "{bad}");
        }
        assert_eq!(parse_when("2026-03-29T09:30Z", true), None);
    }

    #[test]
    fn local_times_at_daylight_saving_changes_are_refused_or_placed() {
        let berlin = calendar::zone("Europe/Berlin").unwrap();
        // 2026-03-29 02:30 does not exist in Berlin; 2026-10-25 02:30 exists twice.
        assert_eq!(
            placed(20_541 * DAY + 2 * 3600 + 1800, &berlin),
            Err("time_does_not_exist_in_timezone_clocks_skip_it")
        );
        assert_eq!(
            placed(20_751 * DAY + 2 * 3600 + 1800, &berlin),
            Err("time_is_ambiguous_in_timezone_give_an_offset")
        );
        // 03:30 after the spring change is summer time, UTC+2.
        assert_eq!(
            placed(20_541 * DAY + 3 * 3600 + 1800, &berlin),
            Ok(20_541 * DAY + 3600 + 1800)
        );
    }
}
