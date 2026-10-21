//! Minimal HTTP/1.1 request handling for the on-device web server.
//!
//! One request per connection (`Connection: close`), bounded header size, and
//! streaming bodies so firmware images never need to fit in RAM.

use core::fmt;

use embedded_io_async::{Read, Write};
use heapless::String;

pub const MAX_HEAD: usize = 2048;
const MAX_HEADERS: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpError {
    Io,
    /// Peer closed before a full request arrived.
    Closed,
    HeadTooLarge,
    Malformed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Other,
}

#[derive(Debug)]
pub struct Request<'a> {
    pub method: Method,
    pub path: &'a str,
    pub query: &'a str,
    pub content_length: usize,
    pub cookie: Option<&'a str>,
    pub authorization: Option<&'a str>,
}

impl<'a> Request<'a> {
    /// Value of cookie `name`.
    pub fn cookie(&self, name: &str) -> Option<&'a str> {
        self.cookie?.split(';').find_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            (k == name).then_some(v)
        })
    }

    pub fn query_value(&self, key: &str) -> Option<&'a str> {
        form_value(self.query, key)
    }
}

/// Read until the end of the request head. Returns `(head_len, bytes_read)`;
/// bytes after `head_len` are the start of the body.
pub async fn read_head<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(usize, usize), HttpError> {
    let mut len = 0;
    loop {
        if len == buf.len() {
            return Err(HttpError::HeadTooLarge);
        }
        let n = r.read(&mut buf[len..]).await.map_err(|_| HttpError::Io)?;
        if n == 0 {
            return Err(HttpError::Closed);
        }
        let start = len.saturating_sub(3);
        len += n;
        if let Some(pos) = buf[start..len].windows(4).position(|w| w == b"\r\n\r\n") {
            return Ok((start + pos + 4, len));
        }
    }
}

pub fn parse(head: &[u8]) -> Result<Request<'_>, HttpError> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(head) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err(HttpError::Malformed),
    }
    let method = match req.method {
        Some("GET") | Some("HEAD") => Method::Get,
        Some("POST") => Method::Post,
        _ => Method::Other,
    };
    let target = req.path.ok_or(HttpError::Malformed)?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut out = Request {
        method,
        path,
        query,
        content_length: 0,
        cookie: None,
        authorization: None,
    };
    for h in req.headers.iter() {
        let value = core::str::from_utf8(h.value).map_err(|_| HttpError::Malformed)?;
        if h.name.eq_ignore_ascii_case("content-length") {
            out.content_length = value.trim().parse().map_err(|_| HttpError::Malformed)?;
        } else if h.name.eq_ignore_ascii_case("cookie") {
            out.cookie = Some(value);
        } else if h.name.eq_ignore_ascii_case("authorization") {
            out.authorization = Some(value);
        }
    }
    // httparse borrows the headers array; re-slice the strings from `head`
    // so the request only borrows the input buffer.
    Ok(Request {
        path: reborrow(head, out.path),
        query: reborrow(head, out.query),
        cookie: out.cookie.map(|c| reborrow(head, c)),
        authorization: out.authorization.map(|a| reborrow(head, a)),
        ..out
    })
}

fn reborrow<'a>(head: &'a [u8], s: &str) -> &'a str {
    if s.is_empty() {
        return "";
    }
    let start = s.as_ptr() as usize - head.as_ptr() as usize;
    core::str::from_utf8(&head[start..start + s.len()]).unwrap_or("")
}

/// Raw (still percent-encoded) value of `key` in an urlencoded string.
pub fn form_value<'a>(form: &'a str, key: &str) -> Option<&'a str> {
    form.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == key).then_some(v)
    })
}

/// All raw values of a repeated key (e.g. checkboxes), in order.
pub fn form_values<'a>(form: &'a str, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    form.split('&').filter_map(move |kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == key).then_some(v)
    })
}

/// Percent-decode an urlencoded value (`+` is a space).
pub fn url_decode<const N: usize>(raw: &str) -> Option<String<N>> {
    let mut bytes = heapless::Vec::<u8, N>::new();
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = match b[i] {
            b'+' => b' ',
            b'%' => {
                let hi = (*b.get(i + 1)? as char).to_digit(16)?;
                let lo = (*b.get(i + 2)? as char).to_digit(16)?;
                i += 2;
                (hi * 16 + lo) as u8
            }
            c => c,
        };
        bytes.push(c).ok()?;
        i += 1;
    }
    String::from_utf8(bytes).ok()
}

/// Decoded form field, empty when absent; `None` if malformed or too long.
pub fn form_field<const N: usize>(form: &str, key: &str) -> Option<String<N>> {
    match form_value(form, key) {
        Some(raw) => url_decode(raw),
        None => Some(String::new()),
    }
}

/// HTML-escaping formatter for untrusted text.
pub struct Escape<'a>(pub &'a str);

impl fmt::Display for Escape<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in self.0.chars() {
            match c {
                '<' => f.write_str("&lt;")?,
                '>' => f.write_str("&gt;")?,
                '&' => f.write_str("&amp;")?,
                '"' => f.write_str("&quot;")?,
                '\'' => f.write_str("&#39;")?,
                c => fmt::Write::write_char(f, c)?,
            }
        }
        Ok(())
    }
}

/// Fixed buffer implementing `fmt::Write`; output past the end is dropped and
/// flagged.
pub struct BufWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
    pub overflow: bool,
}

impl<'a> BufWriter<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buf,
            len: 0,
            overflow: false,
        }
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl fmt::Write for BufWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        if n < s.len() {
            self.overflow = true;
        }
        Ok(())
    }
}

pub async fn write_all<W: Write>(w: &mut W, data: &[u8]) -> Result<(), HttpError> {
    w.write_all(data).await.map_err(|_| HttpError::Io)
}

/// Parse a decimal like "20.5" into tenths (205).
pub fn parse_tenths(s: &str) -> Option<i32> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (int, frac) = s.split_once('.').unwrap_or((s, "0"));
    if int.is_empty()
        || frac.len() != 1
        || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let v = int.parse::<i32>().ok()?.checked_mul(10)? + frac.parse::<i32>().ok()?;
    Some(if neg { -v } else { v })
}

/// Format tenths as a decimal string.
pub struct Tenths(pub i32);

impl fmt::Display for Tenths {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        write!(f, "{}{}.{}", sign, self.0.abs() / 10, self.0.abs() % 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request() {
        let head = b"POST /login?next=%2F HTTP/1.1\r\nHost: x\r\nContent-Length: 12\r\nCookie: a=1; session=abc\r\n\r\n";
        let r = parse(head).unwrap();
        assert_eq!(r.method, Method::Post);
        assert_eq!(r.path, "/login");
        assert_eq!(r.query_value("next"), Some("%2F"));
        assert_eq!(r.content_length, 12);
        assert_eq!(r.cookie("session"), Some("abc"));
        assert_eq!(r.cookie("missing"), None);
    }

    #[test]
    fn decode_forms() {
        let form = "ssid=My+Net%21&pass=&x";
        assert_eq!(form_field::<32>(form, "ssid").unwrap(), "My Net!");
        assert_eq!(form_field::<32>(form, "pass").unwrap(), "");
        assert_eq!(form_field::<32>(form, "absent").unwrap(), "");
        assert!(form_field::<4>(form, "ssid").is_none(), "too long");
        assert!(url_decode::<8>("%zz").is_none());
        let multi = "a=1&sensor=AA%3A01&b=2&sensor=BB%3A02";
        assert_eq!(
            form_values(multi, "sensor").collect::<std::vec::Vec<_>>(),
            ["AA%3A01", "BB%3A02"]
        );
    }

    #[test]
    fn tenths() {
        assert_eq!(parse_tenths("20.5"), Some(205));
        assert_eq!(parse_tenths("21"), Some(210));
        assert_eq!(parse_tenths("-1.5"), Some(-15));
        assert_eq!(parse_tenths("1.25"), None);
        assert_eq!(parse_tenths("abc"), None);
        assert_eq!(std::format!("{}", Tenths(-15)), "-1.5");
    }

    #[test]
    fn escape() {
        assert_eq!(
            std::format!("{}", Escape("<a href=\"x\">&'")),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }
}
