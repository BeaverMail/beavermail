// SPDX-License-Identifier: AGPL-3.0-only

//! Signing in with Google or Microsoft (OAuth 2.0: authorization code with PKCE, redirected to this
//! computer) and keeping an account's access token fresh. The refresh token is the account's secret
//! in the credential store, where a password would be; access tokens stay in memory.
//!
//! The app is registered with each provider; its client id (and Google's client secret, which a
//! desktop app cannot keep secret and is not meant to) is read when the app is built, from
//! `BEAVERMAIL_GOOGLE_CLIENT_ID`, `BEAVERMAIL_GOOGLE_CLIENT_SECRET` and
//! `BEAVERMAIL_MICROSOFT_CLIENT_ID`. A build without one offers no sign-in with that provider.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use beavermail_core::{Account, MailError, OAuthProvider, TokenSource};
use sha2::Digest;

/// How long the browser has to come back with the sign-in.
const SIGN_IN_WAIT: Duration = Duration::from_secs(300);

struct Endpoints {
    auth: &'static str,
    token: &'static str,
    scopes: &'static str,
}

fn endpoints(provider: OAuthProvider) -> Endpoints {
    match provider {
        OAuthProvider::Google => Endpoints {
            auth: "https://accounts.google.com/o/oauth2/v2/auth",
            token: "https://oauth2.googleapis.com/token",
            scopes: "https://mail.google.com/ openid email profile",
        },
        // Every scope is for outlook.office.com: Microsoft gives a token for one resource only.
        OAuthProvider::Microsoft => Endpoints {
            auth: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
            token: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
            scopes: "https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send offline_access openid email profile",
        },
    }
}

/// The app's registration with `provider`: client id and, for Google, client secret.
fn client(provider: OAuthProvider) -> Option<(&'static str, Option<&'static str>)> {
    let (id, secret) = match provider {
        OAuthProvider::Google => (option_env!("BEAVERMAIL_GOOGLE_CLIENT_ID"), option_env!("BEAVERMAIL_GOOGLE_CLIENT_SECRET")),
        OAuthProvider::Microsoft => (option_env!("BEAVERMAIL_MICROSOFT_CLIENT_ID"), None),
    };
    id.map(str::trim).filter(|id| !id.is_empty()).map(|id| (id, secret.map(str::trim).filter(|s| !s.is_empty())))
}

/// Whether this build can sign in with `provider`.
pub fn available(provider: OAuthProvider) -> bool {
    client(provider).is_some()
}

/// The incoming and outgoing servers of `provider`'s mailboxes.
pub fn servers(provider: OAuthProvider) -> (beavermail_core::Server, beavermail_core::Server) {
    use beavermail_core::{Server, Tls};
    let (imap, smtp) = match provider {
        OAuthProvider::Google => ("imap.gmail.com", "smtp.gmail.com"),
        OAuthProvider::Microsoft => ("outlook.office365.com", "smtp.office365.com"),
    };
    (Server { host: imap.into(), port: 993, tls: Tls::Tls }, Server { host: smtp.into(), port: 587, tls: Tls::StartTls })
}

/// What a sign-in gives: who signed in, and the token that keeps the account signed in.
#[derive(Debug)]
pub struct Grant {
    pub email: String,
    pub name: Option<String>,
    pub refresh_token: String,
}

/// Signs in with `provider`: opens its page in the browser through `open`, waits for the browser to
/// come back to this computer, and trades the answer for tokens. `cancel` stops the wait.
pub fn authorize(provider: OAuthProvider, open: impl FnOnce(&str) -> Result<(), String>, cancel: &AtomicBool) -> Result<Grant, String> {
    let (client_id, secret) = client(provider).ok_or("this build cannot sign in with this provider")?;
    let verifier = random_text(64);
    let challenge = URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    let state = random_text(32);
    // `localhost` may be either loopback address, so both listen on one port.
    let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| e.to_string())?;
    let port = v4.local_addr().map_err(|e| e.to_string())?.port();
    let v6 = TcpListener::bind((Ipv6Addr::LOCALHOST, port)).ok();
    // Microsoft matches the registered `http://localhost`; Google takes the address itself.
    let redirect = match provider {
        OAuthProvider::Microsoft => format!("http://localhost:{port}/"),
        OAuthProvider::Google => format!("http://127.0.0.1:{port}/"),
    };
    let ends = endpoints(provider);
    let mut query = vec![
        ("client_id", client_id),
        ("redirect_uri", &redirect),
        ("response_type", "code"),
        ("scope", ends.scopes),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
    ];
    query.extend(match provider {
        // A refresh token only with offline access, and again on a new consent.
        OAuthProvider::Google => [("access_type", "offline"), ("prompt", "consent")],
        OAuthProvider::Microsoft => [("prompt", "select_account"), ("response_mode", "query")],
    });
    open(&format!("{}?{}", ends.auth, form(&query)))?;
    let answer = wait_for_redirect(&v4, v6.as_ref(), cancel)?;
    if let Some(error) = answer.iter().find(|(k, _)| k == "error").map(|(_, v)| v) {
        return Err(if error == "access_denied" { "Sign-in was cancelled".into() } else { format!("{} refused the sign-in: {error}", provider.name()) });
    }
    let field = |name: &str| answer.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    if field("state") != Some(&state) {
        return Err("the sign-in answer did not belong to this request".into());
    }
    let code = field("code").ok_or("the sign-in answer had no code")?;
    let mut exchange = vec![("grant_type", "authorization_code"), ("code", code), ("redirect_uri", &redirect), ("client_id", client_id), ("code_verifier", &verifier)];
    if let Some(secret) = secret {
        exchange.push(("client_secret", secret));
    }
    let tokens = post(ends.token, &exchange).map_err(|e| e.to_string())?;
    let refresh_token = tokens.get("refresh_token").and_then(|v| v.as_str()).ok_or("the provider gave no refresh token")?.to_owned();
    let id_token = tokens.get("id_token").and_then(|v| v.as_str()).ok_or("the provider did not say who signed in")?;
    let (email, name) = identity(provider, id_token, client_id)?;
    Ok(Grant { email, name, refresh_token })
}

/// Who signed in, from the id token. It came straight from the provider's token endpoint over a
/// verified TLS connection, so its signature need not be checked (OpenID Connect Core 3.1.3.7); its
/// issuer and audience are.
fn identity(provider: OAuthProvider, id_token: &str, client_id: &str) -> Result<(String, Option<String>), String> {
    let payload = id_token.split('.').nth(1).ok_or("the id token is malformed")?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).map_err(|_| "the id token is malformed")?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| "the id token is malformed")?;
    let text = |name: &str| claims.get(name).and_then(|v| v.as_str()).map(str::to_owned);
    let issuer = text("iss").unwrap_or_default();
    let issued_here = match provider {
        OAuthProvider::Google => issuer == "https://accounts.google.com" || issuer == "accounts.google.com",
        OAuthProvider::Microsoft => issuer.starts_with("https://login.microsoftonline.com/"),
    };
    let audience = claims.get("aud").is_some_and(|aud| aud.as_str() == Some(client_id) || aud.as_array().is_some_and(|all| all.iter().any(|a| a.as_str() == Some(client_id))));
    if !issued_here || !audience {
        return Err("the id token was not issued for this app".into());
    }
    let email = text("email").or_else(|| text("preferred_username")).filter(|e| e.contains('@')).ok_or("the provider did not give the account's address")?;
    Ok((email.to_lowercase(), text("name").filter(|n| !n.trim().is_empty())))
}

/// Waits for the browser's request to the redirect address and answers it with a page saying the
/// sign-in can go on in the app. Returns the request's query.
fn wait_for_redirect(v4: &TcpListener, v6: Option<&TcpListener>, cancel: &AtomicBool) -> Result<Vec<(String, String)>, String> {
    for listener in std::iter::once(v4).chain(v6) {
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    }
    let deadline = Instant::now() + SIGN_IN_WAIT;
    while Instant::now() < deadline {
        if cancel.load(Ordering::Relaxed) {
            return Err("Sign-in was cancelled".into());
        }
        for listener in std::iter::once(v4).chain(v6) {
            if let Ok((stream, _)) = listener.accept()
                && let Some(query) = answer(stream)
            {
                return Ok(query);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("The sign-in took too long; try again".into())
}

/// Reads one browser request; its query if it is the sign-in's answer (not, say, a favicon request).
fn answer(mut stream: TcpStream) -> Option<Vec<(String, String)>> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buffer = [0u8; 8192];
    let n = stream.read(&mut buffer).ok()?;
    let request = String::from_utf8_lossy(&buffer[..n]);
    let target = request.lines().next()?.split_whitespace().nth(1)?;
    let query = target.split_once('?').map(|(_, q)| q)?;
    let pairs: Vec<(String, String)> = query.split('&').filter_map(|pair| pair.split_once('=')).map(|(k, v)| (decode(k), decode(v))).collect();
    if !pairs.iter().any(|(k, _)| k == "code" || k == "error") {
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return None;
    }
    let page = "<!doctype html><meta charset=utf-8><title>BeaverMail</title><body style=\"font-family:sans-serif;margin:4em\"><h2>You can close this window</h2><p>Go back to BeaverMail to finish.</p>";
    let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len()).as_bytes());
    Some(pairs)
}

/// The access token source for `account`, signed in with `provider`: a token kept in memory until a
/// minute before it expires, then a new one asked for with the refresh token from the credential
/// store. A refresh token the provider replaces is stored in its place.
pub fn token_source(provider: OAuthProvider, account: Account) -> TokenSource {
    let cache: Mutex<Option<(String, Instant)>> = Mutex::new(None);
    Arc::new(move || {
        let mut cached = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((token, until)) = cached.as_ref()
            && Instant::now() < *until
        {
            return Ok(token.clone());
        }
        let (client_id, secret) = client(provider).ok_or_else(|| MailError::Auth(format!("this build cannot sign in with {}", provider.name())))?;
        let refresh = crate::password(&account).map_err(MailError::Auth)?;
        let mut form = vec![("grant_type", "refresh_token"), ("refresh_token", refresh.as_str()), ("client_id", client_id)];
        if let Some(secret) = secret {
            form.push(("client_secret", secret));
        }
        let tokens = post(endpoints(provider).token, &form)?;
        let token = tokens.get("access_token").and_then(|v| v.as_str()).ok_or_else(|| MailError::Backend("the provider gave no access token".into()))?.to_owned();
        if let Some(new) = tokens.get("refresh_token").and_then(|v| v.as_str()).filter(|new| *new != refresh) {
            crate::set_password(&account, new).map_err(MailError::Backend)?;
        }
        let lasts = tokens.get("expires_in").and_then(serde_json::Value::as_u64).unwrap_or(3600);
        *cached = Some((token.clone(), Instant::now() + Duration::from_secs(lasts.saturating_sub(60))));
        Ok(token)
    })
}

/// A form POST to a token endpoint, its JSON answer. A refused grant asks the user to sign in again;
/// no answer at all is [`MailError::Offline`].
fn post(url: &str, fields: &[(&str, &str)]) -> Result<serde_json::Value, MailError> {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(30))).build().into();
    let mut response = agent.post(url).content_type("application/x-www-form-urlencoded").send(form(fields)).map_err(|e| MailError::Offline(e.to_string()))?;
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().map_err(|e| MailError::Offline(e.to_string()))?;
    if (200..300).contains(&status) {
        return serde_json::from_str(&body).map_err(|e| MailError::Backend(format!("unreadable answer from the provider: {e}")));
    }
    Err(if needs_sign_in(&body) {
        MailError::Auth("the provider no longer accepts this sign-in: sign in again".into())
    } else {
        MailError::Backend(format!("the provider refused ({status}): {}", body.chars().take(300).collect::<String>()))
    })
}

/// An answer that only a new sign-in can fix: access revoked, consent withdrawn, password changed.
fn needs_sign_in(body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    ["invalid_grant", "interaction_required", "consent_required", "aadsts65001"].iter().any(|e| body.contains(e))
}

fn random_text(len: usize) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).expect("the system's random source");
    bytes.iter().map(|b| CHARS[usize::from(*b) % CHARS.len()] as char).collect()
}

/// `a=1&b=2`, each value percent-encoded.
fn form(fields: &[(&str, &str)]) -> String {
    let encode = |text: &str| -> String {
        text.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    fields.iter().map(|(k, v)| format!("{}={}", encode(k), encode(v))).collect::<Vec<_>>().join("&")
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(bytes.len()), 0);
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => match bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                Some(byte) => {
                    out.push(byte);
                    i += 3;
                }
                None => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_token(claims: serde_json::Value) -> String {
        format!("{}.{}.sig", URL_SAFE_NO_PAD.encode(b"{\"alg\":\"RS256\"}"), URL_SAFE_NO_PAD.encode(claims.to_string()))
    }

    #[test]
    fn the_id_token_says_who_signed_in_only_when_it_is_for_this_app() {
        let google = id_token(serde_json::json!({"iss": "https://accounts.google.com", "aud": "app", "email": "Ada@Gmail.com", "name": "Ada"}));
        assert_eq!(identity(OAuthProvider::Google, &google, "app"), Ok(("ada@gmail.com".into(), Some("Ada".into()))));
        assert!(identity(OAuthProvider::Google, &google, "another app").is_err(), "the audience must be this app");
        let elsewhere = id_token(serde_json::json!({"iss": "https://evil.example", "aud": "app", "email": "ada@gmail.com"}));
        assert!(identity(OAuthProvider::Google, &elsewhere, "app").is_err(), "the issuer must be the provider");
        let microsoft = id_token(serde_json::json!({"iss": "https://login.microsoftonline.com/tenant/v2.0", "aud": "app", "preferred_username": "grace@contoso.com"}));
        assert_eq!(identity(OAuthProvider::Microsoft, &microsoft, "app"), Ok(("grace@contoso.com".into(), None)), "the username when there is no email claim");
    }

    #[test]
    fn the_browser_answer_is_read_and_a_page_says_to_go_back() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = std::thread::spawn(move || {
            let mut favicon = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            favicon.write_all(b"GET /favicon.ico HTTP/1.1\r\n\r\n").unwrap();
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            stream.write_all(b"GET /?state=s%2B1&code=4%2F0Ab HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
            let mut page = String::new();
            stream.read_to_string(&mut page).unwrap();
            page
        });
        let query = wait_for_redirect(&listener, None, &AtomicBool::new(false)).unwrap();
        assert_eq!(query, [("state".to_owned(), "s+1".to_owned()), ("code".to_owned(), "4/0Ab".to_owned())]);
        assert!(browser.join().unwrap().contains("You can close this window"));
        let cancelled = AtomicBool::new(true);
        assert!(wait_for_redirect(&listener, None, &cancelled).is_err(), "Cancel stops the wait");
    }

    #[test]
    fn only_a_refused_grant_asks_to_sign_in_again() {
        assert!(needs_sign_in(r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#));
        assert!(needs_sign_in(r#"{"error":"invalid_grant","error_description":"AADSTS700082"}"#));
        assert!(!needs_sign_in(r#"{"error":"temporarily_unavailable"}"#));
    }

    #[test]
    fn form_values_are_encoded() {
        assert_eq!(form(&[("scope", "openid email"), ("redirect_uri", "http://localhost:1/")]), "scope=openid%20email&redirect_uri=http%3A%2F%2Flocalhost%3A1%2F");
        assert_eq!(decode("a%2Fb+c"), "a/b c");
        assert_eq!(random_text(64).len(), 64);
    }
}
