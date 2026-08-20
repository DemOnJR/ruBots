//! Steam-emulator ("RevEmu") client identity.
//!
//! Port of `internal/auth/auth.go`. A GoldSrc server running `prot 3` demands a
//! binary STEAM authentication certificate appended to the `connect` packet;
//! this module fabricates one, exactly as `BuildRevEmu` does.
//!
//! Verified from `BuildRevEmu` (`0x1406DC400`) and `CDKeyHash` (`0x1406DC700`):
//!
//! * the default key is the literal `"AIPLAYER0000001"` (15 bytes)
//! * the hash seeds at `0x4E67C6A7` (`mov edx, 0x4e67c6a7`) and mixes each byte
//!   with `h ^= c + (h >> 2) + (h << 5)` (`shr edi,2` / `shl edi,5` / two
//!   `add esi` / `xor edx, esi`), stopping early at a NUL
//! * the certificate is `0x98` = 152 bytes (`mov ebx, 0x98` before `makeslice`)
//! * header: `u32 0x4A`, `u32 hash`, `u32 0x00726576` (`"ver\0"`), `u32 0`,
//!   then a `u64` SteamID whose low half is `hash * 2` (`lea esi,[rdx+rdx]`)
//!   and whose high half is `0x01100001`
//! * the key follows at offset `0x18`, clamped to 128 bytes (`cmp r13,0x7f` /
//!   `mov ecx,0x80` / `cmovle`)
//! * `CDKeyHash` is `crypto/md5.Sum` rendered with the `0123456789abcdef`
//!   alphabet, i.e. lowercase hex
//!
//! **Validated end to end:** a real HLDS `1.1.2.7/Stdio` server accepts this
//! certificate and replies `B 2 "…" 0 10211` (connection accepted). Dropping
//! the `cdkey` info field instead gets `Invalid hashed CD key.`, so both halves
//! are required.

use md5::{Digest, Md5};

/// Fallback identity when no key is supplied.
pub const DEFAULT_KEY: &[u8] = b"AIPLAYER0000001";

/// Size of the certificate blob.
pub const CERT_LEN: usize = 0x98;

/// Offset of the key field inside the certificate.
pub const CERT_KEY_OFFSET: usize = 0x18;

/// Longest key the certificate can carry.
pub const CERT_KEY_MAX: usize = 128;

/// Initial hash state.
pub const HASH_SEED: u32 = 0x4E67C6A7;

/// High 32 bits of the synthesised SteamID (individual account, public universe).
pub const STEAMID_HIGH: u64 = 0x0110_0001;

/// The RevEmu key hash: `h ^= c + (h >> 2) + (h << 5)` per byte.
pub fn revemu_hash(key: &[u8]) -> u32 {
    let mut h = HASH_SEED;
    for &c in key {
        if c == 0 {
            break;
        }
        let mixed = u32::from(c)
            .wrapping_add(h >> 2)
            .wrapping_add(h << 5);
        h ^= mixed;
    }
    h
}

/// The 64-bit SteamID this key resolves to.
pub fn steam_id(key: &[u8]) -> u64 {
    let h = revemu_hash(key);
    (STEAMID_HIGH << 32) | u64::from(h.wrapping_mul(2))
}

/// Build the 152-byte STEAM authentication certificate.
pub fn build_revemu(key: &[u8]) -> [u8; CERT_LEN] {
    let key = if key.is_empty() { DEFAULT_KEY } else { key };
    let h = revemu_hash(key);

    let mut cert = [0u8; CERT_LEN];
    cert[0x00..0x04].copy_from_slice(&0x4Au32.to_le_bytes());
    cert[0x04..0x08].copy_from_slice(&h.to_le_bytes());
    cert[0x08..0x0C].copy_from_slice(&0x0072_6576u32.to_le_bytes()); // "ver\0"
    cert[0x0C..0x10].copy_from_slice(&0u32.to_le_bytes());
    cert[0x10..0x18].copy_from_slice(&steam_id(key).to_le_bytes());

    let n = key.len().min(CERT_KEY_MAX);
    cert[CERT_KEY_OFFSET..CERT_KEY_OFFSET + n].copy_from_slice(&key[..n]);
    cert
}

/// Lowercase MD5 hex of the key — the `cdkey` info-string field.
pub fn cdkey_hash(key: &[u8]) -> String {
    let key = if key.is_empty() { DEFAULT_KEY } else { key };
    let mut h = Md5::new();
    h.update(key);
    let digest = h.finalize();
    let mut out = String::with_capacity(32);
    for b in digest {
        out.push(char::from_digit(u32::from(b >> 4), 16).unwrap());
        out.push(char::from_digit(u32::from(b & 0xF), 16).unwrap());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every expected value below was produced by the decoded algorithm AND
    // accepted by a live HLDS 1.1.2.7/Stdio server.
    const KNOWN_HASH: u32 = 0xBD1B_BD46;
    const KNOWN_STEAMID: u64 = 76_561_200_010_721_932;

    #[test]
    fn hash_matches_the_live_validated_value() {
        assert_eq!(revemu_hash(DEFAULT_KEY), KNOWN_HASH);
    }

    #[test]
    fn steamid_matches_the_live_validated_value() {
        assert_eq!(steam_id(DEFAULT_KEY), KNOWN_STEAMID);
        // Low half is exactly twice the hash.
        assert_eq!(
            (steam_id(DEFAULT_KEY) & 0xFFFF_FFFF) as u32,
            KNOWN_HASH.wrapping_mul(2)
        );
    }

    #[test]
    fn certificate_header_matches_the_wire_bytes() {
        let cert = build_revemu(DEFAULT_KEY);
        assert_eq!(cert.len(), 152);
        let expected_prefix: [u8; 32] = [
            0x4a, 0x00, 0x00, 0x00, // 0x4A
            0x46, 0xbd, 0x1b, 0xbd, // hash, little endian
            0x76, 0x65, 0x72, 0x00, // "ver\0"
            0x00, 0x00, 0x00, 0x00, // zero
            0x8c, 0x7a, 0x37, 0x7a, 0x01, 0x00, 0x10, 0x01, // steamid
            b'A', b'I', b'P', b'L', b'A', b'Y', b'E', b'R', // key begins
        ];
        assert_eq!(&cert[..32], &expected_prefix);
    }

    #[test]
    fn key_lands_at_offset_0x18_and_is_nul_padded() {
        let cert = build_revemu(DEFAULT_KEY);
        assert_eq!(&cert[CERT_KEY_OFFSET..CERT_KEY_OFFSET + 15], DEFAULT_KEY);
        assert!(
            cert[CERT_KEY_OFFSET + 15..].iter().all(|b| *b == 0),
            "remainder of the key field must be zero padded"
        );
    }

    #[test]
    fn empty_key_falls_back_to_the_default() {
        assert_eq!(build_revemu(b""), build_revemu(DEFAULT_KEY));
        assert_eq!(cdkey_hash(b""), cdkey_hash(DEFAULT_KEY));
    }

    #[test]
    fn hash_stops_at_an_embedded_nul() {
        assert_eq!(revemu_hash(b"AB\0CD"), revemu_hash(b"AB"));
    }

    #[test]
    fn overlong_keys_are_clamped_not_panicking() {
        let long = vec![b'X'; 300];
        let cert = build_revemu(&long);
        assert_eq!(cert.len(), CERT_LEN);
        assert_eq!(cert[CERT_LEN - 1], b'X', "key fills to the end of the blob");
    }

    #[test]
    fn cdkey_hash_is_lowercase_md5_hex() {
        let h = cdkey_hash(DEFAULT_KEY);
        assert_eq!(h.len(), 32);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // md5("AIPLAYER0000001") -- this exact value was accepted by the live
        // server as the `cdkey` info field.
        assert_eq!(h, "a5d48e484cdcfcf1bf8d03623bd1c855");
    }

    #[test]
    fn different_keys_give_different_identities() {
        assert_ne!(revemu_hash(b"PLAYER_ONE"), revemu_hash(b"PLAYER_TWO"));
        assert_ne!(steam_id(b"PLAYER_ONE"), steam_id(b"PLAYER_TWO"));
    }
}
