//! Bidirectional identifier mapping between downstream and upstream.
//!
//! What the client sends identifies its machine and session (installation id,
//! session / thread / turn ids, prompt cache key); what upstream returns
//! identifies our account's routing state (`x-codex-turn-state`) and its
//! responses (`resp_…`). Neither side should see the other's identifiers:
//!
//! * downstream → upstream: every client id is replaced by `map_uuid(id)`, a
//!   keyed hash formatted as a UUID. Deterministic per account key, so the same
//!   client session always maps to the same upstream ids (sticky routing and the
//!   prompt cache keep working) without a lookup table.
//! * upstream → downstream: opaque upstream values are `seal`ed into tokens the
//!   client echoes back (turn state header, `previous_response_id`); `open`
//!   recovers the original. Sealing is deterministic (same input, same token) so
//!   an id repeated across SSE events stays consistent, and reversible without
//!   storage — encrypt-then-MAC built from HMAC-SHA256 only.
//!
//! The key lives in `<CODEX_HOME>/asxs-idmap.key` (created on first use) so a
//! restart does not change the mapping.

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::Hmac;
use hmac::Mac;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const KEY_FILE: &str = "asxs-idmap.key";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

pub struct IdMap {
    key: [u8; 32],
}

impl IdMap {
    /// Load the account's mapping key, creating it on first use.
    pub fn load(codex_home: &Path) -> anyhow::Result<Self> {
        let path = codex_home.join(KEY_FILE);
        if let Ok(hex) = std::fs::read_to_string(&path)
            && let Some(key) = parse_hex32(hex.trim())
        {
            return Ok(Self { key });
        }
        let key: [u8; 32] = rand::random();
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        let tmp = codex_home.join(format!("{KEY_FILE}.tmp"));
        std::fs::write(&tmp, hex)?;
        std::fs::rename(&tmp, &path)?;
        Ok(Self { key })
    }

    #[cfg(test)]
    pub fn from_key(key: [u8; 32]) -> Self {
        Self { key }
    }

    fn mac(&self, label: &[u8], data: &[u8]) -> [u8; 32] {
        let mut m = HmacSha256::new_from_slice(&self.key).expect("hmac accepts any key length");
        m.update(label);
        m.update(&[0]);
        m.update(data);
        m.finalize().into_bytes().into()
    }

    /// Deterministic stand-in for a client id, shaped like a UUID v4.
    pub fn map_uuid(&self, id: &str) -> String {
        let h = self.mac(b"uuid", id.as_bytes());
        let mut b = [0u8; 16];
        b.copy_from_slice(&h[..16]);
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        uuid::Uuid::from_bytes(b).to_string()
    }

    /// Reversible, deterministic token for an opaque upstream value.
    pub fn seal(&self, plain: &[u8]) -> String {
        let nonce_full = self.mac(b"nonce", plain);
        let nonce = &nonce_full[..NONCE_LEN];
        let ct = self.xor_stream(nonce, plain);
        let mut tagged = Vec::with_capacity(NONCE_LEN + ct.len());
        tagged.extend_from_slice(nonce);
        tagged.extend_from_slice(&ct);
        let tag = self.mac(b"tag", &tagged);
        let mut out = tagged;
        out.extend_from_slice(&tag[..TAG_LEN]);
        URL_SAFE_NO_PAD.encode(out)
    }

    /// Recover a value sealed by this key; `None` for anything else.
    pub fn open(&self, token: &str) -> Option<Vec<u8>> {
        let raw = URL_SAFE_NO_PAD.decode(token).ok()?;
        if raw.len() < NONCE_LEN + TAG_LEN {
            return None;
        }
        let (tagged, tag) = raw.split_at(raw.len() - TAG_LEN);
        let expect = self.mac(b"tag", tagged);
        if !constant_time_eq(tag, &expect[..TAG_LEN]) {
            return None;
        }
        let (nonce, ct) = tagged.split_at(NONCE_LEN);
        let plain = self.xor_stream(nonce, ct);
        // Deterministic nonce doubles as an integrity check on the plaintext.
        if self.mac(b"nonce", &plain)[..NONCE_LEN] != *nonce {
            return None;
        }
        Some(plain)
    }

    fn xor_stream(&self, nonce: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        let mut counter: u32 = 0;
        while out.len() < data.len() {
            let mut input = Vec::with_capacity(nonce.len() + 4);
            input.extend_from_slice(nonce);
            input.extend_from_slice(&counter.to_be_bytes());
            let block = self.mac(b"stream", &input);
            for b in block {
                if out.len() == data.len() {
                    break;
                }
                out.push(data[out.len()] ^ b);
            }
            counter += 1;
        }
        out
    }
}

fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some(out)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_mapping_is_deterministic_and_keyed() {
        let a = IdMap::from_key([1; 32]);
        let b = IdMap::from_key([2; 32]);
        let id = "0e5d2033-f9e2-467e-b0af-8635a87b8bbe";
        let m = a.map_uuid(id);
        assert_eq!(m, a.map_uuid(id));
        assert_ne!(m, id);
        assert_ne!(m, b.map_uuid(id));
        assert!(uuid::Uuid::parse_str(&m).is_ok());
        assert_eq!(&m[14..15], "4");
    }

    #[test]
    fn seal_roundtrip() {
        let m = IdMap::from_key([7; 32]);
        let blob = b"resp_0bd94bc792496946016aa1f8b080d487d09f73f4a07fe1340b";
        let t = m.seal(blob);
        assert_eq!(t, m.seal(blob), "deterministic");
        assert_eq!(m.open(&t).as_deref(), Some(&blob[..]));
        assert!(m.open("not-a-token").is_none());
        assert!(IdMap::from_key([8; 32]).open(&t).is_none(), "other key");
        let mut broken = t.clone();
        broken.replace_range(5..6, if &t[5..6] == "A" { "B" } else { "A" });
        assert!(m.open(&broken).is_none(), "tampered");
        assert!(m.seal(b"").len() > 0);
        assert_eq!(m.open(&m.seal(b"")).as_deref(), Some(&b""[..]));
    }

    #[test]
    fn key_file_persists() {
        let dir = std::env::temp_dir().join(format!("idmap-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = IdMap::load(&dir).unwrap().map_uuid("x");
        let b = IdMap::load(&dir).unwrap().map_uuid("x");
        assert_eq!(a, b);
        std::fs::remove_dir_all(&dir).ok();
    }
}
