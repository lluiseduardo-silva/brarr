//! The torrent infohash — the identity a torrent client answers to.
//!
//! qBittorrent's `torrents/add` replies with a bare `Ok.` and names
//! nothing, so "which torrent did I just add?" cannot be answered by
//! reading the response. It does not need to be: the infohash is a
//! property of the file brarr already holds, and every torrent client
//! keys on it. Radarr does exactly this — it parses the `.torrent`,
//! takes the hash, and uses it as the download id from then on.
//!
//! The hash is the SHA-1 of the **bencoded `info` dictionary**, byte for
//! byte as it appears in the file. That last part is why this module
//! scans for the value's byte span instead of decoding and re-encoding:
//! a round-trip through any data structure risks reordering keys or
//! normalising integers, and either would produce a different — wrong —
//! hash.

use sha1::{Digest, Sha1};

/// Hex-encoded SHA-1 of the `info` dictionary, lowercase.
///
/// Returns `None` when the bytes are not a well-formed torrent, which
/// the caller should treat as "cannot identify" rather than as a hard
/// failure: the release may still download fine, it just cannot be
/// followed afterwards.
pub(crate) fn from_torrent(bytes: &[u8]) -> Option<String> {
    let (start, end) = info_span(bytes)?;
    let mut hasher = Sha1::new();
    hasher.update(&bytes[start..end]);
    Some(hex(&hasher.finalize()))
}

/// Infohash out of a magnet's `xt=urn:btih:` parameter.
///
/// Hex (40 chars) only. The base32 spelling (32 chars) is legal and
/// rare; returning `None` for it costs the ability to follow that one
/// download, which beats guessing an identity.
pub(crate) fn from_magnet(uri: &str) -> Option<String> {
    let query = uri.split_once('?').map(|(_, q)| q)?;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if !key.eq_ignore_ascii_case("xt") {
            continue;
        }
        let Some(hash) = value.strip_prefix("urn:btih:") else {
            continue;
        };
        if hash.len() == 40 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

/// Byte range of the value of the top-level `info` key.
fn info_span(bytes: &[u8]) -> Option<(usize, usize)> {
    // A torrent file is one dictionary at the top level.
    if bytes.first() != Some(&b'd') {
        return None;
    }
    let mut i = 1;
    while i < bytes.len() {
        if bytes[i] == b'e' {
            return None; // dictionary ended without an `info` key
        }
        let (key, after_key) = scan_string(bytes, i)?;
        let after_value = scan_value(bytes, after_key)?;
        if key == b"info" {
            return Some((after_key, after_value));
        }
        i = after_value;
    }
    None
}

/// Read a bencoded byte string at `i`, returning it and the index just
/// past it. Keys in a bencoded dictionary are always byte strings.
fn scan_string(bytes: &[u8], i: usize) -> Option<(&[u8], usize)> {
    let colon = bytes.get(i..)?.iter().position(|&b| b == b':')? + i;
    let len: usize = std::str::from_utf8(bytes.get(i..colon)?)
        .ok()?
        .parse()
        .ok()?;
    let start = colon + 1;
    let end = start.checked_add(len)?;
    if end > bytes.len() {
        return None;
    }
    Some((&bytes[start..end], end))
}

/// Index just past the bencoded value starting at `i`.
fn scan_value(bytes: &[u8], i: usize) -> Option<usize> {
    match bytes.get(i)? {
        // i<digits>e
        b'i' => {
            let end = bytes.get(i..)?.iter().position(|&b| b == b'e')? + i;
            Some(end + 1)
        }
        // l<values>e
        b'l' => {
            let mut j = i + 1;
            loop {
                if bytes.get(j)? == &b'e' {
                    return Some(j + 1);
                }
                j = scan_value(bytes, j)?;
            }
        }
        // d<key><value>…e
        b'd' => {
            let mut j = i + 1;
            loop {
                if bytes.get(j)? == &b'e' {
                    return Some(j + 1);
                }
                let (_, after_key) = scan_string(bytes, j)?;
                j = scan_value(bytes, after_key)?;
            }
        }
        // <len>:<bytes>
        b'0'..=b'9' => scan_string(bytes, i).map(|(_, end)| end),
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(40), |mut out, b| {
        // Writing to a String is infallible; the result is discarded
        // rather than unwrapped to keep the no-unwrap rule.
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests assert on happy paths")]
mod tests {
    use super::*;

    /// A minimal but structurally real torrent: announce, a nested info
    /// dict, and a key *after* info so the scanner has to find the end
    /// of the value rather than run to the end of the file.
    const TORRENT: &[u8] = b"d8:announce11:https://t/a4:infod6:lengthi1024e4:name10:Matrix.mkv12:piece lengthi16384e6:pieces0:e13:creation datei1700000000ee";

    /// The exact bytes the hash must be taken over.
    const INFO: &[u8] = b"d6:lengthi1024e4:name10:Matrix.mkv12:piece lengthi16384e6:pieces0:e";

    #[test]
    fn the_info_span_is_exact() {
        // The whole point: the hash is over these bytes verbatim, so an
        // off-by-one here silently produces a valid-looking wrong hash.
        let (start, end) = info_span(TORRENT).unwrap();
        assert_eq!(&TORRENT[start..end], INFO);
    }

    #[test]
    fn the_hash_matches_sha1_of_the_info_dict() {
        // Ground truth computed outside this crate, over INFO's 67 bytes:
        //   [Security.Cryptography.SHA1]::Create().ComputeHash($bytes)
        assert_eq!(
            from_torrent(TORRENT).as_deref(),
            Some("659d65ffe26eab1ba01deb5a4d3daeb91d46e715")
        );
        assert_eq!(
            INFO.len(),
            67,
            "the fixture the ground truth was taken over"
        );
    }

    #[test]
    fn a_nested_list_inside_info_does_not_confuse_the_scanner() {
        // Real torrents carry `files` (a list of dicts) and `announce-list`
        // (a list of lists). Both have to be walked, not guessed past.
        let torrent: &[u8] = b"d13:announce-listll11:https://t/aee4:infod5:filesld6:lengthi1e4:pathl1:aeee4:name1:xee";
        let (start, end) = info_span(torrent).unwrap();
        assert_eq!(
            &torrent[start..end],
            b"d5:filesld6:lengthi1e4:pathl1:aeee4:name1:xe"
        );
    }

    #[test]
    fn junk_is_refused_rather_than_hashed() {
        // The HTML login page `deliver.rs` guards against would otherwise
        // hash into a perfectly plausible-looking id.
        assert!(from_torrent(b"<!DOCTYPE html>").is_none());
        assert!(from_torrent(b"").is_none());
        assert!(from_torrent(b"d8:announce3:abce").is_none(), "no info key");
        assert!(from_torrent(b"d4:infod").is_none(), "truncated mid-value");
    }

    #[test]
    fn a_magnet_yields_its_btih() {
        assert_eq!(
            from_magnet("magnet:?xt=urn:btih:9A1DC7DCDF5B1E6B8B78E3E5E10A2F4B1D20A3A7&dn=Matrix")
                .as_deref(),
            Some("9a1dc7dcdf5b1e6b8b78e3e5e10a2f4b1d20a3a7"),
            "case is normalised — qBittorrent answers lowercase"
        );
        assert_eq!(
            from_magnet("magnet:?dn=Matrix&xt=urn:btih:0000000000000000000000000000000000000001")
                .as_deref(),
            Some("0000000000000000000000000000000000000001"),
            "the parameter is not always first"
        );
    }

    #[test]
    fn a_base32_magnet_is_declined_rather_than_guessed() {
        // 32 chars, legal, and not what a client keys on without decoding.
        assert!(from_magnet("magnet:?xt=urn:btih:ABCDEFGHIJKLMNOPQRSTUVWXYZ234567").is_none());
        assert!(from_magnet("magnet:?dn=no-xt-here").is_none());
        assert!(from_magnet("not-a-magnet").is_none());
    }
}
