// SPDX-License-Identifier: AGPL-3.0-only

//! The CardDAV client against a small local server that keeps an address book in memory, the way a
//! real one answers: a well-known address that redirects, a principal, a home, versions per card.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use beavermail_carddav::{Book, Found, Written, vcard};
use beavermail_core::MailError;

const BOOK: &str = "/dav/books/ada/default/";
/// One the server fills by itself, read-only.
const RECENT: &str = "/dav/books/ada/z-app-generated--contactsinteraction--recent/";

#[derive(Default)]
struct Cards {
    /// href → (version, vCard)
    cards: BTreeMap<String, (u32, String)>,
    version: u32,
}

fn start() -> (String, Arc<Mutex<Cards>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let cards = Arc::new(Mutex::new(Cards::default()));
    let served = cards.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (method, path) = (parts.next().unwrap_or_default().to_owned(), parts.next().unwrap_or_default().to_owned());
            let mut headers = BTreeMap::new();
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':') {
                    headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
                }
            }
            let length: usize = headers.get("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let reply = answer(&served, &method, &path, &headers, &String::from_utf8_lossy(&body));
            let mut stream = stream;
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (url, cards)
}

fn answer(cards: &Arc<Mutex<Cards>>, method: &str, path: &str, headers: &BTreeMap<String, String>, body: &str) -> String {
    let reply = |status: &str, extra: &str, text: &str| format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{text}", text.len());
    let multistatus = |inner: String| reply("207 Multi-Status", "Content-Type: application/xml\r\n", &format!(r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">{inner}</d:multistatus>"#));
    let ok = |href: &str, props: &str| format!("<d:response><d:href>{href}</d:href><d:propstat><d:prop>{props}</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>");
    // "ada:secret"
    if headers.get("authorization").map(String::as_str) != Some("Basic YWRhOnNlY3JldA==") {
        return reply("401 Unauthorized", "WWW-Authenticate: Basic\r\n", "");
    }
    let mut state = cards.lock().unwrap();
    match (method, path) {
        ("PROPFIND", "/.well-known/carddav") => reply("301 Moved Permanently", "Location: /dav/principals/ada/\r\n", ""),
        ("PROPFIND", "/dav/principals/ada/") => multistatus(ok(path, "<d:current-user-principal><d:href>/dav/principals/ada/</d:href></d:current-user-principal><c:addressbook-home-set><d:href>/dav/books/ada/</d:href></c:addressbook-home-set><d:resourcetype/>")),
        ("PROPFIND", "/dav/books/ada/") => multistatus(ok(path, "<d:resourcetype><d:collection/></d:resourcetype>") + &ok(BOOK, "<d:resourcetype><d:collection/><c:addressbook/></d:resourcetype><d:displayname>Contacts</d:displayname><d:current-user-privilege-set><d:privilege><d:read/></d:privilege><d:privilege><d:write/></d:privilege></d:current-user-privilege-set>") + &ok(RECENT, "<d:resourcetype><d:collection/><c:addressbook/></d:resourcetype><d:displayname>Recently contacted</d:displayname><d:current-user-privilege-set><d:privilege><d:read/></d:privilege></d:current-user-privilege-set>")),
        ("PROPFIND", BOOK) => {
            let mut inner = ok(BOOK, "<d:resourcetype><d:collection/><c:addressbook/></d:resourcetype>");
            for (href, (version, _)) in &state.cards {
                inner += &ok(href, &format!("<d:getetag>\"{version}\"</d:getetag><d:resourcetype/>"));
            }
            multistatus(inner)
        }
        ("REPORT", BOOK) => {
            let wanted: Vec<&str> = body.split("<d:href>").skip(1).filter_map(|h| h.split("</d:href>").next()).collect();
            let inner = wanted.iter().filter_map(|h| state.cards.get(*h).map(|(v, card)| ok(h, &format!("<d:getetag>\"{v}\"</d:getetag><c:address-data>{card}</c:address-data>")))).collect();
            multistatus(inner)
        }
        ("PUT", href) => {
            let current = state.cards.get(href).map(|(v, _)| format!("\"{v}\""));
            let allowed = match (headers.get("if-match"), headers.get("if-none-match"), &current) {
                (Some(want), _, Some(have)) => want == have,
                (_, Some(star), None) => star == "*",
                (None, None, _) => true,
                _ => false,
            };
            if !allowed {
                return reply("412 Precondition Failed", "", "");
            }
            state.version += 1;
            let version = state.version;
            state.cards.insert(href.to_owned(), (version, body.to_owned()));
            reply("201 Created", &format!("ETag: \"{version}\"\r\n"), "")
        }
        ("DELETE", href) => match state.cards.get(href).map(|(v, _)| format!("\"{v}\"")) {
            None => reply("404 Not Found", "", ""),
            Some(have) if headers.get("if-match") != Some(&have) => reply("412 Precondition Failed", "", ""),
            Some(_) => {
                state.cards.remove(href);
                reply("204 No Content", "", "")
            }
        },
        _ => reply("404 Not Found", "", ""),
    }
}

/// The card with a new name, "Given Family".
fn rename(card: &str, name: &str) -> String {
    let mut details = vcard::details(card);
    let mut words: Vec<&str> = name.split_whitespace().collect();
    details.family = words.pop().unwrap_or_default().to_owned();
    details.given = words.join(" ");
    vcard::apply(card, &details)
}

#[test]
fn the_address_book_is_found_from_the_server_address_through_the_well_known_one() {
    let (url, _) = start();
    let book = Book::discover(&url, "ada", "secret").unwrap();
    assert_eq!(book.url, format!("{url}{BOOK}"));
    let found = Book::discover_all(&url, "ada", "secret").unwrap();
    let book = |path: &str, name: &str, writable| Found { url: format!("{url}{path}"), name: name.to_owned(), writable };
    assert_eq!(found, [book(BOOK, "Contacts", true), book(RECENT, "Recently contacted", false)], "with the names the server gives, and what can be written");
    assert!(matches!(Book::discover(&url, "ada", "wrong"), Err(MailError::Auth(_))));
}

#[test]
fn cards_are_listed_read_written_and_deleted_only_at_the_version_read() {
    let (url, _) = start();
    let book = Book::discover(&url, "ada", "secret").unwrap();
    assert!(book.list().unwrap().is_empty());
    let href = book.new_href("uid-1");
    let card = vcard::new_card("uid-1", "Bob Brown", &["bob@y.org".into()], "");
    let Written::Done(Some(etag)) = book.put(&href, &card, None).unwrap() else { panic!("created with a version") };
    assert_eq!(book.put(&href, &card, None).unwrap(), Written::Conflict, "never two cards at one place");
    let listed = book.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].href.as_str(), listed[0].etag.as_str()), (href.as_str(), etag.as_str()));
    let fetched = book.fetch(std::slice::from_ref(&href)).unwrap();
    assert_eq!(vcard::parse(&fetched[0].1).name, "Bob Brown");
    // Changed elsewhere since it was read: neither written over nor deleted.
    let changed = rename(&card, "Robert Brown");
    let Written::Done(Some(newer)) = book.put(&href, &changed, Some(&etag)).unwrap() else { panic!() };
    assert_eq!(book.put(&href, &card, Some(&etag)).unwrap(), Written::Conflict);
    assert_eq!(book.delete(&href, &etag).unwrap(), Written::Conflict);
    assert_eq!(book.delete(&href, &newer).unwrap(), Written::Done(None));
    assert_eq!(book.delete(&href, &newer).unwrap(), Written::Done(None), "already gone counts as deleted");
    assert!(book.list().unwrap().is_empty());
}

#[test]
fn a_sync_brings_every_card_sends_what_changed_here_and_lets_the_server_win() {
    let (url, served) = start();
    {
        let mut state = served.lock().unwrap();
        state.version = 2;
        state.cards.insert(format!("{BOOK}ada.vcf"), (1, vcard::new_card("ada", "Ada Lovelace", &["ada@x.org".into()], "")));
        // A phone-only contact, as phones keep many: it comes too.
        state.cards.insert(format!("{BOOK}mario.vcf"), (2, "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:mario\r\nFN:Mario Rossi\r\nTEL:+39 02 1234\r\nEND:VCARD\r\n".into()));
    }
    let book = Book::discover(&url, "ada", "secret").unwrap();
    let store = beavermail_store::Store::in_memory().unwrap();
    let first = beavermail_carddav::sync(&book, &store).unwrap();
    assert_eq!(first.received, 2);
    let names: Vec<String> = store.address_book().unwrap().into_iter().map(|c| c.name).collect();
    assert_eq!(names, ["Ada Lovelace", "Mario Rossi"]);
    assert!(beavermail_carddav::sync(&book, &store).unwrap() == beavermail_carddav::Synced::default(), "nothing new: nothing moves");

    // Made here, changed here, deleted here: all sent.
    let bob = book.new_href("bob");
    let text = vcard::new_card("bob", "Bob Brown", &["bob@y.org".into()], "");
    store.add_card(&book.url, &bob, &text, &beavermail_carddav::fields(&text)).unwrap();
    let ada = store.address_book().unwrap().into_iter().find(|c| c.name == "Ada Lovelace").unwrap();
    let renamed = rename(&ada.vcard, "Augusta Ada King");
    store.edit_card(ada.id, &renamed, &beavermail_carddav::fields(&renamed)).unwrap();
    let mario = store.address_book().unwrap().into_iter().find(|c| c.name == "Mario Rossi").unwrap();
    store.delete_card(mario.id).unwrap();
    let sent = beavermail_carddav::sync(&book, &store).unwrap();
    assert_eq!(sent.sent, 3, "{sent:?}");
    let on_server: Vec<String> = served.lock().unwrap().cards.values().map(|(_, c)| vcard::parse(c).name).collect();
    assert_eq!(on_server, ["Augusta Ada King", "Bob Brown"]);

    // Changed on both sides: the server's version stays.
    let ada = store.address_book().unwrap().into_iter().find(|c| c.name == "Augusta Ada King").unwrap();
    let mine = rename(&ada.vcard, "Mine");
    store.edit_card(ada.id, &mine, &beavermail_carddav::fields(&mine)).unwrap();
    {
        let mut state = served.lock().unwrap();
        state.version += 1;
        let version = state.version;
        let theirs = rename(&state.cards[&ada.href].1, "Changed elsewhere");
        state.cards.insert(ada.href.clone(), (version, theirs));
    }
    let crossed = beavermail_carddav::sync(&book, &store).unwrap();
    assert_eq!(crossed.lost, 1);
    assert!(store.address_book().unwrap().iter().any(|c| c.name == "Changed elsewhere"));

    // Deleted elsewhere: gone here.
    served.lock().unwrap().cards.remove(&bob);
    beavermail_carddav::sync(&book, &store).unwrap();
    assert!(!store.address_book().unwrap().iter().any(|c| c.name == "Bob Brown"));
}
