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
//! readable. BLOBs that are not valid UTF-8 (real binary data, which no text
//! column can hold) switch to hex, and so does any BLOB that itself starts with
//! `\x`: in a result cell a leading `\x` then always means hex, so the
//! binary-format decoder can never mistake raw bytes for encoded ones.
//!
//! The send paths then know each column's type: `encode_bytea_text_cells`
//! hex-encodes any text-format cell of a bytea column that is not already in
//! hex form, so a bytea value that happens to be valid UTF-8 is correct too.

/// Render a SQLite BLOB for a text-format result cell.
pub fn blob_to_text_format(blob: &[u8]) -> Vec<u8> {
    if !blob.starts_with(b"\\x") && std::str::from_utf8(blob).is_ok() {
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

/// PostgreSQL type OID of bytea.
pub const BYTEA_OID: i32 = 17;

fn is_hex_text_form(cell: &[u8]) -> bool {
    cell.strip_prefix(b"\\x")
        .is_some_and(|digits| digits.len() % 2 == 0 && digits.iter().all(u8::is_ascii_hexdigit))
}

/// Text-format cell for a column whose PostgreSQL type is bytea. Row builders
/// cannot see the column type, so they pass a valid UTF-8 BLOB through
/// unchanged; once the type is known, a cell not already in `\x` hex form is
/// raw bytes (a UTF-8 BLOB, or text stored in the bytea column) and is
/// hex-encoded, as PostgreSQL would send it.
pub fn bytea_column_text_cell(cell: &[u8]) -> Vec<u8> {
    if is_hex_text_form(cell) {
        return cell.to_vec();
    }
    let mut out = Vec::with_capacity(2 + cell.len() * 2);
    out.extend_from_slice(b"\\x");
    out.extend_from_slice(hex::encode(cell).as_bytes());
    out
}

/// Apply `bytea_column_text_cell` to every text-format bytea cell of a row.
/// `result_formats` follows the Bind rules: empty means all text, one entry
/// applies to every column, otherwise one entry per column.
pub fn encode_bytea_text_cells(row: &mut [Option<Vec<u8>>], field_types: &[i32], result_formats: &[i16]) {
    for (i, cell) in row.iter_mut().enumerate() {
        if field_types.get(i) != Some(&BYTEA_OID) {
            continue;
        }
        let format = match result_formats.len() {
            0 => 0,
            1 => result_formats[0],
            _ => result_formats.get(i).copied().unwrap_or(0),
        };
        if format != 0 {
            continue;
        }
        if let Some(bytes) = cell
            && !is_hex_text_form(bytes)
        {
            *bytes = bytea_column_text_cell(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytea_column_hex_encodes_raw_utf8() {
        assert_eq!(bytea_column_text_cell(b"a\\b"), b"\\x615c62".to_vec());
        assert_eq!(bytea_column_text_cell("caf\u{e9}".as_bytes()), b"\\x636166c3a9".to_vec());
        assert_eq!(bytea_column_text_cell(b""), b"\\x".to_vec());
    }

    #[test]
    fn bytea_column_keeps_hex_form() {
        assert_eq!(bytea_column_text_cell(b"\\xdead00ff"), b"\\xdead00ff".to_vec());
        assert_eq!(bytea_column_text_cell(b"\\xZZ"), b"\\x5c785a5a".to_vec());
        assert_eq!(bytea_column_text_cell(b"\\x123"), b"\\x5c78313233".to_vec());
    }

    #[test]
    fn only_text_format_bytea_cells_change() {
        let row = || vec![Some(b"a\\b".to_vec()), Some(b"a\\b".to_vec()), None];
        let types = [25, BYTEA_OID, BYTEA_OID];

        let mut text = row();
        encode_bytea_text_cells(&mut text, &types, &[]);
        assert_eq!(text, vec![Some(b"a\\b".to_vec()), Some(b"\\x615c62".to_vec()), None]);

        let mut binary = row();
        encode_bytea_text_cells(&mut binary, &types, &[1]);
        assert_eq!(binary, row());

        let mut mixed = row();
        encode_bytea_text_cells(&mut mixed, &types, &[0, 1, 0]);
        assert_eq!(mixed, row());
    }

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
    fn blob_that_looks_like_hex_text_round_trips() {
        let raw = b"\\x4142".to_vec();
        let cell = blob_to_text_format(&raw);
        assert_eq!(cell, b"\\x5c7834313432".to_vec());
        assert_eq!(text_format_to_raw(&cell), raw);
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
