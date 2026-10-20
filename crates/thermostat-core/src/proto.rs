//! Serial firmware update protocol.
//!
//! Runs on the console UART next to log output. Every packet is COBS encoded
//! and delimited by `0x00` on both sides; the device starts each reply with an
//! extra `0x00` so any preceding log text is cut off as a separate (invalid)
//! frame. Packets carry a CRC-32, so line noise or log text is never mistaken
//! for a command.
//!
//! ```text
//! packet = kind u8 | seq u8 | payload | crc32 (LE, over kind..payload)
//! ```
//!
//! | kind | direction | payload                                   |
//! |------|-----------|-------------------------------------------|
//! | 0x01 HELLO  | host → dev | –                                   |
//! | 0x02 BEGIN  | host → dev | total image length u32              |
//! | 0x03 DATA   | host → dev | offset u32, up to [`MAX_DATA`] bytes |
//! | 0x04 FINISH | host → dev | –                                   |
//! | 0x05 REBOOT | host → dev | –                                   |
//! | 0x06 ABORT  | host → dev | –                                   |
//! | 0x80 ACK    | dev → host | optional text (version on HELLO/FINISH) |
//! | 0x81 NAK    | dev → host | error code u8, text                  |
//!
//! Every host packet gets exactly one reply echoing its `seq`. The host
//! retransmits on timeout; DATA for an offset that was already written is
//! acknowledged again without rewriting, so retransmits are idempotent.

use crate::codec::cobs;
use crate::platform::{Ota, OtaError};

pub const MAX_DATA: usize = 1024;
pub const MAX_PACKET: usize = 2 + 4 + MAX_DATA + 4;
pub const MAX_FRAME: usize = cobs::max_encoded_len(MAX_PACKET) + 2;

const CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);

pub mod kind {
    pub const HELLO: u8 = 0x01;
    pub const BEGIN: u8 = 0x02;
    pub const DATA: u8 = 0x03;
    pub const FINISH: u8 = 0x04;
    pub const REBOOT: u8 = 0x05;
    pub const ABORT: u8 = 0x06;
    pub const ACK: u8 = 0x80;
    pub const NAK: u8 = 0x81;
}

/// Build an encoded frame (leading and trailing delimiter) into `out`.
pub fn encode_frame(kind: u8, seq: u8, payload: &[u8], out: &mut [u8]) -> usize {
    let mut pkt = [0u8; MAX_PACKET];
    let n = 2 + payload.len();
    pkt[0] = kind;
    pkt[1] = seq;
    pkt[2..n].copy_from_slice(payload);
    let crc = CRC.checksum(&pkt[..n]);
    pkt[n..n + 4].copy_from_slice(&crc.to_le_bytes());
    out[0] = 0;
    let len = cobs::encode(&pkt[..n + 4], &mut out[1..]);
    out[1 + len] = 0;
    len + 2
}

/// Decode one frame (without delimiters) in place into `(kind, seq, payload)`.
pub fn decode_frame(frame: &mut [u8]) -> Option<(u8, u8, &[u8])> {
    let n = cobs::decode_in_place(frame)?;
    if n < 6 {
        return None;
    }
    let crc = u32::from_le_bytes(frame[n - 4..n].try_into().ok()?);
    if CRC.checksum(&frame[..n - 4]) != crc {
        return None;
    }
    Some((frame[0], frame[1], &frame[2..n - 4]))
}

/// Accumulates delimited frames from a byte stream.
pub struct FrameReader {
    buf: [u8; MAX_FRAME],
    len: usize,
    overflow: bool,
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameReader {
    pub const fn new() -> Self {
        Self {
            buf: [0; MAX_FRAME],
            len: 0,
            overflow: false,
        }
    }

    /// Feed one byte. Returns a complete, undecoded frame when a delimiter arrives.
    pub fn push(&mut self, b: u8) -> Option<&mut [u8]> {
        if b == 0 {
            let len = core::mem::take(&mut self.len);
            if core::mem::take(&mut self.overflow) || len == 0 {
                return None;
            }
            return Some(&mut self.buf[..len]);
        }
        if self.len < MAX_FRAME {
            self.buf[self.len] = b;
            self.len += 1;
        } else {
            self.overflow = true;
        }
        None
    }
}

/// Error codes carried in NAK packets.
pub fn error_code(e: OtaError) -> u8 {
    match e {
        OtaError::Busy => 1,
        OtaError::Sequence => 2,
        OtaError::TooLarge => 3,
        OtaError::Invalid => 4,
        OtaError::Flash => 5,
        OtaError::NotConfirmed => 6,
    }
}

/// Device side of the protocol.
pub struct SerialOta {
    /// Bytes acknowledged so far in the current transfer.
    received: Option<u32>,
    pub reboot_requested: bool,
}

impl Default for SerialOta {
    fn default() -> Self {
        Self::new()
    }
}

impl SerialOta {
    pub const fn new() -> Self {
        Self {
            received: None,
            reboot_requested: false,
        }
    }

    /// Handle one decoded frame; writes the encoded reply into `out` and
    /// returns its length, or `None` for frames that are not ours.
    pub fn handle<O: Ota>(
        &mut self,
        frame: &mut [u8],
        ota: &mut O,
        out: &mut [u8],
    ) -> Option<usize> {
        let (k, seq, payload) = decode_frame(frame)?;
        let mut text = heapless::String::<48>::new();
        let result: Result<(), OtaError> = match k {
            kind::HELLO => {
                let _ = text.push_str(ota.running_version());
                Ok(())
            }
            kind::BEGIN => {
                let len = payload
                    .get(..4)
                    .map(|b| u32::from_le_bytes(b.try_into().unwrap()));
                match (len, self.received) {
                    // Retransmitted BEGIN whose ACK was lost.
                    (Some(_), Some(0)) => Ok(()),
                    (Some(len), _) => ota.ota_begin(len).map(|_| self.received = Some(0)),
                    (None, _) => Err(OtaError::Invalid),
                }
            }
            kind::DATA => match (payload.len() >= 4, self.received) {
                (true, Some(received)) => {
                    let offset = u32::from_le_bytes(payload[..4].try_into().unwrap());
                    let data = &payload[4..];
                    if offset + data.len() as u32 == received && !data.is_empty() {
                        Ok(()) // retransmit of the last chunk; already written
                    } else if offset != received {
                        Err(OtaError::Sequence)
                    } else {
                        ota.ota_write(offset, data)
                            .map(|_| self.received = Some(received + data.len() as u32))
                    }
                }
                _ => Err(OtaError::Sequence),
            },
            kind::FINISH => match self.received.take() {
                Some(_) => ota.ota_finish().map(|v| {
                    let _ = text.push_str(&v);
                }),
                None => Err(OtaError::Sequence),
            },
            kind::REBOOT => {
                self.reboot_requested = true;
                Ok(())
            }
            kind::ABORT => {
                self.received = None;
                ota.ota_abort();
                Ok(())
            }
            _ => return None,
        };
        Some(match result {
            Ok(()) => encode_frame(kind::ACK, seq, text.as_bytes(), out),
            Err(e) => {
                if (k == kind::BEGIN || k == kind::DATA || k == kind::FINISH)
                    && !matches!(e, OtaError::Sequence)
                {
                    self.received = None;
                    ota.ota_abort();
                }
                let mut p = [0u8; 1];
                p[0] = error_code(e);
                encode_frame(kind::NAK, seq, &p, out)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::tests::FakeOta;

    fn request(k: u8, seq: u8, payload: &[u8]) -> std::vec::Vec<u8> {
        let mut out = [0u8; MAX_FRAME];
        let n = encode_frame(k, seq, payload, &mut out);
        out[..n].to_vec()
    }

    /// Push a wire frame through a reader and the device; return the reply packet.
    fn roundtrip(
        dev: &mut SerialOta,
        ota: &mut FakeOta,
        wire: &[u8],
    ) -> Option<(u8, u8, std::vec::Vec<u8>)> {
        let mut reader = FrameReader::new();
        let mut reply = [0u8; MAX_FRAME];
        let mut n = None;
        for &b in wire {
            if let Some(frame) = reader.push(b) {
                n = dev.handle(frame, ota, &mut reply);
            }
        }
        let n = n?;
        let mut r = reply[..n].to_vec();
        assert_eq!(r[0], 0);
        assert_eq!(r[n - 1], 0);
        let (k, s, p) = decode_frame(&mut r[1..n - 1]).unwrap();
        Some((k, s, p.to_vec()))
    }

    #[test]
    fn full_transfer() {
        let mut dev = SerialOta::new();
        let mut ota = FakeOta::default();
        let image: std::vec::Vec<u8> = (0..3000u32).map(|i| i as u8).collect();

        let (k, _, p) = roundtrip(&mut dev, &mut ota, &request(kind::HELLO, 1, &[])).unwrap();
        assert_eq!((k, p.as_slice()), (kind::ACK, b"0.1.0-test".as_slice()));

        let len = (image.len() as u32).to_le_bytes();
        assert_eq!(
            roundtrip(&mut dev, &mut ota, &request(kind::BEGIN, 2, &len))
                .unwrap()
                .0,
            kind::ACK
        );
        for (i, chunk) in image.chunks(MAX_DATA).enumerate() {
            let mut p = ((i * MAX_DATA) as u32).to_le_bytes().to_vec();
            p.extend_from_slice(chunk);
            let wire = request(kind::DATA, 3 + i as u8, &p);
            let (k, s, _) = roundtrip(&mut dev, &mut ota, &wire).unwrap();
            assert_eq!((k, s), (kind::ACK, 3 + i as u8));
            // Retransmit is acknowledged without a second write.
            assert_eq!(roundtrip(&mut dev, &mut ota, &wire).unwrap().0, kind::ACK);
        }
        let (k, _, p) = roundtrip(&mut dev, &mut ota, &request(kind::FINISH, 9, &[])).unwrap();
        assert_eq!(k, kind::ACK);
        assert_eq!(p, b"new");
        assert_eq!(ota.written, image);
        assert!(ota.finished);
    }

    #[test]
    fn log_noise_and_corruption_are_ignored() {
        let mut dev = SerialOta::new();
        let mut ota = FakeOta::default();
        assert!(roundtrip(&mut dev, &mut ota, b"INFO some log line\r\n\0").is_none());
        let mut wire = request(kind::HELLO, 1, &[]);
        wire[3] ^= 0x40;
        assert!(roundtrip(&mut dev, &mut ota, &wire).is_none());
    }

    #[test]
    fn out_of_order_data_naks() {
        let mut dev = SerialOta::new();
        let mut ota = FakeOta::default();
        roundtrip(
            &mut dev,
            &mut ota,
            &request(kind::BEGIN, 1, &100u32.to_le_bytes()),
        )
        .unwrap();
        let mut p = 50u32.to_le_bytes().to_vec();
        p.extend_from_slice(&[1, 2, 3]);
        let (k, _, code) = roundtrip(&mut dev, &mut ota, &request(kind::DATA, 2, &p)).unwrap();
        assert_eq!((k, code[0]), (kind::NAK, error_code(OtaError::Sequence)));
    }
}
