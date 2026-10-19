// SPDX-License-Identifier: AGPL-3.0-only

//! Calendars on a CalDAV server (RFC 4791), and the occurrences of their events in a range of time.

use chrono::{DateTime, Duration, Utc};
use rrule::RRuleSet;

use std::collections::{HashMap, HashSet};

use crate::ical::{Event, When, parse};
use crate::{Client, Found, Kind, Remote};
use beavermail_core::MailError;
use beavermail_store::{EventSpan, Store};

/// One calendar on a CalDAV server, and how to sign in to it.
#[derive(Clone, Debug)]
pub struct Calendar {
    /// The calendar's own URL (a collection), ending in `/`.
    pub url: String,
    client: Client,
}

impl Calendar {
    /// A calendar already found, at its URL.
    pub fn open(url: &str, user: &str, password: &str) -> Self {
        Self { url: url.to_owned(), client: Client { user: user.to_owned(), password: password.to_owned() } }
    }

    /// Every calendar of the user that holds events, from a server address or a domain
    /// (`https://dav.example.org/`, `example.org`), found through `/.well-known/caldav` when needed.
    pub fn discover_all(start: &str, user: &str, password: &str) -> Result<Vec<Found>, MailError> {
        Client { user: user.to_owned(), password: password.to_owned() }.discover(start, Kind::Calendar)
    }

    /// The calendar's objects (each an event with its changed occurrences), with their versions.
    pub fn list(&self) -> Result<Vec<Remote>, MailError> {
        self.client.list(&self.url)
    }

    /// The objects at `hrefs`, as (where and which version, iCalendar text), asked for in batches.
    pub fn fetch(&self, hrefs: &[String]) -> Result<Vec<(Remote, String)>, MailError> {
        self.client.multiget(&self.url, hrefs, Kind::Calendar)
    }
}

/// Brings what the server has: new and changed events stored, deleted ones forgotten. Returns how many
/// arrived. Nothing is written to the server yet (answers to invitations come with their own sync).
pub fn sync(calendar: &Calendar, store: &Store) -> Result<usize, MailError> {
    let key = calendar.url.as_str();
    let remote = calendar.list()?;
    let stored: HashMap<String, String> = store.events(key)?.into_iter().map(|e| (e.href, e.etag)).collect();
    let wanted: Vec<String> = remote.iter().filter(|r| stored.get(&r.href) != Some(&r.etag)).map(|r| r.href.clone()).collect();
    let mut received = 0;
    for (event, text) in calendar.fetch(&wanted)? {
        store.put_event(key, &event.href, &event.etag, &text, &span(&text))?;
        received += 1;
    }
    let there: HashSet<&str> = remote.iter().map(|r| r.href.as_str()).collect();
    for gone in stored.keys().filter(|href| !there.contains(href.as_str())) {
        store.forget_event(key, gone)?;
    }
    Ok(received)
}

/// When an iCalendar object happens, for the archive: its first start and the end of its last
/// occurrence, open for one that repeats (worked out when shown, see [`occurrences`]).
pub fn span(text: &str) -> EventSpan {
    let events = parse(text).events;
    let uid = events.first().map(|e| e.uid.clone()).unwrap_or_default();
    let starts: Vec<(DateTime<Utc>, Duration)> = events.iter().filter_map(|e| e.start.as_ref()?.utc().map(|s| (s, length(e, s)))).collect();
    let first = starts.iter().map(|(s, _)| s.timestamp()).min().unwrap_or(0);
    let repeats = events.iter().any(|e| !e.rules.is_empty() || !e.rdates.is_empty());
    let last = (!repeats).then(|| starts.iter().map(|(s, l)| (*s + *l).timestamp()).max().unwrap_or(first));
    EventSpan { uid, first, last }
}

/// One occurrence of an event, in UTC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Occurrence {
    pub uid: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub all_day: bool,
    /// The event it comes from: the repeating one, or its changed occurrence.
    pub event: Event,
}

/// The most occurrences one event gives for a range: a rule that repeats every minute stops here
/// instead of freezing the window.
const MOST: u16 = 1_000;

/// The occurrences of `events` (one calendar object: an event and its changed occurrences, or several
/// events) that overlap `from` to `to`, earliest first. Cancelled ones are left out.
pub fn occurrences(events: &[Event], from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<Occurrence> {
    let overrides: Vec<&Event> = events.iter().filter(|e| e.recurrence_id.is_some()).collect();
    let mut out = Vec::new();
    for event in events.iter().filter(|e| e.recurrence_id.is_none() && e.status != "CANCELLED") {
        let Some(start) = event.start.as_ref() else { continue };
        let Some(first) = start.utc() else { continue };
        let length = length(event, first);
        let starts = if event.rules.is_empty() && event.rdates.is_empty() { vec![first] } else { repeats(event, start, first, from - length, to) };
        // An occurrence changed on its own replaces the one at its original time.
        let replaced: Vec<DateTime<Utc>> = overrides.iter().filter(|o| o.uid == event.uid).filter_map(|o| o.recurrence_id.as_ref()?.utc()).collect();
        for at in starts.into_iter().filter(|at| !replaced.contains(at)) {
            push(&mut out, event, at, length, from, to);
        }
    }
    for event in overrides.into_iter().filter(|e| e.status != "CANCELLED") {
        let Some(at) = event.start.as_ref().and_then(When::utc) else { continue };
        push(&mut out, event, at, length(event, at), from, to);
    }
    out.sort_by_key(|o| (o.start, o.end));
    out
}

fn push(out: &mut Vec<Occurrence>, event: &Event, start: DateTime<Utc>, length: Duration, from: DateTime<Utc>, to: DateTime<Utc>) {
    let end = start + length;
    // An instant event (no length) counts where it starts.
    if start < to && (end > from || (end == start && start >= from)) {
        out.push(Occurrence { uid: event.uid.clone(), start, end, all_day: event.start.as_ref().is_some_and(|s| s.date_only), event: event.clone() });
    }
}

/// How long an event lasts: to its end, for its duration, or a day for a date without an end.
fn length(event: &Event, start: DateTime<Utc>) -> Duration {
    let seconds = match (event.end.as_ref().and_then(When::utc), event.duration) {
        (Some(end), _) => (end - start).num_seconds(),
        (None, Some(d)) => d,
        (None, None) if event.start.as_ref().is_some_and(|s| s.date_only) => 86_400,
        _ => 0,
    };
    Duration::seconds(seconds.max(0))
}

/// The starts of a repeating event between `from` and `to`, in its own zone (so 09:00 stays 09:00
/// across a change of daylight saving). A rule that cannot be read leaves the first occurrence only.
fn repeats(event: &Event, start: &When, first: DateTime<Utc>, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<DateTime<Utc>> {
    let utc = |w: &When| w.utc().map(|t| t.format("%Y%m%dT%H%M%SZ").to_string());
    // A date alone repeats as midnight UTC (see `When::utc`); a known zone as itself; anything else in UTC.
    let dtstart = match (start.date_only, start.tzid.as_deref().and_then(crate::ical::zone)) {
        (false, Some(tz)) if !start.value.ends_with('Z') => format!("DTSTART;TZID={}:{}", tz.name(), start.value),
        _ => format!("DTSTART:{}", first.format("%Y%m%dT%H%M%SZ")),
    };
    // An UNTIL written as a date covers that whole day.
    let rules = event.rules.iter().map(|r| {
        let until = r.split([';', ':']).find_map(|p| p.strip_prefix("UNTIL=")).filter(|u| u.len() == 8);
        until.map_or_else(|| r.clone(), |u| r.replace(&format!("UNTIL={u}"), &format!("UNTIL={u}T235959Z")))
    });
    let mut text = vec![dtstart];
    text.extend(rules);
    text.extend(event.rdates.iter().filter_map(utc).map(|t| format!("RDATE:{t}")));
    text.extend(event.exdates.iter().filter_map(utc).map(|t| format!("EXDATE:{t}")));
    let Ok(set) = text.join("\n").parse::<RRuleSet>() else { return vec![first] };
    let tz = |t: DateTime<Utc>| t.with_timezone(&rrule::Tz::UTC);
    // `after` and `before` are inclusive here; one second earlier keeps an occurrence starting at `from`.
    set.after(tz(from - Duration::seconds(1))).before(tz(to)).all(MOST).dates.into_iter().map(|t| t.with_timezone(&Utc)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ical::parse;
    use chrono::TimeZone;

    fn day(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
    }

    fn starts(text: &str, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<String> {
        occurrences(&parse(text).events, from, to).iter().map(|o| o.start.format("%m-%d %H:%M").to_string()).collect()
    }

    const WEEKLY: &str = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:w\nSUMMARY:Standup\nDTSTART;TZID=Europe/Rome:20261019T090000\nDTEND;TZID=Europe/Rome:20261019T093000\nRRULE:FREQ=WEEKLY;COUNT=4\nEXDATE;TZID=Europe/Rome:20261102T090000\nEND:VEVENT\nBEGIN:VEVENT\nUID:w\nRECURRENCE-ID;TZID=Europe/Rome:20261026T090000\nSUMMARY:Standup, later\nDTSTART;TZID=Europe/Rome:20261026T110000\nDTEND;TZID=Europe/Rome:20261026T113000\nEND:VEVENT\nEND:VCALENDAR\n";

    #[test]
    fn a_weekly_event_keeps_its_hour_across_daylight_saving_with_its_exceptions() {
        // Rome leaves summer time on 25 October: 09:00 is 07:00 UTC before, 08:00 after.
        assert_eq!(starts(WEEKLY, day(2026, 10, 1), day(2026, 12, 1)), ["10-19 07:00", "10-26 10:00", "11-09 08:00"], "moved on the 26th, none on 2 November");
        let moved = occurrences(&parse(WEEKLY).events, day(2026, 10, 26), day(2026, 10, 27));
        assert_eq!(moved[0].event.summary, "Standup, later");
        assert_eq!(starts(WEEKLY, day(2026, 11, 3), day(2026, 11, 9)), Vec::<String>::new(), "a range between occurrences has none");
    }

    #[test]
    fn all_day_events_ongoing_ones_and_broken_rules_are_placed() {
        let all_day = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:a\nDTSTART;VALUE=DATE:20261012\nDTEND;VALUE=DATE:20261014\nRRULE:FREQ=YEARLY;UNTIL=20271012\nEND:VEVENT\nEND:VCALENDAR\n";
        let found = occurrences(&parse(all_day).events, day(2026, 10, 13), day(2026, 10, 14));
        assert_eq!((found.len(), found[0].all_day, found[0].end), (1, true, day(2026, 10, 14)), "two days long, seen from its second day");
        assert_eq!(starts(all_day, day(2027, 1, 1), day(2028, 1, 1)), ["10-12 00:00"], "an UNTIL date takes in its day");
        let broken = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:b\nDTSTART:20261012T090000Z\nDURATION:PT1H\nRRULE:FREQ=SOMETIMES\nEND:VEVENT\nEND:VCALENDAR\n";
        assert_eq!(starts(broken, day(2026, 10, 1), day(2026, 11, 1)), ["10-12 09:00"]);
        let every_minute = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:m\nDTSTART:20261012T000000Z\nRRULE:FREQ=MINUTELY\nEND:VEVENT\nEND:VCALENDAR\n";
        assert_eq!(starts(every_minute, day(2026, 10, 12), day(2026, 10, 20)).len(), usize::from(MOST), "capped");
        let cancelled = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:c\nSTATUS:CANCELLED\nDTSTART:20261012T090000Z\nEND:VEVENT\nEND:VCALENDAR\n";
        assert!(starts(cancelled, day(2026, 10, 1), day(2026, 11, 1)).is_empty());
    }
}
