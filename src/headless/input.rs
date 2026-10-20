//! Native human-input requests. Only the original live provider RPC receives an answer.
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::harness::Event;
use super::rpc::Rpc;
use crate::proto::{AnswerAction, InputAnswer, InputOption, InputQuestion};

const MAX_FORM: usize = 16 * 1024;

#[derive(Debug, Clone)]
pub struct Form {
    pub key: String,
    pub questions: Vec<InputQuestion>,
    pub message: Option<String>,
    pub blocking: bool,
}

#[derive(Debug)]
pub enum Encoding {
    Codex,
    Acp,
}

pub struct Parsed {
    pub form: Form,
    encoding: Encoding,
}

struct Pending {
    rpc: Rpc,
    id: Value,
    parsed: Parsed,
}

#[derive(Default)]
pub struct Requests {
    pending: Mutex<Option<Pending>>,
    available: AtomicBool,
}

impl Requests {
    pub fn pending(&self, key: &str) -> bool {
        self.pending
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|p| p.parsed.form.key == key)
    }

    pub fn enable(&self, enabled: bool) {
        self.available.store(enabled, Ordering::SeqCst);
    }

    pub fn open(
        &self,
        rpc: &Rpc,
        id: Value,
        parsed: Parsed,
        events: &mpsc::UnboundedSender<Event>,
    ) -> Result<()> {
        ensure!(
            self.available.load(Ordering::SeqCst),
            "structured human input is unavailable on this server"
        );
        let mut pending = self.pending.lock().unwrap();
        ensure!(pending.is_none(), "another human-input request is pending");
        let form = parsed.form.clone();
        *pending = Some(Pending {
            rpc: rpc.clone(),
            id,
            parsed,
        });
        if events.send(Event::OwnerInput(form)).is_err() {
            drop(pending);
            self.cancel(events);
        }
        Ok(())
    }

    /// Take before writing, so cancellation or a repeated board snapshot cannot answer twice.
    pub async fn answer(
        &self,
        key: &str,
        action: AnswerAction,
        answers: &[InputAnswer],
        events: &mpsc::UnboundedSender<Event>,
    ) -> Result<()> {
        let pending = {
            let mut slot = self.pending.lock().unwrap();
            if slot.as_ref().is_none_or(|p| p.parsed.form.key != key) {
                return Ok(());
            }
            slot.take().unwrap()
        };
        let reply = pending.parsed.encode(action, answers);
        let _ = events.send(Event::InputClosed(key.to_string()));
        match reply {
            Ok(reply) => pending.rpc.respond(pending.id, Ok(reply)).await,
            Err(error) => {
                let cancel = pending.parsed.encode(AnswerAction::Cancel, &[]).unwrap();
                pending.rpc.respond(pending.id, Ok(cancel)).await?;
                Err(error)
            }
        }
    }

    pub fn withdraw(&self, id: &Value, events: &mpsc::UnboundedSender<Event>) {
        let p = {
            let mut slot = self.pending.lock().unwrap();
            if slot.as_ref().is_some_and(|p| &p.id == id) {
                slot.take()
            } else {
                None
            }
        };
        if let Some(p) = p {
            let _ = events.send(Event::InputClosed(p.parsed.form.key));
        }
    }

    pub fn cancel(&self, events: &mpsc::UnboundedSender<Event>) {
        let pending = self.pending.lock().unwrap().take();
        if let Some(p) = pending {
            p.rpc.respond_later(
                p.id,
                Ok(p.parsed.encode(AnswerAction::Cancel, &[]).unwrap()),
            );
            let _ = events.send(Event::InputClosed(p.parsed.form.key));
        }
    }
}

impl Parsed {
    fn encode(&self, action: AnswerAction, answers: &[InputAnswer]) -> Result<Value> {
        ensure!(
            matches!(
                action,
                AnswerAction::Submit | AnswerAction::Decline | AnswerAction::Cancel
            ),
            "invalid input action"
        );
        if action != AnswerAction::Submit {
            ensure!(answers.is_empty(), "cancelled input cannot have answers");
            return Ok(match self.encoding {
                Encoding::Codex => json!({"answers": {}}),
                Encoding::Acp => {
                    json!({"action": if action == AnswerAction::Decline { "decline" } else { "cancel" }})
                }
            });
        }
        let mut seen = HashSet::new();
        let mut output = serde_json::Map::new();
        for answer in answers {
            ensure!(seen.insert(&answer.question_id), "duplicate answer");
            let q = self
                .form
                .questions
                .iter()
                .find(|q| q.id == answer.question_id)
                .ok_or_else(|| anyhow::anyhow!("unknown question"))?;
            ensure!(answer.value.len() <= 4096, "answer too long");
            let selected = q.options.iter().any(|o| o.value == answer.value);
            ensure!(
                selected || q.allow_free_text,
                "answer is not a permitted option"
            );
            match &self.encoding {
                Encoding::Codex => {
                    output.insert(q.id.clone(), json!({"answers": [answer.value]}));
                }
                Encoding::Acp => {
                    output.insert(q.id.clone(), answer.value.clone().into());
                }
            }
        }
        ensure!(
            self.form
                .questions
                .iter()
                .all(|q| !q.required || seen.contains(&q.id)),
            "required answer missing"
        );
        Ok(match self.encoding {
            Encoding::Codex => json!({"answers": output}),
            Encoding::Acp => json!({"action": "accept", "content": output}),
        })
    }
}

fn finish(
    questions: Vec<InputQuestion>,
    message: Option<String>,
    encoding: Encoding,
) -> Result<Parsed> {
    ensure!(
        (1..=3).contains(&questions.len()),
        "human input supports 1–3 questions"
    );
    ensure!(
        message.as_ref().is_none_or(|s| s.len() <= 4096),
        "form message exceeds 4 KiB"
    );
    let mut ids = HashSet::new();
    for q in &questions {
        ensure!(
            !q.id.is_empty() && q.id.len() <= 128 && ids.insert(&q.id),
            "invalid or duplicate question id"
        );
        ensure!(
            q.header.len() <= 128 && !q.text.is_empty() && q.text.len() <= 4096,
            "question text exceeds bounds"
        );
        ensure!(
            q.options.len() <= 8,
            "human input supports at most eight options per question"
        );
        let mut values = HashSet::new();
        for o in &q.options {
            ensure!(
                o.label.as_ref().is_none_or(|s| s.len() <= 256)
                    && o.description.as_ref().is_none_or(|s| s.len() <= 2048),
                "option metadata exceeds bounds"
            );
            ensure!(
                !o.value.is_empty() && o.value.len() <= 128 && values.insert(&o.value),
                "invalid or duplicate option"
            );
        }
    }
    Ok(Parsed {
        form: Form {
            key: crate::auth::new_token(),
            questions,
            message,
            blocking: true,
        },
        encoding,
    })
}

fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing string {field}"))
}

pub fn codex(params: &Value) -> Result<Parsed> {
    ensure!(
        params.to_string().len() <= MAX_FORM,
        "human input exceeds 16 KiB"
    );
    let questions = params["questions"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing questions"))?;
    let mut parsed = Vec::new();
    for q in questions {
        ensure!(
            q.get("isSecret").is_none_or(|v| v == false),
            "secret input is unsupported"
        );
        let options = match &q["options"] {
            Value::Null => Vec::new(),
            Value::Array(items) => items
                .iter()
                .map(|o| {
                    Ok(InputOption {
                        value: string(o, "label")?.into(),
                        label: Some(string(o, "label")?.into()),
                        description: Some(string(o, "description")?.into()),
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            _ => bail!("invalid options"),
        };
        let allow_free_text = options.is_empty() || q["isOther"].as_bool().unwrap_or(false);
        parsed.push(InputQuestion {
            id: string(q, "id")?.into(),
            header: string(q, "header")?.into(),
            text: string(q, "question")?.into(),
            options,
            allow_free_text,
            required: false,
        });
    }
    let mut parsed = finish(parsed, None, Encoding::Codex)?;
    parsed.form.blocking = params["isBlocking"]
        .as_bool()
        .ok_or_else(|| anyhow::anyhow!("missing boolean isBlocking"))?;
    Ok(parsed)
}

/// The supported ACP subset is plain strings and string enums. No coercion of richer forms.
pub fn acp(params: &Value) -> Result<Parsed> {
    ensure!(
        params.to_string().len() <= MAX_FORM,
        "human input exceeds 16 KiB"
    );
    ensure!(
        params["mode"] == "form",
        "only ACP form elicitation is supported"
    );
    let schema = &params["requestedSchema"];
    ensure!(schema["type"] == "object", "input schema must be an object");
    ensure!(
        schema
            .as_object()
            .is_some_and(|o| o.keys().all(|k| matches!(
                k.as_str(),
                "type" | "properties" | "required" | "title" | "description" | "$schema"
            ))),
        "unsupported top-level form constraint"
    );
    ensure!(
        params.get("message").is_none_or(|v| v.is_string()),
        "invalid form message"
    );
    let properties = schema["properties"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("missing form properties"))?;
    let required: HashSet<&str> = match &schema["required"] {
        Value::Null => HashSet::new(),
        Value::Array(v) => v
            .iter()
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid required field"))
            })
            .collect::<Result<_>>()?,
        _ => bail!("invalid required fields"),
    };
    ensure!(
        required.iter().all(|k| properties.contains_key(*k)),
        "unknown required field"
    );
    for (key, p) in properties {
        ensure!(p["type"] == "string", "unsupported form type for {key}");
        let p = p.as_object().unwrap();
        ensure!(
            p.keys().all(|k| matches!(
                k.as_str(),
                "type" | "title" | "description" | "enum" | "oneOf" | "default" | "_meta"
            )),
            "unsupported form constraint for {key}"
        );
    }
    let mut questions = Vec::new();
    for (key, p) in properties {
        let options = if let Some(items) = p.get("oneOf") {
            ensure!(p.get("enum").is_none(), "ambiguous input options");
            items
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("invalid oneOf"))?
                .iter()
                .map(|o| {
                    ensure!(
                        o.as_object().is_some_and(|o| o.keys().all(|k| matches!(
                            k.as_str(),
                            "const" | "title" | "description" | "_meta"
                        ))),
                        "unsupported choice schema"
                    );
                    Ok(InputOption {
                        value: string(o, "const")?.into(),
                        label: o["title"].as_str().map(str::to_string),
                        description: o["description"].as_str().map(str::to_string),
                    })
                })
                .collect::<Result<Vec<_>>>()?
        } else if let Some(items) = p.get("enum") {
            items
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("invalid enum"))?
                .iter()
                .map(|o| {
                    Ok(InputOption {
                        value: o
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("non-string enum"))?
                            .into(),
                        label: None,
                        description: None,
                    })
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        ensure!(
            !(p.get("oneOf").is_some() || p.get("enum").is_some()) || !options.is_empty(),
            "empty choice set is unsupported"
        );
        let text = p["description"]
            .as_str()
            .or(p["title"].as_str())
            .unwrap_or(key);
        questions.push(InputQuestion {
            id: key.clone(),
            header: p["title"].as_str().unwrap_or_default().into(),
            text: text.into(),
            allow_free_text: options.is_empty(),
            options,
            required: required.contains(key.as_str()),
        });
    }
    let mut parsed = finish(
        questions,
        params["message"].as_str().map(str::to_string),
        Encoding::Acp,
    )?;
    parsed.form.blocking = true;
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codex_preserves_options_free_text_skip_and_rejects_secrets() {
        let mut p = json!({"isBlocking":false,"questions":[{"id":"q","header":"Choose","question":"Which?","options":[{"label":"A","description":"first"}],"isOther":true}]});
        let parsed = codex(&p).unwrap();
        assert!(parsed.form.message.is_none());
        assert!(!parsed.form.blocking);
        assert_eq!(
            parsed.form.questions[0].options[0].description.as_deref(),
            Some("first")
        );
        assert_eq!(
            parsed
                .encode(
                    AnswerAction::Submit,
                    &[InputAnswer {
                        question_id: "q".into(),
                        value: "other text".into()
                    }]
                )
                .unwrap(),
            json!({"answers":{"q":{"answers":["other text"]}}})
        );
        assert_eq!(
            parsed.encode(AnswerAction::Submit, &[]).unwrap(),
            json!({"answers":{}})
        );
        assert_eq!(
            parsed.encode(AnswerAction::Cancel, &[]).unwrap(),
            json!({"answers":{}})
        );
        let mut invalid = p.clone();
        invalid.as_object_mut().unwrap().remove("isBlocking");
        assert!(codex(&invalid).is_err());
        invalid["isBlocking"] = "false".into();
        assert!(codex(&invalid).is_err());
        p["questions"][0]["isSecret"] = true.into();
        assert!(codex(&p).is_err());
    }
    #[test]
    fn acp_flat_fields_preserve_optional_custom_and_skip() {
        let p = json!({"mode":"form","message":"Which deployment strategy?","requestedSchema":{"type":"object","properties":{"question_0":{"type":"string","title":"Choice","oneOf":[{"const":"a","title":"A","description":"first"}]},"question_0_custom":{"type":"string","title":"Other"}}}});
        let parsed = acp(&p).unwrap();
        assert_eq!(parsed.form.questions.len(), 2);
        assert_eq!(
            parsed.form.message.as_deref(),
            Some("Which deployment strategy?")
        );
        assert_eq!(
            parsed
                .encode(
                    AnswerAction::Submit,
                    &[InputAnswer {
                        question_id: "question_0_custom".into(),
                        value: "custom\ntext".into()
                    }]
                )
                .unwrap(),
            json!({"action":"accept","content":{"question_0_custom":"custom\ntext"}})
        );
        assert_eq!(
            parsed.encode(AnswerAction::Submit, &[]).unwrap(),
            json!({"action":"accept","content":{}})
        );
        assert_eq!(
            parsed.encode(AnswerAction::Decline, &[]).unwrap(),
            json!({"action":"decline"})
        );
    }
    #[test]
    fn acp_richer_or_secret_forms_are_explicitly_unsupported() {
        for field in [
            json!({"type":"array","items":{"type":"string"}}),
            json!({"type":"number"}),
            json!({"type":"string","format":"password"}),
            json!({"type":"string","pattern":".*"}),
            json!({"type":"string","enum":[]}),
            json!({"type":"string","oneOf":[]}),
        ] {
            assert!(acp(&json!({"mode":"form","requestedSchema":{"type":"object","properties":{"x":field}}})).is_err());
        }
        assert!(acp(&json!({"mode":"form","requestedSchema":{"type":"object","properties":{"x":{"type":"string"}},"allOf":[]}})).is_err());
        assert!(acp(&json!({"mode":"url","url":"https://example.com"})).is_err());
    }
}
