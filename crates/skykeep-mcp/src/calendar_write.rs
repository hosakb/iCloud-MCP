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
//! that version any more, so a change made on another device is never written over.
//!
//! Repeating events. One occurrence (an id `<series>__<start>`) is changed only with `scope`
//! said: `this_occurrence` or `whole_series`. There is no default. An event that repeats but
//! is cached under its series id is checked at Apple before the change, and refused without
//! `whole_series`.
//!
//! Times need an explicit zone: an offset (`2026-03-29T09:30:00+02:00`, `...Z`) or a local
//! time with `timezone` (an IANA name such as Europe/Berlin). A local time the clocks skip
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
//! needs the person's approval, on the card or with `skykeep-mcp approve`, and the card never
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
        && !text
            .chars()
            .any(|c| hides_text(c) || (c.is_control() && !(lines && (c == '\n' || c == '\t'))))
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
                return Err("invalid_arguments");
            }
            if op.calendar_guid.is_none() {
                return Err("calendar_guid_required");
            }
            if op.title.is_none() {
                return Err("title_required");
            }
            if op.start.is_none() {
                return Err(INVALID_START);
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
                    if op.scope == Some(Repeat::ThisOccurrence) {
                        return Err("move_of_one_occurrence_not_supported_use_whole_series");
                    }
                }
                _ => {
                    if op.calendar_guid.is_some() || op.changes_fields() {
                        return Err("invalid_arguments");
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

/// Whether the approval is asked in the terminal: when the approval mode has no form for it
/// (no mode set, or `yolo`). The other modes ask every Calendar change in the form.
pub(crate) fn goes_to_terminal(mode: Option<&str>, op: &CalendarWrite) -> bool {
    mode.is_none_or(|m| m == "yolo") && is_destructive(op.action())
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
            _ => return Err(INVALID_START),
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
    let zone_name = match (&op.timezone, before) {
        (Some(z), _) => Some(z.clone()),
        (None, Some(b)) if !b.all_day => b.timezone.clone(),
        _ => None,
    };
    let zone = match &zone_name {
        Some(name) => Some(calendar::zone(name).ok_or("unknown_timezone")?),
        None => None,
    };
    let instant = |w: When, bad: &'static str| -> Result<i64, &'static str> {
        match w {
            When::Instant(at) => Ok(at),
            When::Local(wall) => placed(wall, zone.as_ref().ok_or("timezone_required")?)
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
        _ => return Err(INVALID_START),
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
    let name = zone_name.ok_or("timezone_required")?;
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
        .ok_or("not_found")?;
    if !calendars.iter().any(|c| c.guid == before.calendar_guid) {
        return Err("not_found");
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
                return Err(INVALID_START);
            }
            if op.timezone.is_some() && op.start.is_none() && op.end.is_none() {
                return Err("give_start_to_change_the_timezone");
            }
            let (starts_at, ends_at, timezone) = if op.changes_time() {
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

fn event_view(e: &Event) -> Value {
    let mut v = json!({
        "title": shown(&e.title, SHOWN_CHARS),
        "location": e.location.as_deref().map(|l| shown(l, SHOWN_CHARS)),
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

fn calendar_view(c: &Calendar) -> Value {
    json!({"guid":c.guid,"title":shown(&c.title, SHOWN_CHARS)})
}

/// What the approval shows and is bound to: the action and what it reaches as cached. The
/// same arguments over the same cache give the same preview, so a change in between makes an
/// approval stale.
pub(crate) fn preview(library: &Library, args: &Value) -> Result<Value, &'static str> {
    let op = validate(args)?;
    let plan = plan(library, &op)?;
    let mut value = json!({
        "action": op.action(),
        "destructive": is_destructive(op.action()),
        "calendar": calendar_view(&plan.calendar),
    });
    if let Some(from) = &plan.from {
        value["from_calendar"] = calendar_view(from);
    }
    if let Some(before) = &plan.before {
        value["event"] = event_view(before);
    }
    if let (Some(after), true) = (
        &plan.after,
        matches!(op.action, Action::Create | Action::Update),
    ) {
        value["result"] = event_view(after);
    }
    if let Some(scope) = op.scope {
        value["scope"] = json!(scope.as_str());
    }
    value["alerts"] = match &op.alerts {
        Some(alerts) if alerts.is_empty() => json!("all removed"),
        Some(alerts) => json!({"minutes_before_start": alerts}),
        None if op.action == Action::Create => json!("none"),
        None => json!("kept as Apple has them"),
    };
    let what = match op.action {
        Action::Delete => "a cancellation",
        _ => "an updated invitation",
    };
    value["invitations"] = json!(if op.action == Action::Create {
        "none: calendar_write never adds attendees, so nobody is invited".to_string()
    } else {
        format!("Skykeep reads the event's attendees from Apple just before the change. If other people are invited, Apple sends each of them {what}. That is refused before anything is sent unless permissions.yaml sets {INVITATIONS_RULE}: allow.")
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
    let event = |e: &Value| format!("{}, {}", text(&e["title"]), when(&e["when"]));
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
        lines.push(format!("Event now: {}", event(&preview["event"])));
    }
    if preview["result"].is_object() {
        lines.push(format!("Event after: {}", event(&preview["result"])));
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

/// What the person can do after `error`, when there is something.
pub(crate) fn next_step(error: &str) -> Option<String> {
    Some(match error {
        UNKNOWN => "The connection ended before Apple answered, so the change may or may not have been made. Run calendar_refresh over the event's days and look at it with calendar_events before trying again; a create tried again could make a second event.".into(),
        CHANGED | "etag_not_current_run_calendar_get" | "event_version_unknown_run_calendar_refresh" => "The event is not the version this change was made for. Run calendar_refresh over its days, read it again with calendar_get, and decide again with its current etag.".into(),
        ATTENDEES => format!("Other people are invited to this event, and the change would send them an update or a cancellation. Nothing was sent. To let such changes run, the person sets {INVITATIONS_RULE}: allow in permissions.yaml in their own terminal; otherwise make the change in Apple Calendar."),
        SCOPE_REQUIRED => "This is a repeating event. Ask again with scope: this_occurrence (one occurrence, by its occurrence guid) or scope: whole_series (every occurrence).".into(),
        INVALID_START | INVALID_END | "timezone_required" => format!("The date or time could not be read. {TIME_FORMAT}"),
        "time_does_not_exist_in_timezone_clocks_skip_it" => format!("The clocks skip that local time in that zone (a daylight saving change), so no such time exists there. Pick another time. {TIME_FORMAT}"),
        "time_is_ambiguous_in_timezone_give_an_offset" => "The clocks pass that local time twice in that zone (a daylight saving change). Give it with its offset, such as 2026-10-25T02:30:00+02:00 in Europe/Berlin for the first one.".into(),
        SECOND_PASS => "Apple keeps an event's time as the clock reads in its zone, and in the hour the clocks pass twice that reading means the first pass. A time in the second pass can't be kept. Pick a time outside that hour, or give the event in another zone such as UTC.".into(),
        "give_start_to_change_the_timezone" => "A new timezone changes when the event happens. Give start (and end, if it changes) in the new zone.".into(),
        _ => return None,
    })
}

// --- Running an action.

/// Time limits of one action.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct WriteLimits {
    /// The whole action: reads before the change, the change, and the read after it.
    pub change_timeout: Duration,
}

impl Default for WriteLimits {
    fn default() -> Self {
        Self {
            change_timeout: Duration::from_secs(60),
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
        let mode = crate::policy(&library, dir)?.map(|(p, _)| p.mode);
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
        Ok(result) => result,
        Err(_) if !sent.load(Ordering::SeqCst) => Err("calendar_timed_out_nothing_sent"),
        Err(_) => Err(UNKNOWN),
    };
    settle(dir, &conn.dsid, &effects);
    result.map(|mut value| {
        value["action"] = json!(op.action());
        value
    })
}

async fn act(
    conn: &Conn,
    op: &CalendarWrite,
    plan: &Plan,
    invitations: bool,
    sent: &AtomicBool,
    effects: &mut Effects,
) -> Result<Value, &'static str> {
    let api = CalendarClient::new(conn.client.http(), &conn.root, conn.client.service_params());
    let scope = op.scope.map(Repeat::scope);
    let ctag = plan.calendar.ctag.as_deref();
    let alerts = op.alerts.as_deref();
    if op.action == Action::Create {
        let mut event = plan.after.clone().ok_or("invalid_arguments")?;
        event.guid = uuid::Uuid::new_v4().to_string().to_uppercase();
        sent.store(true, Ordering::SeqCst);
        api.put_event_with_alerts(&event, ctag, Some(alerts.unwrap_or(&[])))
            .await
            .map_err(|e| write_error(&e))?;
        return Ok(read_back(&api, &event, effects).await);
    }
    let before = plan.before.as_ref().ok_or("not_found")?;
    // Apple's copy now: there, the version cached, and how it repeats.
    let now = api
        .current_event(before)
        .await
        .map_err(|e| read_error(&e))?
        .ok_or(GONE)?;
    if now.etag != before.etag {
        return Err(CHANGED);
    }
    if !is_occurrence(&before.guid)
        && scope.is_none()
        && api
            .repeat_of(before)
            .await
            .map_err(|e| read_error(&e))?
            .is_some()
    {
        return Err(SCOPE_REQUIRED);
    }
    let guests = api.guests(before).await.map_err(|e| read_error(&e))?;
    let others = guests.iter().filter(|g| !g.is_me).count();
    if others > 0 && !invitations {
        return Err(ATTENDEES);
    }
    let series = op.scope == Some(Repeat::WholeSeries);
    sent.store(true, Ordering::SeqCst);
    let mut value = match op.action {
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
                        Some((from.guid.as_str(), from.ctag.as_deref())),
                    )
                    .await
                }
                None => api.put_event_full(after, ctag, alerts, None, scope).await,
            }
            .map_err(|e| write_error(&e))?;
            if series {
                // The series' own dates moved with it: what the cache holds is out of date.
                effects.forget.push(before.guid.clone());
                json!({
                    "outcome": "done",
                    "event": {"guid": after.guid, "title": shown(&after.title, SHOWN_CHARS), "calendar_guid": after.calendar_guid},
                    "checked": "Apple accepted the change to the whole series; it was not read back. Run calendar_refresh over the series' days to see it",
                    "remote_verified": false,
                })
            } else {
                read_back(&api, after, effects).await
            }
        }
        Action::Delete => {
            api.delete_event_scoped(before, ctag, scope)
                .await
                .map_err(|e| write_error(&e))?;
            let shown_event =
                json!({"guid": before.guid, "title": shown(&before.title, SHOWN_CHARS)});
            if series {
                effects.forget_series = Some(series_of(&before.guid).to_string());
            }
            effects.forget.push(before.guid.clone());
            if is_occurrence(&before.guid) {
                json!({
                    "outcome": "done",
                    "event": shown_event,
                    "checked": "Apple accepted the deletion; Apple answers for an occurrence with its series, so it was not read back",
                    "remote_verified": false,
                })
            } else {
                match api.current_event(before).await {
                    Ok(None) => json!({
                        "outcome": "done",
                        "event": shown_event,
                        "checked": "the event was read back from Apple and is gone",
                        "remote_verified": true,
                    }),
                    Ok(Some(_)) => {
                        effects.forget.clear();
                        effects.forget_series = None;
                        return Err(NOT_CONFIRMED);
                    }
                    Err(_) => json!({
                        "outcome": "done",
                        "event": shown_event,
                        "checked": "Apple accepted the deletion; reading it back failed",
                        "remote_verified": false,
                    }),
                }
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
    Ok(value)
}

/// Reads the event back from Apple after a save and says what was checked. The cache gets
/// Apple's copy, or, when it can't be read, the event without a version, so the next change
/// asks for a refresh first.
async fn read_back(api: &CalendarClient<'_>, asked: &Event, effects: &mut Effects) -> Value {
    let event = json!({"guid": asked.guid, "title": shown(&asked.title, SHOWN_CHARS), "calendar_guid": asked.calendar_guid});
    match api.current_event(asked).await {
        Ok(Some(apple)) if same(&apple, asked) => {
            effects.put.push(apple);
            json!({
                "outcome": "done",
                "event": event,
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
                    Ok(Some(_)) => "Apple accepted the change, but its copy read back differs; run calendar_refresh and look at it",
                    Ok(None) => "Apple accepted the change, but reading it back did not show the event yet; run calendar_refresh and look at it",
                    Err(_) => "Apple accepted the change; reading it back failed",
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
        "description": "Change the user's Apple iCloud Calendar: create (calendar_guid, title, start, optional end, all_day, timezone, location, notes, url, alerts), update (guid and etag from calendar_get, and the fields to change), move (guid, etag, calendar_guid of another calendar) and delete (guid, etag). Times need an offset or timezone (an IANA name); a local time the clocks skip or pass twice is refused. All-day events take dates, end being the day after the last. A repeating event needs scope: this_occurrence or whole_series; there is no default. A change to an event whose version changed since calendar_get is refused. An event with other people invited is refused unless the user has allowed, in their own terminal, changes that send those people an update. Deletes always need the user's approval. The reply says done and whether Apple's copy was read back (remote_verified), or an error; an unknown outcome means run calendar_refresh and look before trying again.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "action": {"enum": ACTIONS},
                "guid": {"description": "Event guid from calendar_events, calendar_search or calendar_get. For one occurrence of a repeating event, its occurrence guid.", "type":"string","minLength":1,"maxLength":calendar::MAX_GUID_BYTES},
                "etag": {"description": "The event's etag from calendar_get: the version this change is for.", "type":"string","minLength":1,"maxLength":MAX_ETAG_BYTES},
                "calendar_guid": {"description": "create: the calendar to add the event to; move: the calendar to move it into. From calendar_calendars.", "type":"string","minLength":1,"maxLength":calendar::MAX_GUID_BYTES},
                "title": {"type":"string","minLength":1,"maxLength":MAX_TITLE_BYTES},
                "location": {"description": "Empty string removes it.", "type":"string","maxLength":MAX_LOCATION_BYTES},
                "notes": {"description": "Empty string removes them.", "type":"string","maxLength":MAX_NOTES_BYTES},
                "url": {"description": "Empty string removes it.", "type":"string","maxLength":MAX_URL_BYTES},
                "start": time("Start, such as"),
                "end": time("End (default: one hour after start, or the same length as before), such as"),
                "all_day": {"type":"boolean"},
                "timezone": {"description": "IANA zone name such as Europe/Berlin: the zone a timed event lives in and local times are read in. update: default the event's own.", "type":"string","minLength":1,"maxLength":calendar::MAX_ZONE_BYTES},
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
