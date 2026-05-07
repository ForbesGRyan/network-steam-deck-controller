//! Beacon packet wire format. Fixed size, signed by the sender.

use sha2::{Digest, Sha256};
use std::fmt::Write as _;

pub const MAGIC: [u8; 4] = *b"NDB1";
pub const VERSION: u8 = 1;
pub const PUBKEY_LEN: usize = 32;
pub const SIG_LEN: usize = 64;
pub const NAME_MAX: usize = 32;
pub const FPR_LEN: usize = 8;

/// Bytes signed by the sender (everything before the trailing signature).
pub const SIGNED_LEN: usize = 4 + 1 + 1 + 1 + 1 + PUBKEY_LEN + FPR_LEN + 8 + NAME_MAX;
/// Total wire size.
pub const PACKET_LEN: usize = SIGNED_LEN + SIG_LEN;

pub const FLAG_PAIRING: u8 = 0x01;
pub const FLAG_ACCEPT: u8 = 0x02;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BeaconPacket {
    pub flags: u8,
    pub pubkey: [u8; PUBKEY_LEN],
    pub peer_fpr: [u8; FPR_LEN],
    pub timestamp_us: u64,
    pub name: String, // up to NAME_MAX bytes utf-8
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketError {
    Short,
    BadMagic,
    BadVersion(u8),
    BadNameLen(u8),
    BadName,
}

/// Encode the body (everything except the signature) into `out[..SIGNED_LEN]`.
///
/// # Errors
///
/// Returns [`PacketError::BadNameLen`] if `p.name` exceeds [`NAME_MAX`] bytes.
pub fn encode_body(p: &BeaconPacket, out: &mut [u8; PACKET_LEN]) -> Result<(), PacketError> {
    if p.name.len() > NAME_MAX {
        return Err(PacketError::BadNameLen(
            u8::try_from(p.name.len()).unwrap_or(u8::MAX),
        ));
    }
    out.fill(0);
    out[0..4].copy_from_slice(&MAGIC);
    out[4] = VERSION;
    out[5] = p.flags;
    // SAFETY: name.len() <= NAME_MAX == 32, so it always fits in a u8.
    #[allow(clippy::cast_possible_truncation)]
    let name_len_byte = p.name.len() as u8;
    out[6] = name_len_byte;
    out[7] = 0;
    out[8..40].copy_from_slice(&p.pubkey);
    out[40..48].copy_from_slice(&p.peer_fpr);
    out[48..56].copy_from_slice(&p.timestamp_us.to_le_bytes());
    out[56..56 + p.name.len()].copy_from_slice(p.name.as_bytes());
    Ok(())
}

/// Decode a packet from raw bytes. Does NOT verify the signature.
///
/// # Errors
///
/// Returns [`PacketError::Short`] if `buf` is shorter than [`PACKET_LEN`],
/// [`PacketError::BadMagic`] if the magic bytes do not match,
/// [`PacketError::BadVersion`] if the version field is not [`VERSION`],
/// [`PacketError::BadNameLen`] if the encoded name length exceeds [`NAME_MAX`], or
/// [`PacketError::BadName`] if the name bytes are not valid UTF-8.
///
/// # Panics
///
/// Never panics. The `try_into().unwrap()` on the timestamp slice is infallible because
/// the slice is explicitly bounds-checked to exactly 8 bytes before the call.
pub fn decode(buf: &[u8]) -> Result<(BeaconPacket, [u8; SIG_LEN]), PacketError> {
    if buf.len() < PACKET_LEN {
        return Err(PacketError::Short);
    }
    if buf[0..4] != MAGIC {
        return Err(PacketError::BadMagic);
    }
    if buf[4] != VERSION {
        return Err(PacketError::BadVersion(buf[4]));
    }
    let flags = buf[5];
    let name_len = buf[6];
    if name_len as usize > NAME_MAX {
        return Err(PacketError::BadNameLen(name_len));
    }
    let mut pubkey = [0_u8; PUBKEY_LEN];
    pubkey.copy_from_slice(&buf[8..40]);
    let mut peer_fpr = [0_u8; FPR_LEN];
    peer_fpr.copy_from_slice(&buf[40..48]);
    let timestamp_us = u64::from_le_bytes(buf[48..56].try_into().unwrap());
    let name_bytes = &buf[56..56 + name_len as usize];
    let name = std::str::from_utf8(name_bytes)
        .map_err(|_| PacketError::BadName)?
        .to_owned();
    let mut sig = [0_u8; SIG_LEN];
    sig.copy_from_slice(&buf[SIGNED_LEN..PACKET_LEN]);
    Ok((
        BeaconPacket {
            flags,
            pubkey,
            peer_fpr,
            timestamp_us,
            name,
        },
        sig,
    ))
}

/// 8-byte fingerprint = first 8 bytes of SHA256(pubkey).
#[must_use]
pub fn fingerprint(pubkey: &[u8; PUBKEY_LEN]) -> [u8; FPR_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(pubkey);
    let digest = hasher.finalize();
    let mut out = [0_u8; FPR_LEN];
    out.copy_from_slice(&digest[..FPR_LEN]);
    out
}

/// Render a fingerprint as `aa:bb:cc:dd:ee:ff:gg:hh`.
#[must_use]
pub fn fingerprint_str(fpr: &[u8; FPR_LEN]) -> String {
    let mut s = String::with_capacity(FPR_LEN * 3 - 1);
    for (i, b) in fpr.iter().enumerate() {
        if i > 0 {
            s.push(':');
        }
        write!(s, "{b:02x}").unwrap();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BeaconPacket {
        BeaconPacket {
            flags: FLAG_PAIRING,
            pubkey: [7_u8; PUBKEY_LEN],
            peer_fpr: [0_u8; FPR_LEN],
            timestamp_us: 0xCAFE_BABE_DEAD_BEEF,
            name: "deck-living-room".to_owned(),
        }
    }

    #[test]
    fn roundtrip_body() {
        let p = sample();
        let mut buf = [0_u8; PACKET_LEN];
        encode_body(&p, &mut buf).unwrap();
        // Pretend signature is all-zero for the body roundtrip check.
        let (got, _sig) = decode(&buf).unwrap();
        assert_eq!(got, p);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = [0_u8; PACKET_LEN];
        encode_body(&sample(), &mut buf).unwrap();
        buf[0] ^= 0xff;
        assert!(matches!(decode(&buf), Err(PacketError::BadMagic)));
    }

    #[test]
    fn rejects_bad_version() {
        let mut buf = [0_u8; PACKET_LEN];
        encode_body(&sample(), &mut buf).unwrap();
        buf[4] = 99;
        assert!(matches!(decode(&buf), Err(PacketError::BadVersion(99))));
    }

    #[test]
    fn rejects_oversize_name() {
        let mut p = sample();
        p.name = "x".repeat(NAME_MAX + 1);
        let mut buf = [0_u8; PACKET_LEN];
        assert!(matches!(encode_body(&p, &mut buf), Err(PacketError::BadNameLen(_))));
    }

    #[test]
    fn fingerprint_format() {
        let fpr = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x11, 0x22];
        assert_eq!(fingerprint_str(&fpr), "aa:bb:cc:dd:ee:ff:11:22");
    }

    #[test]
    fn rejects_short_buf() {
        let buf = [0_u8; PACKET_LEN - 1];
        assert!(matches!(decode(&buf), Err(PacketError::Short)));
    }

    #[test]
    fn rejects_bad_utf8_name() {
        let mut buf = [0_u8; PACKET_LEN];
        encode_body(&sample(), &mut buf).unwrap();
        buf[6] = 1;        // name_len = 1
        buf[56] = 0xFF;    // first name byte: not valid UTF-8
        assert!(matches!(decode(&buf), Err(PacketError::BadName)));
    }
}
