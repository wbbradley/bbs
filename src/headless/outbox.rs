//! Durable, registration-bound owner replies. Each part is committed before it
//! is offered to the network; an uncertain ACK retries the same key and bytes.
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::state::write_private;

pub const PART_BYTES: usize = 64 * 1024;
/// Stop the producer at this high-water mark, retaining already spoken text.
/// Shutdown may append the bounded tail already in the protocol pipe.
pub const HIGH_WATER: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Source {
    pub session: String,
    pub turn: Option<String>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Part {
    pub key: String,
    pub source: Source,
    pub body: String,
    pub sealed: bool,
    sequence: u64,
}

#[derive(Deserialize, Serialize)]
struct Identity {
    board: String,
    agent: i64,
    next: u64,
}

pub struct Outbox {
    dir: PathBuf,
    identity: Identity,
    parts: VecDeque<Part>,
}

impl Outbox {
    pub fn open(dir: &Path, board: String, agent: i64) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = dir.join("identity.json");
        let identity = if path.exists() {
            let identity: Identity = serde_json::from_slice(&std::fs::read(&path)?)
                .context("invalid owner outbox identity; preserve it for inspection")?;
            ensure!(
                identity.board == board && identity.agent == agent,
                "owner outbox belongs to another board or agent registration; preserve it and resolve its delivery before starting this identity"
            );
            identity
        } else {
            let identity = Identity {
                board,
                agent,
                next: 0,
            };
            write_private(&path, &serde_json::to_vec(&identity)?)?;
            identity
        };
        let mut parts = Vec::new();
        for file in std::fs::read_dir(dir)? {
            let file = file?;
            let name = file.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("part-") || !name.ends_with(".json") {
                continue;
            }
            let mut part: Part = serde_json::from_slice(&std::fs::read(file.path())?)
                .context("invalid owner outbox part; file preserved")?;
            if !part.sealed {
                part.body = match std::fs::read_to_string(file.path().with_extension("text")) {
                    Ok(text) => text,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => return Err(e.into()),
                };
                if part.body.is_empty() {
                    std::fs::remove_file(file.path())?;
                    continue;
                }
            }
            ensure!(
                !part.body.is_empty() && part.body.len() <= PART_BYTES,
                "invalid owner outbox part size; file preserved"
            );
            ensure!(
                part.sequence < identity.next,
                "invalid owner outbox sequence"
            );
            parts.push(part);
        }
        parts.sort_by_key(|p| p.sequence);
        let mut out = Self {
            dir: dir.into(),
            identity,
            parts: parts.into(),
        };
        // A previous process may have stopped mid-message. Deliver its exact
        // durable prefix rather than joining it to newly generated text.
        out.seal()?;
        Ok(out)
    }

    fn path(&self, sequence: u64) -> PathBuf {
        self.dir.join(format!("part-{sequence:020}.json"))
    }

    fn persist(&self, part: &Part) -> Result<()> {
        write_private(&self.path(part.sequence), &serde_json::to_vec(part)?)
            .context("persisting owner reply; stopping rather than silently losing text")?;
        if part.sealed {
            let path = self.path(part.sequence).with_extension("text");
            if path.exists() {
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    fn append_bytes(&self, part: &Part, text: &str) -> Result<()> {
        let path = self.path(part.sequence);
        if !path.exists() {
            let mut header = part.clone();
            header.body.clear();
            header.sealed = false;
            self.persist(&header)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path.with_extension("text"))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    pub fn append(&mut self, source: Source, mut text: &str) -> Result<()> {
        if self
            .parts
            .back()
            .is_some_and(|p| !p.sealed && p.source != source)
        {
            self.seal()?;
        }
        while !text.is_empty() {
            if self.parts.back().is_none_or(|p| p.sealed) {
                let sequence = self.identity.next;
                self.identity.next += 1;
                write_private(
                    &self.dir.join("identity.json"),
                    &serde_json::to_vec(&self.identity)?,
                )?;
                self.parts.push_back(Part {
                    key: crate::auth::new_token(),
                    source: source.clone(),
                    body: String::new(),
                    sealed: false,
                    sequence,
                });
            }
            let part = self.parts.back_mut().unwrap();
            let capacity = PART_BYTES - part.body.len();
            let mut take = text.len().min(capacity);
            while !text.is_char_boundary(take) {
                take -= 1;
            }
            // Prefer a complete paragraph only when a split is necessary.
            if take < text.len()
                && let Some(end) = text[..take].rfind("\n\n")
                && end > take / 2
            {
                take = end + 2;
            }
            if take == 0 {
                self.seal()?;
                continue;
            }
            part.body.push_str(&text[..take]);
            self.append_bytes(self.parts.back().unwrap(), &text[..take])?;
            text = &text[take..];
            if !text.is_empty() || self.parts.back().unwrap().body.len() == PART_BYTES {
                self.seal()?;
            }
        }
        Ok(())
    }

    pub fn seal(&mut self) -> Result<()> {
        if let Some(part) = self.parts.back_mut()
            && !part.sealed
        {
            part.sealed = true;
            self.persist(self.parts.back().unwrap())?;
        }
        Ok(())
    }

    pub fn ready(&self) -> Option<&Part> {
        self.parts.front().filter(|p| p.sealed)
    }
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
    pub fn bytes(&self) -> usize {
        self.parts.iter().map(|p| p.body.len()).sum()
    }

    pub fn acknowledge(&mut self, key: &str) -> Result<Part> {
        let part = self
            .parts
            .front()
            .context("owner ACK without a pending reply")?;
        ensure!(
            part.key == key && part.sealed,
            "out-of-order owner reply ACK"
        );
        let open_text = self.path(part.sequence).with_extension("text");
        if open_text.exists() {
            std::fs::remove_file(open_text)?;
        }
        std::fs::remove_file(self.path(part.sequence))?;
        std::fs::File::open(&self.dir)?.sync_all()?;
        Ok(self.parts.pop_front().unwrap())
    }
}

/// One network writer. Requests never overlap, including retries after a lost
/// response. The runner keeps the disk part until it receives this ACK.
pub struct Sender {
    pub queue: tokio::sync::mpsc::Sender<Part>,
    pub replies: tokio::sync::mpsc::Receiver<Result<(String, i64)>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Sender {
    pub fn start(
        config: crate::config::Config,
        dir: PathBuf,
        token: String,
        warnings: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Self {
        let (queue, mut incoming) = tokio::sync::mpsc::channel::<Part>(1);
        let (results, replies) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(async move {
            let caller = crate::service::Caller {
                token: Some(token),
                ..Default::default()
            };
            let mut backend = None;
            while let Some(part) = incoming.recv().await {
                let mut delay = 1;
                let mut warned = false;
                loop {
                    let attempt = async {
                        if backend.is_none() {
                            backend = Some(
                                crate::backend::Backend::open(&config, &dir, caller.clone())
                                    .await?,
                            );
                        }
                        let local = matches!(backend, Some(crate::backend::Backend::Local(_)));
                        let result = backend
                            .as_mut()
                            .unwrap()
                            .auto_reply(crate::proto::AutoReplyRequest {
                                key: part.key.clone(),
                                body: part.body.clone(),
                            })
                            .await;
                        result.map_err(|err| {
                            if local && !local_busy(&err) {
                                anyhow::Error::new(LocalRefusal(err))
                            } else {
                                err
                            }
                        })
                    };
                    let result =
                        match tokio::time::timeout(std::time::Duration::from_secs(10), attempt)
                            .await
                        {
                            Ok(result) => result,
                            Err(_) => Err(anyhow::anyhow!("owner reply acknowledgment timed out")),
                        };
                    match result {
                        Ok(reply) => {
                            if results
                                .send(Ok((part.key.clone(), reply.id)))
                                .await
                                .is_err()
                            {
                                return;
                            }
                            break;
                        }
                        Err(err) => {
                            backend = None;
                            if permanent(&err) {
                                let _ = results
                                    .send(Err(
                                        err.context("owner delivery refused; outbox retained")
                                    ))
                                    .await;
                                return;
                            }
                            if !warned {
                                let _ = warnings.send(format!(
                                    "owner delivery delayed; durable reply will retry: {err:#}"
                                ));
                                warned = true;
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                            delay = (delay * 2).min(10);
                        }
                    }
                }
            }
        });
        Self {
            queue,
            replies,
            task,
        }
    }
}

fn permanent(err: &anyhow::Error) -> bool {
    use tonic::Code;
    err.downcast_ref::<LocalRefusal>().is_some()
        || err
            .downcast_ref::<crate::auth::AuthenticationError>()
            .is_some()
        || err.downcast_ref::<tonic::Status>().is_some_and(|status| {
            matches!(
                status.code(),
                Code::InvalidArgument
                    | Code::PermissionDenied
                    | Code::Unauthenticated
                    | Code::FailedPrecondition
                    | Code::Unimplemented
                    | Code::NotFound
            )
        })
}

/// Local service refusals are not transport outages. Keep their original cause
/// but distinguish them from retryable SQLite lock contention without parsing text.
#[derive(Debug)]
struct LocalRefusal(anyhow::Error);
impl std::fmt::Display for LocalRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for LocalRefusal {}
fn local_busy(err: &anyhow::Error) -> bool {
    matches!(err.downcast_ref::<rusqlite::Error>(), Some(rusqlite::Error::SqliteFailure(code, _))
        if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn lost_ack_retries_same_part_once_and_local_refusal_stops() {
        use crate::{backend::Backend, config::Config, proto, service::Caller};
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        let mut db = crate::db::open(&dir.path().join("bbs.db")).unwrap();
        crate::db::migrate(&mut db).unwrap();
        drop(db);
        let mut human = Backend::open(
            &config,
            dir.path(),
            Caller {
                human: Some("will".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        human
            .ensure_human(proto::EnsureHumanRequest {
                name: "will".into(),
            })
            .await
            .unwrap();
        let agent = human
            .register(proto::RegisterRequest {
                name: Some("Agent".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let mut client = Backend::open(
            &config,
            dir.path(),
            Caller {
                token: Some(agent.token.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let identity = client.whoami().await.unwrap();
        let mut out = Outbox::open(&dir.path().join("out"), "b".into(), identity.id).unwrap();
        out.append(Source::default(), " exact\n").unwrap();
        out.seal().unwrap();
        let part = out.ready().unwrap().clone();
        let accepted = client
            .auto_reply(proto::AutoReplyRequest {
                key: part.key.clone(),
                body: part.body.clone(),
            })
            .await
            .unwrap();
        // The server accepted it but the runner crashed before acknowledging.
        drop(out);
        let mut out = Outbox::open(&dir.path().join("out"), "b".into(), identity.id).unwrap();
        let (warnings, mut warning_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut sender = Sender::start(config, dir.path().into(), agent.token, warnings);
        sender
            .queue
            .send(out.ready().unwrap().clone())
            .await
            .unwrap();
        let (key, id) =
            tokio::time::timeout(std::time::Duration::from_secs(3), sender.replies.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        assert_eq!(id, accepted.id);
        out.acknowledge(&key).unwrap();
        let read = human.read(proto::ReadRequest::default()).await.unwrap();
        assert_eq!(
            read.messages
                .iter()
                .filter(|m| m.body == " exact\n")
                .count(),
            1
        );
        // A conflicting reuse is a definite local service refusal, not an outage.
        let mut conflict = part;
        conflict.body = "different".into();
        sender.queue.send(conflict).await.unwrap();
        let err = tokio::time::timeout(std::time::Duration::from_secs(3), sender.replies.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(err.downcast_ref::<LocalRefusal>().is_some());
        assert!(warning_rx.try_recv().is_err());
    }

    #[test]
    fn crash_prefix_and_lost_ack_keep_exact_identity_and_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let source = Source {
            session: "parent".into(),
            turn: Some("t".into()),
            message: Some("m".into()),
        };
        let mut out = Outbox::open(dir.path(), "board".into(), 3).unwrap();
        out.append(source.clone(), " a\n\n**é**\n").unwrap();
        assert!(out.ready().is_none());
        drop(out);
        let mut out = Outbox::open(dir.path(), "board".into(), 3).unwrap();
        let part = out.ready().unwrap().clone();
        assert_eq!(part.source, source);
        assert_eq!(part.body, " a\n\n**é**\n");
        drop(out); // Server may already have accepted this key.
        out = Outbox::open(dir.path(), "board".into(), 3).unwrap();
        assert_eq!(out.ready().unwrap().key, part.key);
        assert!(out.acknowledge("wrong").is_err());
        out.acknowledge(&part.key).unwrap();
        drop(out);
        assert!(
            Outbox::open(dir.path(), "board".into(), 3)
                .unwrap()
                .is_empty()
        );
        assert!(Outbox::open(dir.path(), "other".into(), 3).is_err());
        assert!(Outbox::open(dir.path(), "board".into(), 4).is_err());
    }
    #[test]
    fn splitting_and_boundaries_preserve_all_utf8_and_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = Outbox::open(dir.path(), "b".into(), 1).unwrap();
        let text = format!("{}\n\n{}", "é".repeat(40000), "🦀 ".repeat(20000));
        out.append(Source::default(), &text).unwrap();
        out.seal().unwrap();
        let mut result = String::new();
        while let Some(part) = out.ready().cloned() {
            assert!(part.body.len() <= PART_BYTES);
            result.push_str(&part.body);
            out.acknowledge(&part.key).unwrap();
        }
        assert_eq!(result, text);
        out.append(Source::default(), "one").unwrap();
        out.append(
            Source {
                message: Some("new".into()),
                ..Source::default()
            },
            "two",
        )
        .unwrap();
        assert_eq!(out.ready().unwrap().body, "one");
    }
    #[test]
    fn interrupted_seal_cleanup_does_not_rekey_or_duplicate_text() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = Outbox::open(dir.path(), "b".into(), 1).unwrap();
        out.append(Source::default(), " \n\t").unwrap();
        out.seal().unwrap();
        let key = out.ready().unwrap().key.clone();
        // Crash after the sealed JSON was committed, before open-text unlink.
        std::fs::write(out.path(0).with_extension("text"), " \n\t").unwrap();
        drop(out);
        let mut out = Outbox::open(dir.path(), "b".into(), 1).unwrap();
        assert_eq!(out.parts.len(), 1);
        assert_eq!(out.ready().unwrap().key, key);
        assert_eq!(out.ready().unwrap().body, " \n\t");
        out.acknowledge(&key).unwrap();
        assert!(!out.path(0).with_extension("text").exists());
    }

    #[test]
    fn corrupt_parts_are_preserved_and_stop_startup() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = Outbox::open(dir.path(), "b".into(), 1).unwrap();
        out.append(Source::default(), "keep").unwrap();
        std::fs::write(out.path(0), b"broken").unwrap();
        assert!(Outbox::open(dir.path(), "b".into(), 1).is_err());
        assert_eq!(std::fs::read(out.path(0)).unwrap(), b"broken");
    }
}
