//! Shared blocker formatting and CLI dispatch. The server owns permissions,
//! immutable lifecycle transitions and idempotent writes.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::backend::Backend;
use crate::cli::BlockerCommand;
use crate::proto;

pub fn row(blocker: &proto::Blocker) -> Value {
    let identity = |p: &proto::Principal| {
        json!({
            "id": p.id, "kind": if p.kind() == proto::Kind::Human { "human" } else { "agent" },
            "name": p.name, "owner": p.owner,
        })
    };
    let status = match blocker.status() {
        proto::BlockerStatus::BlockerAnswered => "answered",
        proto::BlockerStatus::BlockerCancelled => "cancelled",
        proto::BlockerStatus::BlockerSuperseded => "superseded",
        proto::BlockerStatus::BlockerOpen => "open",
        _ => "unknown",
    };
    json!({
        "id": blocker.id, "root_id": blocker.root_id,
        "group": blocker.group, "group_id": blocker.group_id,
        "creator": blocker.creator.as_ref().map(identity),
        "body": blocker.body, "created_at": blocker.created_at,
        "replaces": blocker.replaces, "replaced_by": blocker.replaced_by,
        "status": status, "can_answer": blocker.can_answer, "can_cancel": blocker.can_cancel,
        "outcome": blocker.outcome.as_ref().map(|o| json!({
            "actor": o.actor.as_ref().map(identity), "created_at": o.created_at,
            "answer": o.answer, "message_id": o.message_id,
        })),
    })
}

pub fn list(response: &proto::ListBlockersResponse) -> Value {
    json!({
        "blockers": response.blockers.iter().map(row).collect::<Vec<_>>(),
        "active_count": response.active_count, "has_more": response.has_more,
        "next_after_root_id": response.next_after_root_id,
    })
}

pub fn get(response: &proto::GetBlockerResponse) -> Value {
    json!({
        "blocker": response.blocker.as_ref().map(row),
        "current": response.current.as_ref().map(row),
        "history": response.history.iter().map(row).collect::<Vec<_>>(),
        "has_more": response.has_more, "next_before_id": response.next_before_id,
    })
}

pub fn mutation(response: &proto::BlockerMutationResponse, key: &str) -> Value {
    json!({ "blocker": response.blocker.as_ref().map(row), "message_id": response.message_id,
        "replayed": response.replayed, "request_key": key })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    group: Option<String>,
    agent: Option<String>,
    #[serde(default)]
    include_closed: bool,
    limit: Option<u32>,
    after_root_id: Option<i64>,
}

pub fn list_request(args: &Value) -> Result<proto::ListBlockersRequest> {
    let args: ListArgs =
        serde_json::from_value(args.clone()).context("invalid blocker list arguments")?;
    Ok(proto::ListBlockersRequest {
        group: args.group,
        agent: args.agent,
        include_closed: args.include_closed,
        limit: args.limit,
        after_root_id: args.after_root_id,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetArgs {
    id: i64,
    limit: Option<u32>,
    before_id: Option<i64>,
}

pub fn get_request(args: &Value) -> Result<proto::GetBlockerRequest> {
    let args: GetArgs =
        serde_json::from_value(args.clone()).context("invalid blocker get arguments")?;
    Ok(proto::GetBlockerRequest {
        id: args.id,
        limit: args.limit,
        before_id: args.before_id,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutArgs {
    group: String,
    body: String,
    replaces: Option<i64>,
    request_key: String,
}

pub fn put_request(args: &Value) -> Result<proto::PutBlockerRequest> {
    let args: PutArgs =
        serde_json::from_value(args.clone()).context("invalid blocker put arguments")?;
    Ok(proto::PutBlockerRequest {
        group: args.group,
        body: args.body,
        replaces: args.replaces,
        request_key: args.request_key,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelArgs {
    id: i64,
    request_key: String,
}

pub fn cancel_request(args: &Value) -> Result<proto::CancelBlockerRequest> {
    let args: CancelArgs =
        serde_json::from_value(args.clone()).context("invalid blocker cancel arguments")?;
    Ok(proto::CancelBlockerRequest {
        id: args.id,
        request_key: args.request_key,
    })
}

fn request_key(key: Option<String>) -> String {
    key.unwrap_or_else(|| data_encoding::HEXLOWER.encode(&rand::random::<[u8; 16]>()))
}

fn human_row(value: &Value) -> String {
    format!(
        "#{} @{} [{}] {}\n{}\n",
        value["id"],
        value["group"].as_str().unwrap_or_default(),
        value["status"].as_str().unwrap_or_default(),
        value["creator"]["name"].as_str().unwrap_or_default(),
        value["body"].as_str().unwrap_or_default()
    )
}

pub async fn run(
    backend: &mut Backend,
    command: BlockerCommand,
    json_output: bool,
) -> Result<String> {
    let value = match command {
        BlockerCommand::List {
            group,
            agent,
            include_closed,
            limit,
            after_root_id,
        } => {
            let response = backend
                .list_blockers(proto::ListBlockersRequest {
                    group,
                    agent,
                    include_closed,
                    limit,
                    after_root_id,
                })
                .await?;
            list(&response)
        }
        BlockerCommand::Get {
            id,
            limit,
            before_id,
        } => get(&backend
            .get_blocker(proto::GetBlockerRequest {
                id,
                limit,
                before_id,
            })
            .await?),
        BlockerCommand::Put {
            group,
            replaces,
            request_key: key,
            body,
        } => {
            let key = request_key(key);
            let response = backend.put_blocker(proto::PutBlockerRequest { group, body: crate::app::read_body(body)?, replaces, request_key: key.clone() }).await
                .with_context(|| format!("blocker request key {key}; reuse --request-key with the same payload if retrying"))?;
            mutation(&response, &key)
        }
        BlockerCommand::Answer {
            id,
            request_key: key,
            answer,
        } => {
            let key = request_key(key);
            let response = backend.answer_blocker(proto::AnswerBlockerRequest { id, answer: crate::app::read_body(answer)?, request_key: key.clone() }).await
                .with_context(|| format!("blocker request key {key}; reuse --request-key with the same payload if retrying"))?;
            mutation(&response, &key)
        }
        BlockerCommand::Cancel {
            id,
            request_key: key,
        } => {
            let key = request_key(key);
            let response = backend.cancel_blocker(proto::CancelBlockerRequest { id, request_key: key.clone() }).await
                .with_context(|| format!("blocker request key {key}; reuse --request-key with the same payload if retrying"))?;
            mutation(&response, &key)
        }
    };
    if json_output {
        return Ok(format!("{}\n", serde_json::to_string(&value)?));
    }
    if let Some(rows) = value["blockers"].as_array() {
        let mut text = format!("{} active blockers\n", value["active_count"]);
        for row in rows {
            text.push_str(&human_row(row));
        }
        if value["has_more"] == true {
            text.push_str(&format!(
                "More: --after-root-id {}\n",
                value["next_after_root_id"]
            ));
        }
        Ok(text)
    } else {
        let mut text = human_row(&value["blocker"]);
        if !value["blocker"]["outcome"].is_null() {
            text.push_str(&format!("Outcome: {}\n", value["blocker"]["outcome"]));
        }
        if let Some(history) = value["history"].as_array() {
            text.push_str("History (newest first):\n");
            for version in history {
                text.push_str(&human_row(version));
            }
            if value["has_more"] == true {
                text.push_str(&format!(
                    "More history: --before-id {}\n",
                    value["next_before_id"]
                ));
            }
        }
        if let Some(key) = value["request_key"].as_str() {
            text.push_str(&format!("Request key: {key}\n"));
        }
        Ok(text)
    }
}
