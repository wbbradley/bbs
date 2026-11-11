//! Byte budgets for new blocker descriptions and response configurations.

use anyhow::{Result, bail};

pub const MAX_BODY_BYTES: usize = 2 * 1024;
/// Covers the response options only; a goal proposal's goal is stored beside
/// them and bounded by `blocker_choices::MAX_GOAL_OBJECTIVE`.
pub const MAX_CHOICES_BYTES: usize = 1024;
/// The database's bound on a stored body, which for a goal proposal includes
/// the goal the server appends.
pub const MAX_STORED_BODY_BYTES: usize = 8 * 1024;

pub fn validate_body(body: &str) -> Result<()> {
    if body.trim().is_empty() || body.len() > MAX_BODY_BYTES {
        bail!(
            "blocker details must be nonblank and at most 2 KiB ({MAX_BODY_BYTES} UTF-8 bytes); received {} bytes",
            body.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_count_utf8_and_markdown_bytes_without_trimming() {
        for body in [
            "x".repeat(MAX_BODY_BYTES),
            "é".repeat(MAX_BODY_BYTES / 2),
            format!("**{}**", "x".repeat(MAX_BODY_BYTES - 4)),
        ] {
            validate_body(&body).unwrap();
            let too_big = format!("{body}\n");
            let error = validate_body(&too_big).unwrap_err().to_string();
            assert!(error.contains("2 KiB (2048 UTF-8 bytes)"), "{error}");
            assert!(error.contains("received 2049 bytes"), "{error}");
        }
        assert!(
            validate_body("  \n  ")
                .unwrap_err()
                .to_string()
                .contains("nonblank")
        );
    }
}
