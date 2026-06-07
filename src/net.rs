//! Tiny non-blocking weather + time fetcher.
//!
//! wttr.in serves a plain-text, curl-friendly response over **HTTP** (no TLS needed on the
//! MCU), and a custom `format=` string lets us grab the local time and condition in one tiny
//! request:
//!
//! ```text
//! GET /Bengaluru?format=~%T~%C~  ->  "~20:21:01+0530~Partly cloudy~"
//! ```
//!
//! The `~` markers make the body trivial and robust to parse (even if a proxy chunks it), and we
//! get both the [`WeatherState`] and the wall-clock time from a single round trip — no separate
//! SNTP client required. Everything uses fixed-size stack buffers; there are no heap allocations.

use embassy_net::{IpEndpoint, Stack, dns::DnsQueryType, tcp::TcpSocket};
use embassy_time::{Duration, Instant};
use embedded_io_async::Write;

use crate::weather::WeatherState;
use crate::{TimeSync, Update};

const HOST: &str = "wttr.in";
const PORT: u16 = 80;

/// Long-lived scratch buffers for the network task, allocated once in `static` memory so the task
/// future itself stays tiny (keeps task stacks small and the framebuffer firmware within budget).
pub struct NetScratch {
    pub rx: &'static mut [u8],
    pub tx: &'static mut [u8],
    pub resp: &'static mut [u8],
}

/// Resolve, connect, GET, and parse. Returns `None` on any network/parse error so the caller can
/// simply retry later without the renderer ever blocking.
pub async fn fetch(
    stack: Stack<'_>,
    city: &str,
    rx: &mut [u8],
    tx: &mut [u8],
    resp: &mut [u8],
) -> Option<Update> {
    let addrs = stack.dns_query(HOST, DnsQueryType::A).await.ok()?;
    let ip = *addrs.first()?;

    let mut socket = TcpSocket::new(stack, rx, tx);
    socket.set_timeout(Some(Duration::from_secs(10)));
    socket.connect(IpEndpoint::new(ip, PORT)).await.ok()?;

    let mut request = [0u8; 192];
    let len = build_request(&mut request, city)?;
    socket.write_all(&request[..len]).await.ok()?;

    let mut filled = 0;
    while filled < resp.len() {
        match socket.read(&mut resp[filled..]).await {
            Ok(0) | Err(_) => break, // peer closed (Connection: close) or errored
            Ok(n) => filled += n,
        }
    }
    socket.close();

    parse(&resp[..filled])
}

/// Write the HTTP request into `buf`, returning its length. A `curl` User-Agent is required so
/// wttr.in returns the terminal (plain-text) format rather than HTML.
fn build_request(buf: &mut [u8], city: &str) -> Option<usize> {
    let mut w = Cursor::new(buf);
    w.put(b"GET /")?;
    w.put(city.as_bytes())?;
    w.put(b"?format=~%T~%C~ HTTP/1.1\r\n")?;
    w.put(b"Host: wttr.in\r\n")?;
    w.put(b"User-Agent: curl/8.0\r\n")?;
    w.put(b"Connection: close\r\n\r\n")?;
    Some(w.len)
}

fn parse(response: &[u8]) -> Option<Update> {
    let body = body_of(response)?;
    let text = core::str::from_utf8(body).ok()?;

    // Body looks like: "~HH:MM:SS+ZZZZ~Condition text~" (possibly wrapped in chunk framing).
    let mut parts = text.split('~');
    parts.next()?; // anything before the first marker (chunk size / leading whitespace)
    let time = parts.next()?;
    let condition = parts.next()?;

    let secs_of_day = parse_time(time)?;
    Some(Update {
        weather: WeatherState::from_condition(condition),
        time: TimeSync {
            secs_of_day,
            at: Instant::now(),
        },
    })
}

/// Return the slice after the `\r\n\r\n` header/body separator.
fn body_of(response: &[u8]) -> Option<&[u8]> {
    response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| &response[i + 4..])
}

/// Parse "HH:MM:SS..." into seconds-of-day.
fn parse_time(s: &str) -> Option<u32> {
    let b = s.as_bytes();
    if b.len() < 8 || b[2] != b':' || b[5] != b':' {
        return None;
    }
    let hh = two_digits(b[0], b[1])?;
    let mm = two_digits(b[3], b[4])?;
    let ss = two_digits(b[6], b[7])?;
    if hh > 23 || mm > 59 || ss > 59 {
        return None;
    }
    Some(hh as u32 * 3600 + mm as u32 * 60 + ss as u32)
}

fn two_digits(hi: u8, lo: u8) -> Option<u8> {
    if !hi.is_ascii_digit() || !lo.is_ascii_digit() {
        return None;
    }
    Some((hi - b'0') * 10 + (lo - b'0'))
}

/// Minimal bounds-checked byte writer over a fixed slice.
struct Cursor<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Cursor { buf, len: 0 }
    }

    fn put(&mut self, bytes: &[u8]) -> Option<()> {
        let end = self.len.checked_add(bytes.len())?;
        if end > self.buf.len() {
            return None;
        }
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Some(())
    }
}
