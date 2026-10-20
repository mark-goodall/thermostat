//! Minimal DHCP server for the setup access point.
//!
//! Hands out addresses from a small pool to clients joining the device's own
//! access point so a phone or laptop can reach the web UI. Supports
//! DISCOVER/OFFER and REQUEST/ACK|NAK, which is all clients need.

pub const SERVER_PORT: u16 = 67;
pub const CLIENT_PORT: u16 = 68;

const MAGIC: [u8; 4] = [99, 130, 83, 99];
const FIXED_LEN: usize = 236;
const POOL_START: u8 = 10;
const POOL_SIZE: usize = 16;
const LEASE_SECS: u32 = 3600;

const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;

pub struct DhcpServer {
    /// Server address; the pool is `a.b.c.10 ..= a.b.c.25`.
    pub ip: [u8; 4],
    leases: [Option<[u8; 6]>; POOL_SIZE],
}

impl DhcpServer {
    pub const fn new(ip: [u8; 4]) -> Self {
        Self {
            ip,
            leases: [None; POOL_SIZE],
        }
    }

    fn addr(&self, slot: usize) -> [u8; 4] {
        [self.ip[0], self.ip[1], self.ip[2], POOL_START + slot as u8]
    }

    fn slot_for(&mut self, mac: [u8; 6]) -> Option<usize> {
        if let Some(i) = self.leases.iter().position(|l| *l == Some(mac)) {
            return Some(i);
        }
        let i = self.leases.iter().position(|l| l.is_none())?;
        self.leases[i] = Some(mac);
        Some(i)
    }

    /// Handle a request; writes the reply into `out` and returns its length.
    /// Replies go to the broadcast address on [`CLIENT_PORT`].
    pub fn handle(&mut self, req: &[u8], out: &mut [u8]) -> Option<usize> {
        if req.len() < FIXED_LEN + 4
            || req[0] != 1
            || req[1] != 1
            || req[2] != 6
            || req[FIXED_LEN..FIXED_LEN + 4] != MAGIC
        {
            return None;
        }
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&req[28..34]);
        let mut msg_type = None;
        let mut requested = None;
        let mut opts = &req[FIXED_LEN + 4..];
        while let [code, rest @ ..] = opts {
            match *code {
                0 => opts = rest,
                255 => break,
                _ => {
                    let [len, rest @ ..] = rest else { break };
                    let len = *len as usize;
                    let Some(val) = rest.get(..len) else { break };
                    match (*code, len) {
                        (53, 1) => msg_type = Some(val[0]),
                        (50, 4) => requested = Some([val[0], val[1], val[2], val[3]]),
                        _ => {}
                    }
                    opts = &rest[len..];
                }
            }
        }
        let (reply, yiaddr) = match msg_type? {
            DISCOVER => {
                let slot = self.slot_for(mac)?;
                (OFFER, self.addr(slot))
            }
            REQUEST => {
                let slot = self.slot_for(mac)?;
                let ours = self.addr(slot);
                let wanted = requested.unwrap_or([req[12], req[13], req[14], req[15]]);
                if wanted == ours {
                    (ACK, ours)
                } else {
                    (NAK, [0; 4])
                }
            }
            _ => return None,
        };

        let out = out.get_mut(..FIXED_LEN + 4 + 64)?;
        out.fill(0);
        out[0] = 2; // BOOTREPLY
        out[1] = 1;
        out[2] = 6;
        out[4..8].copy_from_slice(&req[4..8]); // xid
        out[10..12].copy_from_slice(&req[10..12]); // flags
        out[16..20].copy_from_slice(&yiaddr);
        out[20..24].copy_from_slice(&self.ip);
        out[28..44].copy_from_slice(&req[28..44]); // chaddr
        out[FIXED_LEN..FIXED_LEN + 4].copy_from_slice(&MAGIC);
        let mut o = FIXED_LEN + 4;
        let mut opt = |code: u8, val: &[u8]| {
            out[o] = code;
            out[o + 1] = val.len() as u8;
            out[o + 2..o + 2 + val.len()].copy_from_slice(val);
            o += 2 + val.len();
        };
        opt(53, &[reply]);
        opt(54, &self.ip);
        if reply != NAK {
            opt(51, &LEASE_SECS.to_be_bytes());
            opt(1, &[255, 255, 255, 0]);
            opt(3, &self.ip);
        }
        out[o] = 255;
        Some(o + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(msg: u8, mac: [u8; 6], requested: Option<[u8; 4]>) -> std::vec::Vec<u8> {
        let mut p = std::vec![0u8; FIXED_LEN];
        p[0] = 1;
        p[1] = 1;
        p[2] = 6;
        p[4..8].copy_from_slice(&[1, 2, 3, 4]);
        p[28..34].copy_from_slice(&mac);
        p.extend_from_slice(&MAGIC);
        p.extend_from_slice(&[53, 1, msg]);
        if let Some(r) = requested {
            p.extend_from_slice(&[50, 4]);
            p.extend_from_slice(&r);
        }
        p.push(255);
        p
    }

    fn reply_type(out: &[u8]) -> u8 {
        assert_eq!(out[FIXED_LEN..FIXED_LEN + 4], MAGIC);
        assert_eq!(out[FIXED_LEN + 4], 53);
        out[FIXED_LEN + 6]
    }

    #[test]
    fn discover_request_ack() {
        let mut s = DhcpServer::new([192, 168, 4, 1]);
        let mut out = [0u8; 400];
        let mac = [2, 0, 0, 0, 0, 1];
        let n = s.handle(&packet(DISCOVER, mac, None), &mut out).unwrap();
        assert_eq!(reply_type(&out[..n]), OFFER);
        let offered = [out[16], out[17], out[18], out[19]];
        assert_eq!(offered, [192, 168, 4, 10]);
        assert_eq!(out[4..8], [1, 2, 3, 4]);
        let n = s
            .handle(&packet(REQUEST, mac, Some(offered)), &mut out)
            .unwrap();
        assert_eq!(reply_type(&out[..n]), ACK);
        // A second client gets the next address; the first keeps its own.
        let n = s
            .handle(&packet(DISCOVER, [2, 0, 0, 0, 0, 2], None), &mut out)
            .unwrap();
        assert_eq!(out[19], 11);
        assert_eq!(reply_type(&out[..n]), OFFER);
        s.handle(&packet(DISCOVER, mac, None), &mut out).unwrap();
        assert_eq!(out[19], 10);
    }

    #[test]
    fn wrong_address_is_naked() {
        let mut s = DhcpServer::new([192, 168, 4, 1]);
        let mut out = [0u8; 400];
        let n = s
            .handle(&packet(REQUEST, [2; 6], Some([10, 0, 0, 5])), &mut out)
            .unwrap();
        assert_eq!(reply_type(&out[..n]), NAK);
    }

    #[test]
    fn ignores_garbage() {
        let mut s = DhcpServer::new([192, 168, 4, 1]);
        let mut out = [0u8; 400];
        assert!(s.handle(&[0; 50], &mut out).is_none());
        let mut p = packet(DISCOVER, [2; 6], None);
        p.truncate(FIXED_LEN + 6); // option cut short
        assert!(s.handle(&p, &mut out).is_none());
    }
}
