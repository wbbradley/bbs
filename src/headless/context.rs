//! Provider-reported context occupancy, never cumulative session consumption.
use crate::proto::{ContextUsage, context_usage::Source};
use serde_json::Value;

pub fn codex(params: &Value) -> Option<ContextUsage> {
    let usage = &params["tokenUsage"];
    let used_tokens = usage["last"]["totalTokens"].as_u64();
    let limit_tokens = usage["modelContextWindow"].as_u64().filter(|n| *n > 0);
    (used_tokens.is_some() || limit_tokens.is_some()).then_some(ContextUsage {
        used_tokens,
        limit_tokens,
        source: Source::LastModelRequest.into(),
    })
}
pub fn acp(update: &Value) -> Option<ContextUsage> {
    let used_tokens = update["used"].as_u64();
    let limit_tokens = update["size"].as_u64().filter(|n| *n > 0);
    (used_tokens.is_some() || limit_tokens.is_some()).then_some(ContextUsage {
        used_tokens,
        limit_tokens,
        source: Source::CurrentContext.into(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn uses_latest_context_not_cumulative_or_cache_double_counting() {
        let c=codex(&json!({"tokenUsage":{"last":{"totalTokens":80,"inputTokens":60,"cachedInputTokens":50},"total":{"totalTokens":9000},"modelContextWindow":100}})).unwrap();
        assert_eq!(c.used_tokens, Some(80));
        assert_eq!(c.limit_tokens, Some(100));
        assert!(codex(&json!({"tokenUsage":{"total":{"totalTokens":9000}}})).is_none());
        assert_eq!(
            acp(&json!({"used":150,"size":100})).unwrap().used_tokens,
            Some(150)
        );
        assert_eq!(acp(&json!({"used":0,"size":0})).unwrap().limit_tokens, None);
        assert!(acp(&json!({"used":-1,"size":-1})).is_none());
    }
}
