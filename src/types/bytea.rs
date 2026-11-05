//! PostgreSQL bytea on the wire.
//!
//! SQLite hands back BLOB values as raw bytes. PostgreSQL's text result format
//! for bytea is hex: `\x` followed by two lowercase hex digits per byte. Sending
//! the raw bytes in a text-format column made every client decode garbage
//! (node-postgres read `de ad 00 ff` back as `ad 00 fd`), so result rows carry
//! binary BLOBs in the hex text form, and the binary-format encoder turns that
//! form back into raw bytes.
//!
//! A BLOB that is printable ASCII without a backslash is sent unchanged: bytea
//! clients decode it as escape format, which yields the same bytes, and a text
//! column whose value SQLite returns as a BLOB stays readable. Non-ASCII goes
//! to hex because clients decode escape format per character, not per byte.

/// Render a SQLite BLOB for a text-format result cell.
pub fn blob_to_text_format(blob: &[u8]) -> Vec<u8> {
    if is_escape_safe_text(blob) {
        return blob.to_vec();
    }
    let mut out = Vec::with_capacity(2 + blob.len() * 2);
    out.extend_from_slice(b"\\x");
    out.extend_from_slice(hex::encode(blob).as_bytes());
    out
}

fn is_escape_safe_text(blob: &[u8]) -> bool {
    blob.iter().all(|&b| (0x20..=0x7e).contains(&b) && b != b'\\')
}

/// Raw bytes for a binary-format bytea column. A cell in hex text form (from
/// `blob_to_text_format`, or a bytea literal pgsqlite stored as text) is
/// decoded; anything else is already raw and passes through.
pub fn text_format_to_raw(cell: &[u8]) -> Vec<u8> {
    if let Some(digits) = cell.strip_prefix(b"\\x")
        && let Ok(raw) = hex::decode(digits)
    {
        return raw;
    }
    cell.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_renders_as_hex_text() {
        assert_eq!(blob_to_text_format(&[0xde, 0xad, 0x00, 0xff]), b"\\xdead00ff".to_vec());
    }

    #[test]
    fn printable_ascii_blob_is_sent_unchanged() {
        assert_eq!(blob_to_text_format(b"Hello, mundo"), b"Hello, mundo".to_vec());
        assert_eq!(blob_to_text_format(b""), b"".to_vec());
    }

    #[test]
    fn backslash_and_control_bytes_force_hex() {
        assert_eq!(blob_to_text_format(b"a\\b"), b"\\x615c62".to_vec());
        assert_eq!(blob_to_text_format(b"a\nb"), b"\\x610a62".to_vec());
        assert_eq!(blob_to_text_format("ñ".as_bytes()), b"\\xc3b1".to_vec());
    }

    #[test]
    fn hex_text_round_trips_to_raw() {
        let raw = vec![0x00, 0x5c, 0x78, 0xff, 0x0a];
        assert_eq!(text_format_to_raw(&blob_to_text_format(&raw)), raw);
    }

    #[test]
    fn non_hex_cells_pass_through() {
        assert_eq!(text_format_to_raw(b"plain"), b"plain".to_vec());
        assert_eq!(text_format_to_raw(b"\\xzz"), b"\\xzz".to_vec());
    }
}
