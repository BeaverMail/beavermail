// SPDX-License-Identifier: AGPL-3.0-only

//! Fetching the images and styles of a message, only after the user agreed to it.
//!
//! Loading reveals the reader's address and that the message was opened, so the rules are strict:
//! GET only, `http(s)` only, no cookies, no referrer, no servers on the local network, a size and
//! time limit for each file and a limit on how many files one message may pull in.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request, Url};
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};
use ureq::Error;

const WORKERS: usize = 4;
const MAX_FILES: usize = 100;
const MAX_BYTES: u64 = 5 * 1024 * 1024;
const MAX_REDIRECTS: usize = 3;

/// The one local port a test may fetch from.
#[cfg(test)]
pub static TEST_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

type Job = (Url, Box<dyn NetHandler>);

/// Downloads for one open message. Dropping it stops the workers.
pub struct Net {
    jobs: mpsc::Sender<Job>,
    requested: AtomicUsize,
    /// Files finished (loaded or failed); the page redraws when this changes.
    pub done: Arc<AtomicUsize>,
}

impl Net {
    /// `wake` is called whenever a file has arrived.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        let (jobs, queue) = mpsc::channel::<Job>();
        let queue = Arc::new(Mutex::new(queue));
        let done = Arc::new(AtomicUsize::new(0));
        let wake = Arc::new(wake);
        for _ in 0..WORKERS {
            let (queue, done, wake) = (queue.clone(), done.clone(), wake.clone());
            std::thread::spawn(move || {
                let agent = agent();
                loop {
                    let job = queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner).recv();
                    let Ok((url, handler)) = job else { return };
                    if let Some(bytes) = get(&agent, url.clone()) {
                        handler.bytes(url.to_string(), Bytes::from(bytes));
                    }
                    done.fetch_add(1, Ordering::Relaxed);
                    wake();
                }
            });
        }
        Arc::new(Self { jobs, requested: AtomicUsize::new(0), done })
    }
}

impl NetProvider for Net {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        if request.method == blitz_traits::net::Method::GET && self.requested.fetch_add(1, Ordering::Relaxed) < MAX_FILES {
            let _ = self.jobs.send((request.url, handler));
        }
    }
}

fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .max_redirects(0) // followed by hand below, so every hop is checked
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(10)))
        .user_agent("Mozilla/5.0")
        .build();
    ureq::Agent::with_parts(config, DefaultConnector::default(), PublicOnly(DefaultResolver::default()))
}

/// Resolves names like the default resolver but hands the client only public addresses. The check and
/// the connection use the same answer, so a server cannot say "public" first and "local" afterwards.
#[derive(Debug)]
struct PublicOnly(DefaultResolver);

impl Resolver for PublicOnly {
    fn resolve(&self, uri: &Uri, config: &Config, timeout: NextTimeout) -> Result<ResolvedSocketAddrs, Error> {
        let mut out = self.empty();
        for addr in self.0.resolve(uri, config, timeout)?.iter().filter(|a| public(**a)) {
            out.push(*addr);
        }
        if out.is_empty() { Err(Error::HostNotFound) } else { Ok(out) }
    }
}

fn get(agent: &ureq::Agent, mut url: Url) -> Option<Vec<u8>> {
    for _ in 0..=MAX_REDIRECTS {
        if !allowed(&url) {
            return None;
        }
        let mut response = agent.get(url.as_str()).call().ok()?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            let location = response.headers().get("location")?.to_str().ok()?;
            url = url.join(location).ok()?;
            continue;
        }
        return if status == 200 { response.body_mut().with_config().limit(MAX_BYTES).read_to_vec().ok() } else { None };
    }
    None
}

/// Asks a mailing list to stop sending, the RFC 8058 way: one POST to the sender's `https` link, with
/// the same limits as loading images. A redirect is not followed: the RFC forbids the sender one.
pub fn one_click(link: &str) -> Result<(), String> {
    let url = Url::parse(link).ok().filter(|u| u.scheme() == "https" && allowed(u)).ok_or("the unsubscribe link is not an https address")?;
    let response = agent().post(url.as_str()).content_type("application/x-www-form-urlencoded").send("List-Unsubscribe=One-Click").map_err(|e| e.to_string())?;
    match response.status().as_u16() {
        200..300 => Ok(()),
        status => Err(format!("the sender answered {status}")),
    }
}

/// Only the web: names and addresses are checked when the client resolves them (see [`PublicOnly`]).
fn allowed(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
}

fn public(addr: SocketAddr) -> bool {
    #[cfg(test)]
    if addr.port() == TEST_PORT.load(Ordering::Relaxed) {
        return true;
    }
    public_ip(addr.ip())
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => !(a.is_private() || a.is_loopback() || a.is_link_local() || a.is_unspecified() || a.is_broadcast() || a.is_documentation() || a.octets()[0] == 100 && (64..128).contains(&a.octets()[1])),
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(v4) => public_ip(IpAddr::V4(v4)),
            None => !(a.is_loopback() || a.is_unspecified() || a.is_unique_local() || a.is_unicast_link_local()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_web_addresses_are_allowed() {
        for bad in ["127.0.0.1", "192.168.1.1", "10.0.0.5", "169.254.169.254", "::1", "::ffff:10.0.0.1", "100.64.0.1", "fd00::1", "fe80::1"] {
            assert!(!public_ip(bad.parse().unwrap()), "{bad}");
        }
        assert!(public_ip("93.184.216.34".parse().unwrap()));
        for bad in ["ftp://example.com/x", "file:///etc/passwd", "data:text/plain,x"] {
            assert!(!allowed(&Url::parse(bad).unwrap()), "{bad}");
        }
    }

    #[test]
    fn one_click_needs_https_and_a_public_server() {
        assert!(one_click("http://example.com/u").is_err());
        assert!(one_click("mailto:a@example.com").is_err());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(one_click(&format!("https://localhost:{}/u", listener.local_addr().unwrap().port())).is_err());
        assert!(listener.accept().is_err(), "nothing may connect to a local address");
    }

    #[test]
    fn a_local_server_is_fetched_from_never() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = Url::parse(&format!("http://{}/pixel.gif", listener.local_addr().unwrap())).unwrap();
        assert!(get(&agent(), url).is_none());
        assert!(listener.accept().is_err(), "nothing may connect to a local address");
        // A name that resolves to this computer is refused too: the answer is checked where it is used.
        let named = Url::parse(&format!("http://localhost:{}/pixel.gif", listener.local_addr().unwrap().port())).unwrap();
        assert!(get(&agent(), named).is_none());
        assert!(listener.accept().is_err(), "a name for this computer is no way in");
    }
}
