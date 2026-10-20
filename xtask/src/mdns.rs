//! Minimal mDNS client over a raw Ethernet frame tap, for checking the
//! device's responder in QEMU.

use std::net::{Ipv4Addr, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub const TYPE_A: u16 = 1;
pub const TYPE_PTR: u16 = 12;
pub const TYPE_TXT: u16 = 16;
pub const TYPE_SRV: u16 = 33;

#[derive(Debug, Clone, PartialEq)]
pub enum Data {
    A(Ipv4Addr),
    Ptr(String),
    Txt(Vec<String>),
    Srv { port: u16, target: String },
    Other(u16),
}

#[derive(Debug, Clone)]
pub struct Record {
    pub name: String,
    pub data: Data,
}

/// Answers and additional records of one response.
#[derive(Debug, Default)]
pub struct Response {
    pub answers: Vec<Record>,
    pub additional: Vec<Record>,
}

impl Response {
    pub fn all(&self) -> impl Iterator<Item = &Record> {
        self.answers.iter().chain(&self.additional)
    }
}

fn checksum(b: &[u8]) -> u16 {
    let mut sum: u32 = b
        .chunks(2)
        .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32)
        .sum();
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// An Ethernet frame carrying a multicast mDNS query from a made-up peer.
fn query_frame(name: &str, qtype: u16) -> Vec<u8> {
    let mut dns = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in name.split('.') {
        dns.push(label.len() as u8);
        dns.extend_from_slice(label.as_bytes());
    }
    dns.push(0);
    dns.extend_from_slice(&qtype.to_be_bytes());
    dns.extend_from_slice(&1u16.to_be_bytes());

    let mut udp = Vec::new();
    udp.extend_from_slice(&5353u16.to_be_bytes());
    udp.extend_from_slice(&5353u16.to_be_bytes());
    udp.extend_from_slice(&((8 + dns.len()) as u16).to_be_bytes());
    udp.extend_from_slice(&[0, 0]); // no UDP checksum (allowed for IPv4)
    udp.extend_from_slice(&dns);

    let mut ip = vec![0x45, 0];
    ip.extend_from_slice(&((20 + udp.len()) as u16).to_be_bytes());
    ip.extend_from_slice(&[0, 1, 0, 0, 255, 17, 0, 0, 10, 0, 2, 99, 224, 0, 0, 251]);
    let c = checksum(&ip);
    ip[10..12].copy_from_slice(&c.to_be_bytes());

    let mut frame = vec![
        0x01, 0x00, 0x5e, 0x00, 0x00, 0xfb, 0x02, 0, 0, 0, 0, 0x99, 0x08, 0x00,
    ];
    frame.extend_from_slice(&ip);
    frame.extend_from_slice(&udp);
    frame
}

fn read_name(msg: &[u8], mut pos: usize) -> Result<(String, usize)> {
    let mut labels = Vec::new();
    let mut end = None;
    for _ in 0..64 {
        let len = *msg.get(pos).context("truncated name")? as usize;
        if len == 0 {
            return Ok((labels.join("."), end.unwrap_or(pos + 1)));
        }
        if len & 0xC0 == 0xC0 {
            let ptr = (len & 0x3F) << 8 | *msg.get(pos + 1).context("truncated pointer")? as usize;
            end.get_or_insert(pos + 2);
            pos = ptr;
            continue;
        }
        let label = msg.get(pos + 1..pos + 1 + len).context("truncated label")?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        pos += 1 + len;
    }
    bail!("name compression loop")
}

pub fn parse(msg: &[u8]) -> Result<Response> {
    let u16_at = |p: usize| -> Result<u16> {
        Ok(u16::from_be_bytes(
            msg.get(p..p + 2).context("truncated")?.try_into().unwrap(),
        ))
    };
    let (qd, an, ns, ar) = (u16_at(4)?, u16_at(6)?, u16_at(8)?, u16_at(10)?);
    let mut pos = 12;
    for _ in 0..qd {
        pos = read_name(msg, pos)?.1 + 4;
    }
    let mut out = Response::default();
    for i in 0..(an + ns + ar) as usize {
        let (name, p) = read_name(msg, pos)?;
        let rtype = u16_at(p)?;
        let rdlen = u16_at(p + 8)? as usize;
        let rd = p + 10;
        let rdata = msg.get(rd..rd + rdlen).context("truncated rdata")?;
        let data = match rtype {
            TYPE_A if rdlen == 4 => Data::A(Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3])),
            TYPE_PTR => Data::Ptr(read_name(msg, rd)?.0),
            TYPE_SRV => Data::Srv {
                port: u16_at(rd + 4)?,
                target: read_name(msg, rd + 6)?.0,
            },
            TYPE_TXT => {
                let mut strings = Vec::new();
                let mut q = 0;
                while q < rdata.len() {
                    let l = rdata[q] as usize;
                    strings.push(
                        String::from_utf8_lossy(&rdata[q + 1..(q + 1 + l).min(rdata.len())])
                            .into_owned(),
                    );
                    q += 1 + l;
                }
                Data::Txt(strings)
            }
            t => Data::Other(t),
        };
        let rec = Record { name, data };
        if i < an as usize {
            out.answers.push(rec)
        } else {
            out.additional.push(rec)
        }
        pos = rd + rdlen;
    }
    Ok(out)
}

/// Send one query into the emulated link and return the first mDNS response.
pub fn query(
    local_port: u16,
    qemu_port: u16,
    name: &str,
    qtype: u16,
    timeout: Duration,
) -> Result<Response> {
    let sock = UdpSocket::bind(("127.0.0.1", local_port))?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    let deadline = Instant::now() + timeout;
    let mut next_send = Instant::now();
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        if Instant::now() >= next_send {
            sock.send_to(&query_frame(name, qtype), ("127.0.0.1", qemu_port))?;
            next_send = Instant::now() + Duration::from_secs(1);
        }
        let Ok((n, _)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let f = &buf[..n];
        // IPv4 / UDP from port 5353 that is not our own query echoed back by the hub.
        if n > 42
            && f[12..14] == [0x08, 0x00]
            && f[23] == 17
            && f[34..36] == 5353u16.to_be_bytes()
            && f[26..30] != [10, 0, 2, 99]
        {
            let resp = parse(&f[42..])?;
            if !resp.answers.is_empty() {
                return Ok(resp);
            }
        }
    }
    bail!("no mDNS response for {name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_frame_is_well_formed() {
        let f = query_frame("thermostat.local", TYPE_A);
        assert_eq!(&f[30..34], &[224, 0, 0, 251]);
        assert_eq!(checksum(&f[14..34]), 0, "IP header checksum");
        let dns = &f[42..];
        assert_eq!(read_name(dns, 12).unwrap().0, "thermostat.local");
    }

    #[test]
    fn parses_compressed_response() {
        // header: id 0, flags 8400, 0 questions, 2 answers
        let mut m = vec![0, 0, 0x84, 0, 0, 0, 0, 2, 0, 0, 0, 0];
        // thermostat.local A 10.0.2.15
        m.extend_from_slice(b"\x0athermostat\x05local\x00");
        m.extend_from_slice(&[0, 1, 0x80, 1, 0, 0, 0, 120, 0, 4, 10, 0, 2, 15]);
        // _thermostat._tcp.local PTR -> thermostat.<ptr to "_thermostat._tcp.local">
        let svc = m.len();
        m.extend_from_slice(b"\x0b_thermostat\x04_tcp\xc0\x17");
        m.extend_from_slice(&[0, 12, 0, 1, 0, 0, 0, 120, 0, 13]);
        m.extend_from_slice(b"\x0athermostat");
        m.extend_from_slice(&[0xC0, svc as u8]);
        let r = parse(&m).unwrap();
        assert_eq!(r.answers[0].data, Data::A(Ipv4Addr::new(10, 0, 2, 15)));
        assert_eq!(r.answers[1].name, "_thermostat._tcp.local");
        assert_eq!(
            r.answers[1].data,
            Data::Ptr("thermostat._thermostat._tcp.local".into())
        );
    }
}
