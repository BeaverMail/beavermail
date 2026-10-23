// SPDX-License-Identifier: AGPL-3.0-only

//! Folder names as IMAP writes them (RFC 3501 5.1.3, modified UTF-7): printable ASCII as itself, `&`
//! as `&-`, anything else as UTF-16 in base64 between `&` and `-`, with `,` in place of `/`.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,";

/// `name` as the server expects it, to create a folder with it.
pub fn encode(name: &str) -> String {
    let mut out = String::new();
    let mut wide: Vec<u16> = Vec::new();
    for c in name.chars().chain(std::iter::once('\0')) {
        let plain = (' '..='~').contains(&c) || c == '\0';
        if plain && !wide.is_empty() {
            let bytes: Vec<u8> = wide.drain(..).flat_map(u16::to_be_bytes).collect();
            out.push('&');
            for chunk in bytes.chunks(3) {
                let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
                for i in 0..=chunk.len() {
                    out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
                }
            }
            out.push('-');
        }
        match c {
            '\0' => {}
            '&' => out.push_str("&-"),
            _ if plain => out.push(c),
            _ => wide.extend(c.encode_utf16(&mut [0; 2]).iter()),
        }
    }
    out
}

/// `name` from the server as people read it. A part that is not valid modified UTF-7 is left as it came.
pub fn decode(name: &str) -> String {
    let mut out = String::new();
    let mut rest = name;
    while let Some(at) = rest.find('&') {
        let Some(end) = rest[at..].find('-').map(|e| at + e) else { break };
        out.push_str(&rest[..at]);
        let chunk = &rest[at + 1..end];
        match wide(chunk) {
            _ if chunk.is_empty() => out.push('&'),
            Some(text) => out.push_str(&text),
            None => out.push_str(&rest[at..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// The text of the base64 between `&` and `-`.
fn wide(chunk: &str) -> Option<String> {
    let (mut bits, mut count, mut bytes) = (0u32, 0, Vec::new());
    for c in chunk.bytes() {
        bits = bits << 6 | ALPHABET.iter().position(|a| *a == c)? as u32;
        count += 6;
        if count >= 8 {
            count -= 8;
            bytes.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    if bytes.len() % 2 != 0 {
        return None;
    }
    String::from_utf16(&bytes.chunks(2).map(|b| u16::from_be_bytes([b[0], b[1]])).collect::<Vec<_>>()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_go_to_the_server_and_back() {
        // The examples of RFC 3501, then the ones a user here would type.
        assert_eq!(encode("~peter/mail/台北/日本語"), "~peter/mail/&U,BTFw-/&ZeVnLIqe-");
        assert_eq!(decode("~peter/mail/&U,BTFw-/&ZeVnLIqe-"), "~peter/mail/台北/日本語");
        for name in ["Città", "Fatture & ricevute", "Clienti/Società 2026", "Inbox", "Ünïcödé €", "😀 Emoji", ""] {
            assert_eq!(decode(&encode(name)), name);
        }
        assert_eq!(encode("Fatture & ricevute"), "Fatture &- ricevute");
        assert_eq!(encode("Città"), "Citt&AOA-");
        assert_eq!(decode("Not &valid- either &"), "Not &valid- either &", "left as it came");
    }
}
