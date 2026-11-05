//! PostgreSQL bytea on the wire.
//!
//! SQLite hands back BLOB values as raw bytes. PostgreSQL's text result format
//! for bytea is hex: `\x` followed by two lowercase hex digits per byte. Sending
//! the raw bytes in a text-format column made every client decode garbage
//! (node-postgres read `de ad 00 ff` back as `ad 00 fd`), so result rows carry
//! binary BLOBs in the hex text form, and the binary-format encoder turns that
//! form back into raw bytes.
//!
//! Rows are built from the SQLite storage class alone, without the column's
//! PostgreSQL type, so a BLOB that is valid UTF-8 is sent unchanged, exactly as
//! before: a text column whose value SQLite returns as a BLOB must stay
//! readable. Only BLOBs that are not valid UTF-8 (real binary data, which no
//! text column can hold) switch to hex.

/// Render a SQLite BLOB for a text-format result cell.
pub fn blob_to_text_format(blob: &[u8]) -> Vec<u8> {
    if std::str::from_utf8(blob).is_ok() {
        return blob.to_vec();
    }
    let mut out = Vec::with_capacity(2 + blob.len() * 2);
    out.extend_from_slice(b"\\x");
    out.extend_from_slice(hex::encode(blob).as_bytes());
    out
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
    fn utf8_blob_is_sent_unchanged() {
        assert_eq!(blob_to_text_format(b"Hello, mundo"), b"Hello, mundo".to_vec());
        assert_eq!(blob_to_text_format("caf\u{e9}".as_bytes()), "caf\u{e9}".as_bytes().to_vec());
        assert_eq!(blob_to_text_format(b""), b"".to_vec());
    }

    #[test]
    fn non_utf8_blob_is_hex() {
        assert_eq!(blob_to_text_format(&[0x50, 0x4b, 0x03, 0x04, 0xff]), b"\\x504b0304ff".to_vec());
    }

    #[test]
    fn hex_text_round_trips_to_raw() {
        let raw = vec![0x00, 0x5c, 0x78, 0xff, 0x0a];
        assert!(std::str::from_utf8(&raw).is_err());
        assert_eq!(text_format_to_raw(&blob_to_text_format(&raw)), raw);
    }

    #[test]
    fn non_hex_cells_pass_through() {
        assert_eq!(text_format_to_raw(b"plain"), b"plain".to_vec());
        assert_eq!(text_format_to_raw(b"\\xzz"), b"\\xzz".to_vec());
    }
}
