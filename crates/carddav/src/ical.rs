// SPDX-License-Identifier: AGPL-3.0-only

//! iCalendar (RFC 5545): what BeaverMail shows of the events of a calendar object or of an invitation.
//! Lines are read as vCard's are (folding, quoted parameters, escapes); alarms, time zone definitions
//! and other components are skipped.

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};

use crate::vcard::{escape, split_unquoted, unescape, unfold};

/// A calendar object, or the calendar part of a message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Calendar {
    /// What a message carrying it asks (iTIP, RFC 5546): `REQUEST`, `REPLY`, `CANCEL`, `PUBLISH`…; empty
    /// for an event as a calendar keeps it.
    pub method: String,
    pub events: Vec<Event>,
}

/// One `VEVENT`: an event, or one occurrence of a repeating event changed on its own (`recurrence_id`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Event {
    pub uid: String,
    pub recurrence_id: Option<When>,
    pub summary: String,
    pub location: String,
    pub description: String,
    pub start: Option<When>,
    pub end: Option<When>,
    /// `DURATION`, in seconds, when there is no end.
    pub duration: Option<i64>,
    /// `RRULE` and `RDATE` lines as written, and the `EXDATE` times.
    pub rules: Vec<String>,
    pub rdates: Vec<When>,
    pub exdates: Vec<When>,
    /// `CONFIRMED`, `TENTATIVE` or `CANCELLED`; empty when not said.
    pub status: String,
    pub sequence: i64,
    pub organizer: Option<Person>,
    pub attendees: Vec<Person>,
}

/// An organiser or an attendee.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Person {
    /// Lower case, without `mailto:`.
    pub email: String,
    pub name: String,
    /// Their answer: `ACCEPTED`, `DECLINED`, `TENTATIVE`, `NEEDS-ACTION`…; empty when not said.
    pub partstat: String,
}

/// A date or a date and time as written: in UTC (`Z`), in a named zone, floating (this computer's time),
/// or a date alone (an all-day event).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct When {
    /// `20261012T090000Z`, `20261012T090000` or `20261012`.
    pub value: String,
    pub tzid: Option<String>,
    pub date_only: bool,
}

impl When {
    /// The moment it stands for. A date alone is its midnight in UTC, so that it stays the same day
    /// wherever it is shown; a zone that is not known is taken as this computer's.
    pub fn utc(&self) -> Option<DateTime<Utc>> {
        if self.date_only {
            let date = NaiveDate::parse_from_str(self.value.get(..8)?, "%Y%m%d").ok()?;
            return Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?));
        }
        let naive = NaiveDateTime::parse_from_str(self.value.trim_end_matches('Z'), "%Y%m%dT%H%M%S").ok()?;
        if self.value.ends_with('Z') {
            return Some(Utc.from_utc_datetime(&naive));
        }
        match self.tzid.as_deref().and_then(zone) {
            Some(tz) => tz.from_local_datetime(&naive).earliest().map(|t| t.with_timezone(&Utc)),
            None => Local.from_local_datetime(&naive).earliest().map(|t| t.with_timezone(&Utc)),
        }
    }
}

/// A time zone by the name a calendar gives it: an IANA name, one with a path in front (as some clients
/// write it), or a Windows name.
pub fn zone(name: &str) -> Option<chrono_tz::Tz> {
    let name = name.trim().trim_matches('"');
    if let Ok(tz) = name.parse::<chrono_tz::Tz>() {
        return Some(tz);
    }
    // "/mozilla.org/20050126_1/Europe/Rome": the last two parts.
    let parts: Vec<&str> = name.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() >= 2
        && let Ok(tz) = parts[parts.len() - 2..].join("/").parse::<chrono_tz::Tz>()
    {
        return Some(tz);
    }
    WINDOWS.iter().find(|(windows, _)| windows.eq_ignore_ascii_case(name)).and_then(|(_, iana)| iana.parse().ok())
}

/// Windows zone names as some servers write them, with the IANA zone of their main city.
// shortcut: the zones mail is most often sent from; add the others from CLDR's windowsZones if one is missed.
const WINDOWS: [(&str, &str); 40] = [
    ("UTC", "UTC"),
    ("GMT Standard Time", "Europe/London"),
    ("Greenwich Standard Time", "Atlantic/Reykjavik"),
    ("W. Europe Standard Time", "Europe/Berlin"),
    ("Romance Standard Time", "Europe/Paris"),
    ("Central Europe Standard Time", "Europe/Budapest"),
    ("Central European Standard Time", "Europe/Warsaw"),
    ("E. Europe Standard Time", "Europe/Chisinau"),
    ("FLE Standard Time", "Europe/Kiev"),
    ("GTB Standard Time", "Europe/Bucharest"),
    ("Russian Standard Time", "Europe/Moscow"),
    ("Turkey Standard Time", "Europe/Istanbul"),
    ("Israel Standard Time", "Asia/Jerusalem"),
    ("Arabian Standard Time", "Asia/Dubai"),
    ("India Standard Time", "Asia/Kolkata"),
    ("China Standard Time", "Asia/Shanghai"),
    ("Singapore Standard Time", "Asia/Singapore"),
    ("Tokyo Standard Time", "Asia/Tokyo"),
    ("Korea Standard Time", "Asia/Seoul"),
    ("AUS Eastern Standard Time", "Australia/Sydney"),
    ("New Zealand Standard Time", "Pacific/Auckland"),
    ("Eastern Standard Time", "America/New_York"),
    ("Central Standard Time", "America/Chicago"),
    ("Mountain Standard Time", "America/Denver"),
    ("US Mountain Standard Time", "America/Phoenix"),
    ("Pacific Standard Time", "America/Los_Angeles"),
    ("Alaskan Standard Time", "America/Anchorage"),
    ("Hawaiian Standard Time", "Pacific/Honolulu"),
    ("Atlantic Standard Time", "America/Halifax"),
    ("Canada Central Standard Time", "America/Regina"),
    ("Mexico Standard Time", "America/Mexico_City"),
    ("Central America Standard Time", "America/Guatemala"),
    ("SA Pacific Standard Time", "America/Bogota"),
    ("Argentina Standard Time", "America/Buenos_Aires"),
    ("E. South America Standard Time", "America/Sao_Paulo"),
    ("South Africa Standard Time", "Africa/Johannesburg"),
    ("Egypt Standard Time", "Africa/Cairo"),
    ("W. Central Africa Standard Time", "Africa/Lagos"),
    ("Morocco Standard Time", "Africa/Casablanca"),
    ("Coordinated Universal Time", "UTC"),
];

/// A property's parameters: names upper case, values without quotes.
type Params = Vec<(String, String)>;

/// A property's name (upper case), its parameters and value.
fn property(line: &str) -> Option<(String, Params, &str)> {
    let (head, value) = split_unquoted(line)?;
    let mut parts = split_params(head).into_iter();
    let name = parts.next()?.to_ascii_uppercase();
    let params = parts.filter_map(|p| p.split_once('=').map(|(k, v)| (k.to_ascii_uppercase(), v.trim_matches('"').to_owned()))).collect();
    Some((name, params, value))
}

/// `NAME;A=1;B="x;y"` at its `;`s that are not inside quotes.
fn split_params(head: &str) -> Vec<&str> {
    let (mut out, mut quoted, mut from) = (Vec::new(), false, 0);
    for (i, c) in head.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => {
                out.push(&head[from..i]);
                from = i + 1;
            }
            _ => {}
        }
    }
    out.push(&head[from..]);
    out
}

fn param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn when(params: &[(String, String)], value: &str) -> When {
    let value = value.trim().to_owned();
    When { date_only: param(params, "VALUE") == Some("DATE") || (value.len() == 8 && !value.contains('T')), tzid: param(params, "TZID").map(str::to_owned), value }
}

fn person(params: &[(String, String)], value: &str) -> Person {
    let email = value.trim();
    let email = email.get(..7).filter(|p| p.eq_ignore_ascii_case("mailto:")).map_or(email, |_| &email[7..]);
    Person { email: email.to_lowercase(), name: param(params, "CN").unwrap_or_default().to_owned(), partstat: param(params, "PARTSTAT").unwrap_or_default().to_ascii_uppercase() }
}

/// An ISO 8601 duration as iCalendar writes it (`PT1H30M`, `P1D`, `-PT15M`, `P1W`), in seconds.
pub fn duration(text: &str) -> Option<i64> {
    let text = text.trim();
    let (sign, text) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.trim_start_matches('+')),
    };
    let mut rest = text.strip_prefix('P')?;
    let (mut total, mut in_time, mut number) = (0i64, false, String::new());
    while let Some(c) = rest.chars().next() {
        rest = &rest[c.len_utf8()..];
        match c {
            'T' => in_time = true,
            '0'..='9' => number.push(c),
            unit => {
                let n: i64 = std::mem::take(&mut number).parse().ok()?;
                total += n * match (unit, in_time) {
                    ('W', _) => 7 * 86_400,
                    ('D', _) => 86_400,
                    ('H', true) => 3_600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => return None,
                };
            }
        }
    }
    Some(sign * total)
}

/// The events of an iCalendar text, with the method it carries.
pub fn parse(text: &str) -> Calendar {
    let mut calendar = Calendar::default();
    // Components open now: only the properties of a VEVENT itself (not of an alarm inside it) count.
    let mut open: Vec<String> = Vec::new();
    let mut event: Option<Event> = None;
    for line in unfold(text) {
        let Some((name, params, value)) = property(&line) else { continue };
        match name.as_str() {
            "BEGIN" => {
                let component = value.trim().to_ascii_uppercase();
                if component == "VEVENT" && open.last().is_some_and(|c| c == "VCALENDAR") {
                    event = Some(Event::default());
                }
                open.push(component);
                continue;
            }
            "END" => {
                if open.pop().as_deref() == Some("VEVENT")
                    && let Some(done) = event.take()
                {
                    calendar.events.push(done);
                }
                continue;
            }
            "METHOD" if open.last().is_some_and(|c| c == "VCALENDAR") => calendar.method = value.trim().to_ascii_uppercase(),
            _ => {}
        }
        let Some(e) = event.as_mut().filter(|_| open.last().is_some_and(|c| c == "VEVENT")) else { continue };
        match name.as_str() {
            "UID" => e.uid = value.trim().to_owned(),
            "SUMMARY" => e.summary = unescape(value),
            "LOCATION" => e.location = unescape(value),
            "DESCRIPTION" => e.description = unescape(value),
            "DTSTART" => e.start = Some(when(&params, value)),
            "DTEND" => e.end = Some(when(&params, value)),
            "DURATION" => e.duration = duration(value),
            "RECURRENCE-ID" => e.recurrence_id = Some(when(&params, value)),
            "RRULE" => e.rules.push(format!("RRULE:{}", value.trim())),
            "RDATE" => e.rdates.extend(value.split(',').map(|v| when(&params, v))),
            "EXDATE" => e.exdates.extend(value.split(',').map(|v| when(&params, v))),
            "STATUS" => e.status = value.trim().to_ascii_uppercase(),
            "SEQUENCE" => e.sequence = value.trim().parse().unwrap_or(0),
            "ORGANIZER" => e.organizer = Some(person(&params, value)),
            "ATTENDEE" => e.attendees.push(person(&params, value)),
            _ => {}
        }
    }
    calendar
}

/// An answer to an invitation (iTIP `REPLY`, RFC 5546): the event's identity and times, and the one
/// attendee answering, `me`, with `partstat` (`ACCEPTED`, `TENTATIVE` or `DECLINED`). The time zone
/// definitions the times need come along. `None` when the text has no event.
pub fn reply(text: &str, me: &str, name: &str, partstat: &str, now: DateTime<Utc>) -> Option<String> {
    const KEPT: [&str; 9] = ["UID", "RECURRENCE-ID", "SEQUENCE", "DTSTART", "DTEND", "DURATION", "SUMMARY", "ORGANIZER", "LOCATION"];
    let mut out = vec!["BEGIN:VCALENDAR".to_owned(), "VERSION:2.0".to_owned(), "PRODID:-//BeaverMail//Calendar//EN".to_owned(), "METHOD:REPLY".to_owned()];
    let (mut open, mut events) = (Vec::<String>::new(), 0);
    for line in unfold(text) {
        let Some((head, _)) = split_unquoted(&line) else { continue };
        let property = split_params(head)[0].to_ascii_uppercase();
        let value = line.split_once(':').map(|(_, v)| v.trim().to_ascii_uppercase()).unwrap_or_default();
        match property.as_str() {
            "BEGIN" => open.push(value.clone()),
            "END" => {
                if open.last().is_some_and(|c| *c == "VEVENT") {
                    out.push(format!("ATTENDEE;PARTSTAT={partstat}{}:mailto:{me}", if name.is_empty() { String::new() } else { format!(";CN=\"{}\"", name.replace('"', "'")) }));
                    out.push(format!("DTSTAMP:{}", now.format("%Y%m%dT%H%M%SZ")));
                    events += 1;
                }
                if open.last().is_some_and(|c| c == "VTIMEZONE" || c == "VEVENT") || open.iter().any(|c| c == "VTIMEZONE") {
                    out.push(line.clone());
                }
                open.pop();
                continue;
            }
            _ => {}
        }
        let inside = open.iter().any(|c| c == "VTIMEZONE") || (open.last().is_some_and(|c| c == "VEVENT") && KEPT.contains(&property.as_str()));
        if inside || (property == "BEGIN" && (value == "VEVENT" || open.iter().any(|c| c == "VTIMEZONE"))) {
            out.push(line);
        }
    }
    out.push("END:VCALENDAR".to_owned());
    (events > 0).then(|| fold(&out))
}

/// The event as a calendar keeps it, with `me` answering `partstat`: the iTIP method taken off, my
/// attendee line with the answer (added when the invitation reached me another way, as a list).
pub fn with_answer(text: &str, me: &str, partstat: &str) -> String {
    let mut out = Vec::new();
    let mut found = false;
    let mut in_event = 0;
    for line in unfold(text) {
        let Some((head, value)) = split_unquoted(&line) else { continue };
        let parts = split_params(head);
        let property = parts[0].to_ascii_uppercase();
        match property.as_str() {
            "METHOD" => continue,
            "BEGIN" if value.trim().eq_ignore_ascii_case("VEVENT") => in_event += 1,
            "END" if value.trim().eq_ignore_ascii_case("VEVENT") => {
                if !found {
                    out.push(format!("ATTENDEE;PARTSTAT={partstat}:mailto:{me}"));
                }
                found = false;
                in_event -= 1;
            }
            "ATTENDEE" if in_event > 0 && person(&[], value).email == me => {
                found = true;
                let params: Vec<&str> = parts[1..].iter().copied().filter(|p| !p.to_ascii_uppercase().starts_with("PARTSTAT=") && !p.to_ascii_uppercase().starts_with("RSVP=")).collect();
                let params = params.iter().map(|p| format!(";{p}")).collect::<String>();
                out.push(format!("ATTENDEE{params};PARTSTAT={partstat}:{value}"));
                continue;
            }
            _ => {}
        }
        out.push(line);
    }
    fold(&out)
}

/// An event sent to be added (iTIP `PUBLISH`) as a calendar keeps it: without the method.
pub fn as_kept(text: &str) -> String {
    fold(&unfold(text).into_iter().filter(|l| !l.to_ascii_uppercase().starts_with("METHOD:")).collect::<Vec<_>>())
}

/// What the user writes of an event of their own: its title, place, notes and times. An all-day event
/// has dates (its end the day after its last), a timed one moments in UTC.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    pub summary: String,
    pub location: String,
    pub description: String,
    pub start: When,
    pub end: When,
}

/// The properties a [`Draft`] sets; an event written over keeps all the others (people, alarms…).
const DRAFTED: [&str; 9] = ["SUMMARY", "LOCATION", "DESCRIPTION", "DTSTART", "DTEND", "DURATION", "DTSTAMP", "LAST-MODIFIED", "SEQUENCE"];

/// The calendar object for `draft`: a new event with this `uid`, or, given the text kept (`old`), that
/// event with the draft's values in place of its own and its sequence one higher, the rest untouched.
pub fn written(old: Option<&str>, uid: &str, draft: &Draft, now: DateTime<Utc>) -> String {
    let stamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    let when = |name: &str, w: &When| if w.date_only { format!("{name};VALUE=DATE:{}", w.value) } else { format!("{name}:{}", w.value) };
    let mut drafted = vec![format!("SUMMARY:{}", escape(draft.summary.trim())), when("DTSTART", &draft.start), when("DTEND", &draft.end)];
    for (name, value) in [("LOCATION", &draft.location), ("DESCRIPTION", &draft.description)] {
        if !value.trim().is_empty() {
            drafted.push(format!("{name}:{}", escape(value.trim())));
        }
    }
    drafted.extend([format!("DTSTAMP:{stamp}"), format!("LAST-MODIFIED:{stamp}")]);
    let Some(old) = old else {
        let mut out = vec!["BEGIN:VCALENDAR".to_owned(), "VERSION:2.0".to_owned(), "PRODID:-//BeaverMail//Calendar//EN".to_owned(), "BEGIN:VEVENT".to_owned(), format!("UID:{uid}"), format!("CREATED:{stamp}"), "SEQUENCE:0".to_owned()];
        out.extend(drafted);
        out.extend(["END:VEVENT".to_owned(), "END:VCALENDAR".to_owned()]);
        return fold(&out);
    };
    let sequence = parse(old).events.first().map_or(0, |e| e.sequence) + 1;
    let (mut out, mut open) = (Vec::new(), Vec::<String>::new());
    for line in unfold(old) {
        let Some((head, value)) = split_unquoted(&line) else { continue };
        let name = split_params(head)[0].to_ascii_uppercase();
        let in_event = open.last().is_some_and(|c| c == "VEVENT");
        match name.as_str() {
            "BEGIN" => open.push(value.trim().to_ascii_uppercase()),
            "END" => {
                if in_event {
                    out.extend(drafted.iter().cloned());
                    out.push(format!("SEQUENCE:{sequence}"));
                }
                open.pop();
            }
            _ if in_event && DRAFTED.contains(&name.as_str()) => continue,
            _ => {}
        }
        out.push(line);
    }
    fold(&out)
}

/// Lines folded at 75 octets (RFC 5545 3.1), never inside a character, ending in CRLF.
fn fold(lines: &[String]) -> String {
    let mut out = String::new();
    for line in lines {
        let mut rest = line.as_str();
        let mut first = true;
        while !rest.is_empty() {
            let room = if first { 75 } else { 74 };
            let mut cut = rest.len().min(room);
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            if !first {
                out.push(' ');
            }
            out.push_str(&rest[..cut]);
            out.push_str("\r\n");
            rest = &rest[cut..];
            first = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVITE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Rome\r\nBEGIN:STANDARD\r\nDTSTART:19701025T030000\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:plan-42@example.org\r\nSUMMARY:Plan review\\, Q4\r\nLOCATION:Room 2\r\nDESCRIPTION:Agenda:\\n1. budget\r\nDTSTART;TZID=Europe/Rome:20261012T090000\r\nDTEND;TZID=Europe/Rome:20261012T100000\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nEXDATE;TZID=Europe/Rome:20261019T090000\r\nORGANIZER;CN=\"Ada, Lovelace\":mailto:Ada@Example.org\r\nATTENDEE;CN=Bob;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:MAILTO:bob@example.org\r\nSEQUENCE:2\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:not the event's\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn an_invitation_is_read_with_its_people_and_its_rule() {
        let calendar = parse(INVITE);
        assert_eq!(calendar.method, "REQUEST");
        let [e] = &calendar.events[..] else { panic!("one event: {calendar:?}") };
        assert_eq!((e.uid.as_str(), e.summary.as_str(), e.description.as_str()), ("plan-42@example.org", "Plan review, Q4", "Agenda:\n1. budget"));
        assert_eq!(e.organizer.as_ref().map(|p| (p.email.as_str(), p.name.as_str())), Some(("ada@example.org", "Ada, Lovelace")));
        assert_eq!((e.attendees[0].email.as_str(), e.attendees[0].partstat.as_str()), ("bob@example.org", "NEEDS-ACTION"));
        assert_eq!((e.rules.as_slice(), e.exdates.len(), e.sequence), (&["RRULE:FREQ=WEEKLY;COUNT=4".to_owned()][..], 1, 2));
        // 09:00 in Rome in October is 07:00 UTC.
        assert_eq!(e.start.as_ref().and_then(When::utc).map(|t| t.to_rfc3339()), Some("2026-10-12T07:00:00+00:00".into()));
    }

    #[test]
    fn an_answer_names_the_event_and_me_only_and_the_kept_copy_carries_it() {
        let now = Utc.with_ymd_and_hms(2026, 10, 1, 8, 0, 0).unwrap();
        let reply = reply(INVITE, "bob@example.org", "Bob", "ACCEPTED", now).unwrap();
        let read = parse(&reply);
        assert_eq!(read.method, "REPLY");
        let e = &read.events[0];
        assert_eq!((e.uid.as_str(), e.sequence, e.summary.as_str()), ("plan-42@example.org", 2, "Plan review, Q4"));
        assert_eq!(e.attendees.iter().map(|a| (a.email.as_str(), a.partstat.as_str())).collect::<Vec<_>>(), [("bob@example.org", "ACCEPTED")], "only the one answering");
        assert!(reply.contains("BEGIN:VTIMEZONE") && reply.contains("DTSTAMP:20261001T080000Z") && !reply.contains("RRULE") && !reply.contains("VALARM"), "{reply}");
        assert!(reply.lines().all(|l| l.len() <= 76), "folded: {reply}");
        let kept = with_answer(INVITE, "bob@example.org", "TENTATIVE");
        let read = parse(&kept);
        assert_eq!((read.method.as_str(), read.events[0].attendees[0].partstat.as_str()), ("", "TENTATIVE"));
        assert!(!kept.contains("RSVP=TRUE") && kept.contains("RRULE:FREQ=WEEKLY"), "the rest as it was: {kept}");
        let elsewhere = parse(&with_answer(INVITE, "carl@example.org", "ACCEPTED"));
        assert_eq!(elsewhere.events[0].attendees.len(), 2, "added when not invited by name");
    }

    #[test]
    fn a_new_event_is_written_whole_and_an_edited_one_keeps_what_the_draft_does_not_set() {
        let now = Utc.with_ymd_and_hms(2026, 10, 1, 8, 0, 0).unwrap();
        let draft = Draft {
            summary: "Lunch, with Ada".into(),
            location: "Café; upstairs".into(),
            description: "Bring\nthe plan".replace("\\n", "\n"),
            start: When { value: "20261012T110000Z".into(), tzid: None, date_only: false },
            end: When { value: "20261012T120000Z".into(), tzid: None, date_only: false },
        };
        let new = written(None, "lunch-1@example.org", &draft, now);
        let e = &parse(&new).events[0];
        assert_eq!((e.uid.as_str(), e.summary.as_str(), e.location.as_str(), e.sequence), ("lunch-1@example.org", "Lunch, with Ada", "Café; upstairs", 0));
        assert_eq!(e.end.as_ref().and_then(When::utc).map(|t| t.to_rfc3339()), Some("2026-10-12T12:00:00+00:00".into()));
        assert!(new.contains("DTSTAMP:20261001T080000Z") && new.ends_with("END:VCALENDAR\r\n"), "{new}");

        let all_day = Draft { summary: "Off".into(), start: When { value: "20261012".into(), tzid: None, date_only: true }, end: When { value: "20261014".into(), tzid: None, date_only: true }, ..Default::default() };
        let edited = written(Some(INVITE), "plan-42@example.org", &all_day, now);
        let read = parse(&edited);
        let e = &read.events[0];
        assert_eq!((e.summary.as_str(), e.location.as_str(), e.sequence), ("Off", "", 3), "the draft's values, one sequence on");
        assert!(e.start.as_ref().is_some_and(|s| s.date_only && s.value == "20261012"), "{edited}");
        assert_eq!((e.attendees.len(), e.organizer.is_some()), (1, true), "the people stay");
        assert!(edited.contains("BEGIN:VALARM") && edited.contains("TRIGGER:-PT15M") && edited.contains("BEGIN:VTIMEZONE"), "the rest as it was: {edited}");
        assert!(!edited.contains("Agenda") && !edited.contains("Room 2"), "what the draft leaves empty goes: {edited}");
        assert_eq!(edited.matches("SUMMARY:").count(), 1, "the alarm's own lines are not the event's: {edited}");
    }

    #[test]
    fn times_and_durations_are_read_as_written() {
        let date = When { value: "20261012".into(), date_only: true, tzid: None };
        assert_eq!(date.utc().map(|t| t.to_rfc3339()), Some("2026-10-12T00:00:00+00:00".into()));
        let windows = When { value: "20260115T090000".into(), tzid: Some("W. Europe Standard Time".into()), date_only: false };
        assert_eq!(windows.utc().map(|t| t.to_rfc3339()), Some("2026-01-15T08:00:00+00:00".into()));
        assert_eq!(zone("/mozilla.org/20050126_1/Europe/Rome"), Some(chrono_tz::Europe::Rome));
        assert_eq!([duration("PT1H30M"), duration("P1D"), duration("-PT15M"), duration("P1W"), duration("P1DT2H"), duration("1H")], [Some(5_400), Some(86_400), Some(-900), Some(604_800), Some(93_600), None]);
    }
}
