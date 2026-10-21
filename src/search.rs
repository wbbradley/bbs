//! Bounded literal message search and excerpts. Unicode matching returns byte
//! ranges in the original text, so excerpts never need reconstructed prose.

use crate::db::{self, FeedFilter, MessageView};
use anyhow::{Result, bail};
use regex::{Regex, RegexBuilder};
use rusqlite::Connection;
use std::ops::Range;

pub const DEFAULT_LIMIT: u32 = 20;
pub const MAX_LIMIT: u32 = 100;
pub const TEXT_BUDGET: usize = 32 * 1024;
pub const MAX_QUERY_BYTES: usize = 4096;
const SCAN_BATCH: u32 = 64;

pub struct Matcher(Regex);
impl Matcher {
    pub fn new(query: &str, case_sensitive: bool) -> Result<Self> {
        if query.is_empty() || query.len() > MAX_QUERY_BYTES {
            bail!("search query must be 1 to {MAX_QUERY_BYTES} UTF-8 bytes");
        }
        // Escaping is unconditional: regex syntax is never part of the API.
        Ok(Self(
            RegexBuilder::new(&regex::escape(query))
                .case_insensitive(!case_sensitive)
                .build()?,
        ))
    }
    fn find(&self, body: &str) -> Option<Range<usize>> {
        self.0.find(body).map(|m| m.range())
    }
}

pub struct Hit {
    pub message: MessageView,
    pub original_body_bytes: usize,
    pub body_offset_bytes: usize,
    pub matched: Range<usize>,
}

/// Scan bounded batches in descending id order, retaining only the output
/// budget plus one candidate batch. Matching happens before the result limit.
pub fn messages(
    conn: &Connection,
    matcher: &Matcher,
    limit: usize,
    filter: &mut FeedFilter<'_>,
) -> Result<(Vec<Hit>, bool)> {
    let mut hits = Vec::new();
    let mut remaining = TEXT_BUDGET;
    loop {
        let batch = db::feed_messages(conn, SCAN_BATCH, filter)?;
        let count = batch.len();
        for mut message in batch.into_iter().rev() {
            filter.before_id = Some(message.id);
            let Some(matched) = matcher.find(&message.body) else {
                continue;
            };
            if hits.len() == limit || (!hits.is_empty() && message.body.len() > remaining) {
                return Ok((hits, true));
            }
            let original_body_bytes = message.body.len();
            let mut body_offset_bytes = 0;
            if original_body_bytes > remaining {
                // This is the first result. Keep a match-centered piece of
                // the oversized body, aligned to original UTF-8 boundaries.
                let context = (remaining - matched.len()) / 2;
                let mut start = matched
                    .start
                    .saturating_sub(context)
                    .min(original_body_bytes.saturating_sub(remaining));
                while !message.body.is_char_boundary(start) {
                    start += 1;
                }
                let mut end = (start + remaining).min(original_body_bytes);
                while !message.body.is_char_boundary(end) {
                    end -= 1;
                }
                body_offset_bytes = start;
                message.body = message.body[start..end].to_owned();
            }
            remaining -= message.body.len();
            hits.push(Hit {
                message,
                original_body_bytes,
                body_offset_bytes,
                matched,
            });
        }
        if count < SCAN_BATCH as usize {
            return Ok((hits, false));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn literal_matching_preserves_unicode_offsets_and_whitespace() {
        let m = Matcher::new("Σ", false).unwrap();
        for body in ["xσ", "xς", "xΣ"] {
            assert_eq!(m.find(body), Some(1..3));
        }
        let m = Matcher::new("[a.*]_%\n\\", false).unwrap();
        assert_eq!(m.find("x[a.*]_%\n\\y"), Some(1..10));
        assert!(m.find("anything").is_none());
        assert_eq!(Matcher::new(" ", false).unwrap().find("x y"), Some(1..2));
        assert!(Matcher::new("cafe", false).unwrap().find("café").is_none());
        assert!(Matcher::new("É", true).unwrap().find("é").is_none());
        assert!(Matcher::new("x", false).unwrap().find("").is_none());
    }
}
