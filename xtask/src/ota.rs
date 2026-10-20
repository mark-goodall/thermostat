//! Host-side update clients: serial protocol and HTTP.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use thermostat_core::proto::{self, FrameReader, MAX_DATA, MAX_FRAME, kind};

pub trait Port: Read + Write {}
impl<T: Read + Write> Port for T {}

/// Open `tcp:HOST:PORT` (QEMU console) or a serial device.
pub fn open_port(spec: &str, baud: u32) -> Result<Box<dyn Port>> {
    if let Some(addr) = spec.strip_prefix("tcp:") {
        let s = TcpStream::connect(addr).with_context(|| format!("connecting to {addr}"))?;
        s.set_read_timeout(Some(Duration::from_millis(50)))?;
        s.set_nodelay(true)?;
        Ok(Box::new(s))
    } else {
        let p = serialport::new(spec, baud)
            .timeout(Duration::from_millis(50))
            .open()
            .with_context(|| format!("opening {spec}"))?;
        Ok(Box::new(p))
    }
}

pub struct Client<'a> {
    port: &'a mut dyn Port,
    reader: FrameReader,
    seq: u8,
    /// Print device log text seen between protocol frames.
    pub echo_logs: bool,
    log_line: Vec<u8>,
}

impl<'a> Client<'a> {
    pub fn new(port: &'a mut dyn Port) -> Self {
        Self {
            port,
            reader: FrameReader::new(),
            seq: 0,
            echo_logs: true,
            log_line: Vec::new(),
        }
    }

    /// Send a request and wait for its reply, retransmitting on timeout.
    pub fn request(&mut self, k: u8, payload: &[u8], timeout: Duration) -> Result<(u8, Vec<u8>)> {
        self.seq = self.seq.wrapping_add(1);
        let mut frame = [0u8; MAX_FRAME];
        let n = proto::encode_frame(k, self.seq, payload, &mut frame);
        for _attempt in 0..5 {
            self.port.write_all(&frame[..n])?;
            self.port.flush()?;
            if let Some(reply) = self.wait_reply(timeout)? {
                return Ok(reply);
            }
        }
        bail!("no reply from device (kind {k:#x})")
    }

    fn wait_reply(&mut self, timeout: Duration) -> Result<Option<(u8, Vec<u8>)>> {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 256];
        while Instant::now() < deadline {
            let n = match self.port.read(&mut buf) {
                Ok(n) => n,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    0
                }
                Err(e) => return Err(e.into()),
            };
            for &b in &buf[..n] {
                self.echo(b);
                if let Some(frame) = self.reader.push(b)
                    && let Some((k, seq, payload)) = proto::decode_frame(frame)
                    && seq == self.seq
                    && (k == kind::ACK || k == kind::NAK)
                {
                    return Ok(Some((k, payload.to_vec())));
                }
            }
        }
        Ok(None)
    }

    /// Mirror printable console output so the user sees device logs.
    fn echo(&mut self, b: u8) {
        if !self.echo_logs {
            return;
        }
        match b {
            b'\n' => {
                let line = String::from_utf8_lossy(&self.log_line);
                if line
                    .chars()
                    .filter(|c| c.is_ascii_graphic() || *c == ' ')
                    .count()
                    * 10
                    >= line.len() * 9
                {
                    eprintln!("  | {}", line.trim_end());
                }
                self.log_line.clear();
            }
            0 => self.log_line.clear(),
            b => {
                if self.log_line.len() < 512 {
                    self.log_line.push(b)
                }
            }
        }
    }
}

fn expect_ack(reply: (u8, Vec<u8>), what: &str) -> Result<Vec<u8>> {
    match reply {
        (kind::ACK, p) => Ok(p),
        (_, p) => bail!(
            "{what} rejected by device (code {})",
            p.first().copied().unwrap_or(0)
        ),
    }
}

pub fn serial(image: &[u8], port: &str, baud: u32, reboot: bool) -> Result<()> {
    let mut port = open_port(port, baud)?;
    serial_update(&mut *port, image, reboot)
}

pub fn serial_update(port: &mut dyn Port, image: &[u8], reboot: bool) -> Result<()> {
    let mut c = Client::new(port);
    let version = expect_ack(
        c.request(kind::HELLO, &[], Duration::from_secs(1))?,
        "hello",
    )?;
    eprintln!("device runs {}", String::from_utf8_lossy(&version));
    expect_ack(
        c.request(
            kind::BEGIN,
            &(image.len() as u32).to_le_bytes(),
            Duration::from_secs(5),
        )?,
        "begin",
    )?;
    let start = Instant::now();
    for (i, chunk) in image.chunks(MAX_DATA).enumerate() {
        let mut p = ((i * MAX_DATA) as u32).to_le_bytes().to_vec();
        p.extend_from_slice(chunk);
        // Generous timeout: a chunk may cross a sector that needs erasing.
        expect_ack(c.request(kind::DATA, &p, Duration::from_secs(3))?, "data")?;
        if i % 64 == 0 {
            eprintln!("  {} / {} bytes", i * MAX_DATA, image.len());
        }
    }
    let v = expect_ack(
        c.request(kind::FINISH, &[], Duration::from_secs(30))?,
        "finish",
    )?;
    eprintln!(
        "staged {} in {:.1}s",
        String::from_utf8_lossy(&v),
        start.elapsed().as_secs_f32()
    );
    if reboot {
        expect_ack(
            c.request(kind::REBOOT, &[], Duration::from_secs(2))?,
            "reboot",
        )?;
        eprintln!("rebooting into the new image");
    }
    Ok(())
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            s.push(if i <= c.len() {
                T[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    s
}

/// Minimal blocking HTTP request; returns (status, body).
pub fn http_request(
    host: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<(u16, String)> {
    let addr = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:80")
    };
    let mut s = TcpStream::connect(&addr).with_context(|| format!("connecting to {addr}"))?;
    s.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes())?;
    for chunk in body.chunks(4096) {
        s.write_all(chunk)?;
    }
    let mut resp = Vec::new();
    s.read_to_end(&mut resp)?;
    let text = String::from_utf8_lossy(&resp).into_owned();
    let status = text
        .get(9..12)
        .and_then(|s| s.parse().ok())
        .context("malformed HTTP response")?;
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Ok((status, body))
}

pub fn http(image: &[u8], host: &str, user: &str, password: &str) -> Result<()> {
    let auth = format!("Basic {}", base64(format!("{user}:{password}").as_bytes()));
    eprintln!("uploading {} bytes to {host}", image.len());
    let (status, body) = http_request(
        host,
        "POST",
        "/api/update",
        &[
            ("Authorization", &auth),
            ("Content-Type", "application/octet-stream"),
        ],
        image,
    )?;
    if status != 200 {
        bail!("upload failed ({status}): {body}");
    }
    eprintln!("{body}");
    Ok(())
}
