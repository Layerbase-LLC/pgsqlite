//! Shared SQL identifier normalization.
//!
//! PostgreSQL clients (Laravel/Doctrine, Django, Rails, ...) routinely quote
//! every identifier: `CREATE TABLE "themes" ("created_at" datetime)`. SQLite
//! accepts the quoted form and stores the *unquoted* name, which is what
//! `PRAGMA table_info` reports. pgsqlite, however, used to copy the raw token
//! straight out of the SQL text into the `__pgsqlite_*` metadata tables, so the
//! metadata ended up holding `"created_at"` (quotes embedded in the TEXT value)
//! while SQLite held `created_at`.
//!
//! Nothing matched at runtime (lookups query by the clean name) and the startup
//! schema-drift validator reported every column of every table as both missing
//! from SQLite and missing from metadata, which hard-fails startup and bricks
//! the database. Every place that persists an identifier must funnel through
//! [`normalize_identifier`] so both sides agree on a single canonical spelling.

/// Strip SQL double-quote quoting from an identifier.
///
/// A quoted identifier keeps its interior verbatim except that a doubled `""`
/// is the escape for a literal double quote, so `"my""col"` normalizes to
/// `my"col`. Unquoted input is returned trimmed and otherwise untouched -
/// notably the case is preserved, because SQLite (and pgsqlite's metadata) are
/// case-preserving.
pub fn normalize_identifier(identifier: &str) -> String {
    let trimmed = identifier.trim();

    if is_quoted_identifier(trimmed) {
        trimmed[1..trimmed.len() - 1].replace("\"\"", "\"")
    } else {
        trimmed.to_string()
    }
}

/// True when the value still carries surrounding double quotes, i.e. it is a
/// raw SQL token rather than a normalized identifier.
pub fn is_quoted_identifier(identifier: &str) -> bool {
    let trimmed = identifier.trim();
    trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"')
}

/// Render an identifier back into its quoted SQL form, escaping any interior
/// double quotes. Used when a normalized name has to be interpolated into SQL
/// text (e.g. `PRAGMA table_info("...")`).
pub fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_surrounding_double_quotes() {
        assert_eq!(normalize_identifier("\"created_at\""), "created_at");
        assert_eq!(normalize_identifier("\"themes\""), "themes");
    }

    #[test]
    fn leaves_unquoted_identifiers_alone() {
        assert_eq!(normalize_identifier("created_at"), "created_at");
        assert_eq!(normalize_identifier("  created_at  "), "created_at");
        assert_eq!(normalize_identifier("MixedCase"), "MixedCase");
    }

    #[test]
    fn unescapes_doubled_quotes_inside_quoted_identifiers() {
        assert_eq!(normalize_identifier("\"my\"\"col\""), "my\"col");
    }

    #[test]
    fn handles_degenerate_input() {
        assert_eq!(normalize_identifier(""), "");
        assert_eq!(normalize_identifier("\"\""), "");
        assert_eq!(normalize_identifier("\""), "\"");
    }

    #[test]
    fn detects_quoted_identifiers() {
        assert!(is_quoted_identifier("\"users\""));
        assert!(!is_quoted_identifier("users"));
        assert!(!is_quoted_identifier("\""));
    }

    #[test]
    fn quote_round_trips() {
        for name in ["users", "my\"col", "MixedCase"] {
            assert_eq!(normalize_identifier(&quote_identifier(name)), name);
        }
    }
}
