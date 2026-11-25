// SPDX-License-Identifier: AGPL-3.0-only

//! Tasks (`VTODO`, RFC 5545 3.6.2): what BeaverMail shows of one, and changes that rewrite only the
//! properties they touch, so alarms, unknown properties and everything else come back as they were.

use chrono::{DateTime, Duration, Local, TimeZone, Utc};

use crate::calendar::occurrences;
use crate::ical::{Event, Params, When, fold, param, property, split_params, when, zone};
use crate::vcard::{escape, split_unquoted, unescape, unfold};

/// One task, as read from its calendar object: the task itself, not an occurrence changed on its own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Task {
    pub uid: String,
    pub summary: String,
    pub description: String,
    pub due: Option<When>,
    pub start: Option<When>,
    /// Done, by any of the three ways it is said (`STATUS`, `COMPLETED`, `PERCENT-COMPLETE`).
    pub done: bool,
    pub completed: Option<When>,
    /// 1 (highest) to 9 (lowest), 0 for none.
    pub priority: u8,
    pub categories: Vec<String>,
    /// The `UID` of the task it is a part of.
    pub parent: Option<String>,
    /// The order it was put in by hand (`X-APPLE-SORT-ORDER`).
    pub sort: Option<i64>,
    pub created: Option<When>,
    /// `RRULE` lines as written, `RDATE` and `EXDATE` times.
    pub rules: Vec<String>,
    pub rdates: Vec<When>,
    pub exdates: Vec<When>,
    /// `URL`: for a task made from a message, `mid:` and its Message-ID.
    pub url: String,
    /// Reminders were set elsewhere.
    pub alarms: bool,
}

impl Task {
    pub fn repeats(&self) -> bool {
        !self.rules.is_empty() || !self.rdates.is_empty()
    }

    /// Where it goes in a list put in order by hand: its sort order, else seconds from 2001-01-01 UTC to
    /// when it was made, as other clients count it.
    pub fn order(&self) -> i64 {
        self.sort.unwrap_or_else(|| self.created.as_ref().and_then(When::utc).map_or(0, |c| c.timestamp() - 978_307_200))
    }

    /// The Message-ID of the message it was made from (without `<>`).
    pub fn message_id(&self) -> Option<&str> {
        let url = self.url.trim();
        let id = url.get(..4).filter(|p| p.eq_ignore_ascii_case("mid:")).map(|_| &url[4..])?;
        Some(id.trim_matches(['<', '>'])).filter(|id| !id.is_empty())
    }
}

/// The task of a calendar object: the first `VTODO` that is not an occurrence changed on its own;
/// `None` when it holds none.
pub fn parse(text: &str) -> Option<Task> {
    // Each task read, with whether it is an occurrence changed on its own.
    let mut found: Vec<(bool, Task)> = Vec::new();
    let (mut open, mut current) = (Vec::<String>::new(), None::<(bool, Task)>);
    for line in unfold(text) {
        let Some((name, params, value)) = property(&line) else { continue };
        match name.as_str() {
            "BEGIN" => {
                let component = value.trim().to_ascii_uppercase();
                if component == "VTODO" && open.last().is_some_and(|c| c == "VCALENDAR") {
                    current = Some((false, Task::default()));
                }
                if component == "VALARM"
                    && open.last().is_some_and(|c| c == "VTODO")
                    && let Some((_, task)) = current.as_mut()
                {
                    task.alarms = true;
                }
                open.push(component);
                continue;
            }
            "END" => {
                if open.pop().as_deref() == Some("VTODO")
                    && let Some(done) = current.take()
                {
                    found.push(done);
                }
                continue;
            }
            _ => {}
        }
        let Some((overrides, t)) = current.as_mut().filter(|_| open.last().is_some_and(|c| c == "VTODO")) else { continue };
        read(t, overrides, &name, &params, value);
    }
    let first = found.iter().position(|(overrides, _)| !overrides).unwrap_or(0);
    (!found.is_empty()).then(|| found.swap_remove(first).1)
}

fn read(t: &mut Task, overrides: &mut bool, name: &str, params: &Params, value: &str) {
    match name {
        "UID" => t.uid = value.trim().to_owned(),
        "SUMMARY" => t.summary = unescape(value),
        "DESCRIPTION" => t.description = unescape(value),
        "DUE" => t.due = Some(when(params, value)),
        "DTSTART" => t.start = Some(when(params, value)),
        "COMPLETED" => {
            t.completed = Some(when(params, value));
            t.done = true;
        }
        "STATUS" if value.trim().eq_ignore_ascii_case("COMPLETED") => t.done = true,
        "PERCENT-COMPLETE" if value.trim() == "100" => t.done = true,
        "PRIORITY" => t.priority = value.trim().parse::<u8>().unwrap_or(0).min(9),
        "CATEGORIES" => t.categories.extend(list(value).into_iter().filter(|c| !c.is_empty())),
        "RELATED-TO" if param(params, "RELTYPE").is_none_or(|r| r.eq_ignore_ascii_case("PARENT")) => t.parent = Some(value.trim().to_owned()).filter(|p| !p.is_empty()),
        "X-APPLE-SORT-ORDER" => t.sort = value.trim().parse().ok(),
        "CREATED" => t.created = Some(when(params, value)),
        "RRULE" => t.rules.push(format!("RRULE:{}", value.trim())),
        "RDATE" => t.rdates.extend(value.split(',').map(|v| when(params, v))),
        "EXDATE" => t.exdates.extend(value.split(',').map(|v| when(params, v))),
        "URL" => t.url = value.trim().to_owned(),
        "RECURRENCE-ID" => *overrides = true,
        _ => {}
    }
}

/// What the archive keeps of a task to find it. A due day is due at the end of that day here.
pub fn fields(text: &str) -> beavermail_store::TaskFields {
    let task = parse(text).unwrap_or_default();
    let due = task.due.as_ref().and_then(|d| match d.date_only {
        true => chrono::NaiveDate::parse_from_str(d.value.get(..8)?, "%Y%m%d").ok()?.succ_opt()?.and_hms_opt(0, 0, 0).and_then(|t| Local.from_local_datetime(&t).earliest()).map(|t| t.timestamp()),
        false => d.utc().map(|t| t.timestamp()),
    });
    beavermail_store::TaskFields { message: task.message_id().unwrap_or_default().to_owned(), uid: task.uid, parent: task.parent.unwrap_or_default(), due, done: task.done }
}

/// Whether a calendar object is a task rather than an event.
pub fn is_task(text: &str) -> bool {
    let components: Vec<String> = unfold(text).iter().filter_map(|l| l.get(..6).filter(|b| b.eq_ignore_ascii_case("BEGIN:")).map(|_| l[6..].trim().to_ascii_uppercase())).collect();
    components.iter().any(|c| c == "VTODO") && !components.iter().any(|c| c == "VEVENT")
}

/// A text list (`CATEGORIES`) at its commas that are not escaped, each unescaped.
fn list(value: &str) -> Vec<String> {
    let (mut out, mut item, mut escaped) = (Vec::new(), String::new(), false);
    for c in value.chars() {
        match (c, escaped) {
            (',', false) => out.push(unescape(std::mem::take(&mut item).trim())),
            _ => {
                escaped = c == '\\' && !escaped;
                item.push(c);
            }
        }
    }
    out.push(unescape(item.trim()));
    out
}

/// A change to a task: the properties it replaces, and the lines it writes in their place (none to
/// remove them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub names: Vec<&'static str>,
    pub lines: Vec<String>,
}

fn text_line(name: &'static str, value: &str) -> Edit {
    let value = value.trim();
    Edit { names: vec![name], lines: if value.is_empty() { Vec::new() } else { vec![format!("{name}:{}", escape(value))] } }
}

fn stamp(t: DateTime<Utc>) -> String {
    t.format("%Y%m%dT%H%M%SZ").to_string()
}

fn when_line(name: &str, w: &When) -> String {
    match (&w.tzid, w.date_only) {
        (_, true) => format!("{name};VALUE=DATE:{}", w.value),
        (Some(tz), false) => format!("{name};TZID={tz}:{}", w.value),
        (None, false) => format!("{name}:{}", w.value),
    }
}

impl Edit {
    pub fn summary(text: &str) -> Self {
        text_line("SUMMARY", text)
    }

    pub fn description(text: &str) -> Self {
        text_line("DESCRIPTION", text)
    }

    /// The due date, or none. A start the new due would contradict (another kind of value, or not before
    /// it) goes, and so does a duration: neither may stand with a due as written.
    pub fn due(task: &Task, due: Option<&When>) -> Self {
        let mut names = vec!["DUE", "DURATION"];
        let clashes = |start: &When, due: &When| start.date_only != due.date_only || start.utc().zip(due.utc()).is_none_or(|(s, d)| s >= d);
        if let (Some(start), Some(due)) = (&task.start, due)
            && clashes(start, due)
        {
            names.push("DTSTART");
        }
        Self { names, lines: due.map(|d| when_line("DUE", d)).into_iter().collect() }
    }

    /// Done (when, now) or open again, said all three ways.
    pub fn done(done: bool, now: DateTime<Utc>) -> Self {
        let names = vec!["STATUS", "COMPLETED", "PERCENT-COMPLETE"];
        let lines = if done { vec!["STATUS:COMPLETED".to_owned(), format!("COMPLETED:{}", stamp(now)), "PERCENT-COMPLETE:100".to_owned()] } else { vec!["STATUS:NEEDS-ACTION".to_owned()] };
        Self { names, lines }
    }

    /// 1 (highest) to 9 (lowest), 0 for none.
    pub fn priority(priority: u8) -> Self {
        Self { names: vec!["PRIORITY"], lines: (priority > 0).then(|| format!("PRIORITY:{}", priority.min(9))).into_iter().collect() }
    }

    pub fn categories(labels: &[String]) -> Self {
        let labels: Vec<String> = labels.iter().map(|l| escape(l.trim())).filter(|l| !l.is_empty()).collect();
        Self { names: vec!["CATEGORIES"], lines: (!labels.is_empty()).then(|| format!("CATEGORIES:{}", labels.join(","))).into_iter().collect() }
    }

    /// The task it is a part of, or none. Other relations (`RELTYPE=CHILD`, `SIBLING`) stay.
    pub fn parent(uid: Option<&str>) -> Self {
        Self { names: vec!["RELATED-TO"], lines: uid.map(str::trim).filter(|u| !u.is_empty()).map(|u| format!("RELATED-TO:{u}")).into_iter().collect() }
    }

    pub fn sort(order: i64) -> Self {
        Self { names: vec!["X-APPLE-SORT-ORDER"], lines: vec![format!("X-APPLE-SORT-ORDER:{order}")] }
    }

    /// The message it is made from, by its Message-ID.
    pub fn message(message_id: &str) -> Self {
        let id = message_id.trim().trim_matches(['<', '>']);
        Self { names: vec!["URL"], lines: (!id.is_empty()).then(|| format!("URL:mid:{id}")).into_iter().collect() }
    }

    /// The property names `edits` replace, each once: what a change made here touched.
    pub fn touched(edits: &[Edit]) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        for name in edits.iter().flat_map(|e| e.names.iter().copied()) {
            if !out.contains(&name) {
                out.push(name);
            }
        }
        out
    }

    /// The same change, from the task as written here: its lines for `names`, to be applied again on
    /// the server's newer version of it.
    pub fn from_text(text: &str, names: &[&'static str]) -> Self {
        let mut lines = Vec::new();
        let mut open: Vec<String> = Vec::new();
        let mut index = 0;
        let master = master(text);
        for line in unfold(text) {
            let Some((head, value)) = split_unquoted(&line) else { continue };
            let name = split_params(head)[0].to_ascii_uppercase();
            match name.as_str() {
                "BEGIN" => open.push(value.trim().to_ascii_uppercase()),
                "END" => {
                    if open.pop().as_deref() == Some("VTODO") {
                        index += 1;
                    }
                }
                _ if open.last().is_some_and(|c| c == "VTODO") && index == master && names.iter().any(|n| replaces(n, &line)) => lines.push(line.clone()),
                _ => {}
            }
        }
        Self { names: names.to_vec(), lines }
    }
}

/// Whether a line is one of the property `name` an edit replaces: a `RELATED-TO` only when it names the parent.
fn replaces(name: &str, line: &str) -> bool {
    let Some((head, value)) = split_unquoted(line) else { return false };
    let parts = split_params(head);
    if !parts[0].eq_ignore_ascii_case(name) {
        return false;
    }
    name != "RELATED-TO" || property(&format!("{head}:{value}")).is_none_or(|(_, params, _)| param(&params, "RELTYPE").is_none_or(|r| r.eq_ignore_ascii_case("PARENT")))
}

/// The position, among its `VTODO`s, of the task itself (the first without a `RECURRENCE-ID`).
fn master(text: &str) -> usize {
    let (mut open, mut index, mut overrides, mut first_plain) = (Vec::<String>::new(), 0, false, None);
    for line in unfold(text) {
        let Some((name, _, value)) = property(&line) else { continue };
        match name.as_str() {
            "BEGIN" => {
                open.push(value.trim().to_ascii_uppercase());
                overrides = false;
            }
            "END" => {
                if open.pop().as_deref() == Some("VTODO") {
                    if !overrides && first_plain.is_none() {
                        first_plain = Some(index);
                    }
                    index += 1;
                }
            }
            "RECURRENCE-ID" if open.last().is_some_and(|c| c == "VTODO") => overrides = true,
            _ => {}
        }
    }
    first_plain.unwrap_or(0)
}

/// The task in `text` with `edits` made, and its stamps (`DTSTAMP`, `LAST-MODIFIED`) of `now`; the rest
/// of the object, other tasks in it included, as it was.
pub fn apply(text: &str, edits: &[Edit], now: DateTime<Utc>) -> String {
    let stamps = Edit { names: vec!["DTSTAMP", "LAST-MODIFIED"], lines: vec![format!("DTSTAMP:{}", stamp(now)), format!("LAST-MODIFIED:{}", stamp(now))] };
    let all: Vec<&Edit> = edits.iter().chain(std::iter::once(&stamps)).collect();
    let target = master(text);
    let (mut out, mut open, mut index) = (Vec::new(), Vec::<String>::new(), 0);
    for line in unfold(text) {
        let Some((head, value)) = split_unquoted(&line) else {
            if !line.is_empty() {
                out.push(line);
            }
            continue;
        };
        let name = split_params(head)[0].to_ascii_uppercase();
        let here = open.last().is_some_and(|c| c == "VTODO") && index == target;
        match name.as_str() {
            "BEGIN" => open.push(value.trim().to_ascii_uppercase()),
            "END" => {
                if open.pop().as_deref() == Some("VTODO") {
                    if index == target {
                        out.extend(all.iter().flat_map(|e| e.lines.iter().cloned()));
                    }
                    index += 1;
                }
            }
            _ if here && all.iter().any(|e| e.names.iter().any(|n| replaces(n, &line))) => continue,
            _ => {}
        }
        out.push(line);
    }
    fold(&out)
}

/// A new calendar object holding one task with this `uid` and `edits`.
pub fn new(uid: &str, edits: &[Edit], now: DateTime<Utc>) -> String {
    let shell = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//BeaverMail//Tasks//EN\r\nBEGIN:VTODO\r\nUID:{uid}\r\nCREATED:{}\r\nEND:VTODO\r\nEND:VCALENDAR\r\n", stamp(now));
    let mut edits = edits.to_vec();
    if !edits.iter().any(|e| e.names.contains(&"STATUS")) {
        edits.push(Edit::done(false, now));
    }
    apply(&shell, &edits, now)
}

/// A repeating task ticked: the occurrence done as a task of its own (a new `uid`, no repeats), and the
/// series moved on to its next due date (with its start, by as much). `None` for the series when the
/// repeats are over: then the task itself is done.
pub fn complete_repeating(text: &str, uid: &str, now: DateTime<Utc>) -> (String, Option<String>) {
    let Some(task) = parse(text) else { return (apply(text, &[Edit::done(true, now)], now), None) };
    let copy = {
        let names = ["UID", "RRULE", "RDATE", "EXDATE", "EXRULE", "RECURRENCE-ID"];
        let clean = Edit { names: names.to_vec(), lines: vec![format!("UID:{uid}")] };
        // Only the task itself: changed occurrences stay with the series.
        let target = master(text);
        let mut index = 0;
        let mut open = Vec::<String>::new();
        let mut kept = Vec::new();
        let mut skip = false;
        for line in unfold(text) {
            let upper = line.to_ascii_uppercase();
            if upper.starts_with("BEGIN:VTODO") && open.last().is_some_and(|c: &String| c == "VCALENDAR") {
                skip = index != target;
            }
            if let Some(c) = upper.strip_prefix("BEGIN:") {
                open.push(c.trim().to_owned());
            }
            let ends_todo = upper.starts_with("END:VTODO");
            if upper.starts_with("END:") {
                open.pop();
            }
            if !skip {
                kept.push(line);
            }
            if ends_todo {
                skip = false;
                index += 1;
            }
        }
        apply(&fold(&kept), &[clean, Edit::done(true, now)], now)
    };
    let base = task.start.as_ref().or(task.due.as_ref());
    let next = base.and_then(|base| {
        let first = base.utc()?;
        let event = Event { uid: task.uid.clone(), start: Some(base.clone()), rules: task.rules.clone(), rdates: task.rdates.clone(), exdates: task.exdates.clone(), ..Event::default() };
        // The occurrence after the one done, within ten years.
        let from = first + Duration::seconds(1);
        occurrences(&[event], from, from + Duration::days(3_660)).into_iter().map(|o| o.start).find(|s| *s > first)
    });
    let Some(next) = next else { return (copy, None) };
    let delta = next - base.and_then(When::utc).unwrap_or(next);
    let mut edits = Vec::new();
    if let Some(due) = &task.due {
        edits.push(Edit { names: vec!["DUE"], lines: vec![when_line("DUE", &shifted(due, delta))] });
    }
    if let Some(start) = &task.start {
        edits.push(Edit { names: vec!["DTSTART"], lines: vec![when_line("DTSTART", &shifted(start, delta))] });
    }
    edits.push(Edit::done(false, now));
    (copy, Some(apply(text, &edits, now)))
}

/// A date or time moved by `delta`, written as it was: a date, in UTC, in its zone, or floating.
fn shifted(w: &When, delta: Duration) -> When {
    let Some(at) = w.utc().map(|t| t + delta) else { return w.clone() };
    let value = if w.date_only {
        at.format("%Y%m%d").to_string()
    } else if w.value.ends_with('Z') {
        stamp(at)
    } else {
        match w.tzid.as_deref().and_then(zone) {
            Some(tz) => tz.from_utc_datetime(&at.naive_utc()).format("%Y%m%dT%H%M%S").to_string(),
            None => Local.from_utc_datetime(&at.naive_utc()).format("%Y%m%dT%H%M%S").to_string(),
        }
    };
    When { value, ..w.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap()
    }

    const PHONE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Example//Phone//EN\r\nBEGIN:VTODO\r\nUID:t1\r\nSUMMARY:Pay rent\\, flat 2\r\nDUE;VALUE=DATE:20261201\r\nPRIORITY:1\r\nCATEGORIES:home,bills\r\nRELATED-TO;RELTYPE=SIBLING:t9\r\nRELATED-TO:t0\r\nX-EXAMPLE-COLOUR:red\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:Rent\r\nTRIGGER;RELATED=END:-PT1H\r\nEND:VALARM\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";

    #[test]
    fn a_task_reads_its_fields_and_its_parent_but_not_its_siblings() {
        let t = parse(PHONE).unwrap();
        assert_eq!((t.uid.as_str(), t.summary.as_str(), t.priority, t.done), ("t1", "Pay rent, flat 2", 1, false));
        assert_eq!(t.due.as_ref().map(|d| (d.value.as_str(), d.date_only)), Some(("20261201", true)));
        assert_eq!(t.categories, ["home", "bills"]);
        assert_eq!(t.parent.as_deref(), Some("t0"));
        assert!(t.alarms);
        assert!(parse("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:e\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n").is_none(), "an event is not a task");
    }

    #[test]
    fn done_is_read_from_any_of_its_three_ways() {
        for line in ["STATUS:COMPLETED", "COMPLETED:20261101T100000Z", "PERCENT-COMPLETE:100"] {
            assert!(parse(&format!("BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:x\r\n{line}\r\nEND:VTODO\r\nEND:VCALENDAR\r\n")).unwrap().done, "{line}");
        }
    }

    #[test]
    fn an_edit_rewrites_only_what_it_touches() {
        let now = at(2026, 11, 25);
        let t = parse(PHONE).unwrap();
        let text = apply(PHONE, &[Edit::summary("Pay rent"), Edit::done(true, now), Edit::parent(None)], now);
        let back = parse(&text).unwrap();
        assert_eq!((back.summary.as_str(), back.done, back.parent.as_deref()), ("Pay rent", true, None));
        assert!(text.contains("PERCENT-COMPLETE:100") && text.contains("COMPLETED:20261125T120000Z"), "done all three ways");
        assert!(text.contains("X-EXAMPLE-COLOUR:red") && text.contains("TRIGGER;RELATED=END:-PT1H") && text.contains("RELTYPE=SIBLING:t9"), "the rest kept");
        assert_eq!(back.categories, t.categories);
        let open = parse(&apply(&text, &[Edit::done(false, now)], now)).unwrap();
        assert!(!open.done && open.completed.is_none(), "reopened, nothing says done");
    }

    #[test]
    fn a_due_that_contradicts_the_start_takes_the_start_away() {
        let now = at(2026, 11, 25);
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:x\r\nDTSTART:20261210T090000Z\r\nDURATION:PT1H\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let day = When { value: "20261205".into(), tzid: None, date_only: true };
        let t = parse(&apply(text, &[Edit::due(&parse(text).unwrap(), Some(&day))], now)).unwrap();
        assert!(t.start.is_none() && t.due == Some(day));
        assert!(!apply(text, &[Edit::due(&parse(text).unwrap(), None)], now).contains("DURATION"), "never a duration beside a due");
    }

    #[test]
    fn a_change_made_here_is_applied_again_on_the_servers_newer_version() {
        let now = at(2026, 11, 25);
        let mine = apply(PHONE, &[Edit::priority(9)], now);
        let theirs = apply(PHONE, &[Edit::summary("Pay the rent")], now);
        let merged = parse(&apply(&theirs, &[Edit::from_text(&mine, &["PRIORITY"])], now)).unwrap();
        assert_eq!((merged.summary.as_str(), merged.priority), ("Pay the rent", 9));
    }

    #[test]
    fn a_new_task_from_a_message_links_back_to_it() {
        let now = at(2026, 11, 25);
        let t = parse(&new("n1", &[Edit::summary("Answer Bob"), Edit::message("<abc@example.org>")], now)).unwrap();
        assert_eq!((t.uid.as_str(), t.summary.as_str(), t.message_id(), t.done), ("n1", "Answer Bob", Some("abc@example.org"), false));
    }

    #[test]
    fn a_repeating_task_ticked_leaves_a_done_copy_and_moves_on() {
        let now = at(2026, 11, 2);
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:r\r\nSUMMARY:Rent\r\nDUE;VALUE=DATE:20261101\r\nRRULE:FREQ=MONTHLY\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let (copy, series) = complete_repeating(text, "r-done", now);
        let copy = parse(&copy).unwrap();
        assert!(copy.done && !copy.repeats() && copy.uid == "r-done" && copy.due.as_ref().unwrap().value == "20261101");
        let series = parse(&series.unwrap()).unwrap();
        assert!(!series.done && series.repeats() && series.uid == "r");
        assert_eq!(series.due.unwrap().value, "20261201");
        let last = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:r\r\nDUE;VALUE=DATE:20261101\r\nRRULE:FREQ=MONTHLY;COUNT=1\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        assert!(complete_repeating(last, "r2", now).1.is_none(), "no occurrence left: the series is over");
    }
}
