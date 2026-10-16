//! `bbs headless run`: drive one agent's harness from its BBS mail.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::acp::{Acp, Event, McpServer};
use super::log::Log;
use super::mcp::TOKEN_ENV;
use super::settings::{MidTurn, Settings};
use super::state::{AgentDir, State};
use crate::app::format_messages;
use crate::backend::Backend;
use crate::cli::HeadlessRunArgs;
use crate::config::Config;
use crate::proto;
use crate::service::Caller;

/// Wait before retrying after the mail wait fails.
const MAIL_RETRY: Duration = Duration::from_secs(10);
/// "already running" retries: after a token takeover the old token's waiter can
/// hold the inbox briefly. Persisting means another process drives this agent.
const SLOT_RETRIES: u32 = 3;

pub async fn run(dir: &Path, as_human: Option<&str>, args: HeadlessRunArgs) -> Result<()> {
    let settings = Settings::load()?;
    let (harness, mut harness_config) = settings.harness(args.harness.as_deref())?;
    if args.model.is_some() {
        harness_config.model = args.model.clone();
    }
    let explicit_steer = args.mid_turn.or(settings.mid_turn) == Some(MidTurn::Steer);
    let mid_turn = args.mid_turn.or(settings.mid_turn).unwrap_or_default();
    let interval = match &args.min_turn_interval {
        Some(s) => humantime::parse_duration(s)
            .with_context(|| format!("invalid --min-turn-interval {s:?}"))?,
        None => settings.min_turn_interval()?,
    };
    let cwd = match &args.cwd {
        Some(cwd) => cwd
            .canonicalize()
            .with_context(|| format!("no directory {}", cwd.display()))?,
        None => std::env::current_dir()?,
    };
    let instructions = match &args.prompt_file {
        Some(path) => Some(
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
        ),
        None => None,
    };

    let config = Config::load()?;
    let agent_dir = AgentDir::new(&args.agent)?;
    agent_dir.create()?;
    // One driver per agent on this machine; the OS releases the lock on exit.
    let _lock = agent_dir.lock()?;
    let mut state = agent_dir.load()?;
    let (agent, token) = agent_token(
        &config,
        dir,
        as_human,
        &args.agent,
        &harness,
        &cwd,
        &mut state,
    )
    .await?;
    agent_dir.save(&state)?;

    let mut log = Log::open(&agent_dir.log_path())?;
    log.event(
        "start",
        json!({ "agent": agent, "harness": harness, "cwd": cwd, "pid": std::process::id() }),
    );

    let mcp = McpServer {
        name: "bbs".into(),
        command: std::env::current_exe()?.display().to_string(),
        args: vec!["headless".into(), "mcp".into()],
        env: std::iter::once((TOKEN_ENV.to_string(), token.clone()))
            .chain(config.server.clone().map(|s| ("BBS_SERVER".to_string(), s)))
            .collect(),
    };
    let resume = state
        .session_id
        .clone()
        .filter(|_| state.harness.as_deref() == Some(harness.as_str()));
    // Listen before starting the harness: a signal during setup drops the
    // start, which kills the harness's process group.
    let mut signals = Signals::new()?;
    let stderr = agent_dir.stderr_path();
    let started = tokio::select! {
        started = Acp::start(
            &harness_config,
            &cwd,
            &stderr,
            &mcp,
            resume.as_deref(),
        ) => started,
        _ = signals.recv() => Err(anyhow::anyhow!("stopped during harness setup")),
    };
    let (acp, mut process, mut events, warnings) = match started {
        Ok(started) => started,
        Err(err) => {
            log.event("stop", json!({ "error": format!("{err:#}") }));
            return Err(err);
        }
    };
    state.harness = Some(harness.clone());
    state.session_id = Some(acp.session_id().to_string());
    agent_dir.save(&state)?;
    log.event(
        "session",
        json!({ "id": acp.session_id(), "resumed": acp.session_resumed }),
    );
    for warning in warnings {
        eprintln!("bbs: warning: {warning}");
        log.event("warning", json!({ "message": warning }));
    }
    if explicit_steer && !acp.can_steer() {
        let warning = format!(
            "harness {harness:?} cannot steer a running turn; mail that arrives during a turn is queued"
        );
        eprintln!("bbs: warning: {warning}");
        log.event("warning", json!({ "message": warning }));
    }
    eprintln!(
        "bbs: {agent} is running headless with {harness}; log: {}",
        agent_dir.log_path().display()
    );

    let steer = mid_turn == MidTurn::Steer && acp.can_steer();
    let mut preamble = (!acp.session_resumed).then(|| preamble(&agent, instructions.as_deref()));
    let (mail_tx, mut mail_rx) = mpsc::unbounded_channel();
    let mail_task = tokio::spawn(wait_for_mail(
        config.clone(),
        dir.to_path_buf(),
        token,
        mail_tx,
    ));

    let mut queue: Vec<proto::Message> = Vec::new();
    let mut turn: Option<JoinHandle<Result<String>>> = None;
    let mut last_start: Option<Instant> = None;
    let result = loop {
        let ready_at = last_start
            .map(|t| t + interval)
            .unwrap_or_else(Instant::now);
        if turn.is_none() && !queue.is_empty() && Instant::now() >= ready_at {
            let mut prompt = preamble.take().unwrap_or_default();
            prompt.push_str(&format!(
                "New BBS mail ({} message{}):\n\n{}",
                queue.len(),
                if queue.len() == 1 { "" } else { "s" },
                format_messages(&queue, false)?
            ));
            queue.clear();
            log.event("prompt", json!({ "text": prompt }));
            let acp = acp.clone();
            turn = Some(tokio::spawn(async move { acp.prompt(&prompt).await }));
            last_start = Some(Instant::now());
            continue;
        }
        let throttled = turn.is_none() && !queue.is_empty();
        tokio::select! {
            mail = mail_rx.recv() => match mail {
                Some(Ok(mut batch)) => {
                    // Control messages act now and are not prompts. The server
                    // accepts them only from the agent's owner.
                    let cancels = batch.iter().filter(|m| m.control() == proto::Control::Cancel).count();
                    batch.retain(|m| m.control.is_none());
                    if cancels > 0 {
                        let running = turn.is_some();
                        if running && let Err(err) = acp.cancel().await {
                            log.event("error", json!({ "message": format!("cancel: {err:#}") }));
                        }
                        log.event("cancel", json!({ "turn_running": running }));
                    }
                    if batch.is_empty() {
                        continue;
                    }
                    let waiting = turn.is_some() || Instant::now() < ready_at;
                    log.event("mail", json!({
                        "count": batch.len(),
                        "from": batch.iter().map(|m| m.sender_display_name()).collect::<Vec<_>>(),
                        "queued": waiting,
                    }));
                    if steer && turn.is_some() {
                        todo!("steer mail into the running turn (no harness can yet)");
                    }
                    queue.extend(batch);
                }
                Some(Err(err)) => break Err(err),
                None => break Err(anyhow::anyhow!("the mail wait stopped")),
            },
            done = async { turn.as_mut().unwrap().await }, if turn.is_some() => {
                turn = None;
                match done {
                    Ok(Ok(stop)) => log.event("turn_end", json!({ "stop_reason": stop })),
                    Ok(Err(err)) => log.event("turn_error", json!({ "message": format!("{err:#}") })),
                    Err(err) => log.event("turn_error", json!({ "message": err.to_string() })),
                }
            }
            event = events.recv() => match event {
                Some(Event::Exited) | None => break Err(anyhow::anyhow!("the harness exited")),
                Some(event) => record(&mut log, event),
            },
            _ = tokio::time::sleep_until(ready_at), if throttled => {}
            status = process.wait() => break Err(match status {
                Ok(status) => anyhow::anyhow!("the harness exited ({status})"),
                Err(err) => anyhow::anyhow!("waiting for the harness: {err}"),
            }),
            _ = signals.recv() => break Ok(()),
        }
    };
    mail_task.abort();
    // Mail the wait already took but the loop had not yet received.
    while let Ok(Ok(batch)) = mail_rx.try_recv() {
        queue.extend(batch.into_iter().filter(|m| m.control.is_none()));
    }
    if turn.is_some() {
        let _ = acp.cancel().await;
    }
    log.flush();
    if !queue.is_empty() {
        // Already marked read on the server; keep them where the owner can find them.
        log.event(
            "unprocessed",
            json!({ "text": format_messages(&queue, false)? }),
        );
    }
    match &result {
        Ok(()) => log.event("stop", json!({})),
        Err(err) => log.event("stop", json!({ "error": format!("{err:#}") })),
    }
    process.stop().await;
    result
}

fn record(log: &mut Log, event: Event) {
    match event {
        Event::Text(t) => log.chunk("text", &t),
        Event::Thought(t) => log.chunk("thought", &t),
        Event::Tool(id, mut fields) => {
            fields["id"] = id.into();
            log.event("tool", fields);
        }
        Event::Permission(fields) => log.event("permission", fields),
        Event::Other(kind) => log.event("update", json!({ "kind": kind })),
        Event::Exited => {}
    }
}

/// The agent's identity and token: the saved token if it is still valid on this
/// server, else a new registration.
async fn agent_token(
    config: &Config,
    dir: &Path,
    as_human: Option<&str>,
    agent: &str,
    harness: &str,
    cwd: &Path,
    state: &mut State,
) -> Result<(String, String)> {
    if let Some(token) = state
        .token
        .clone()
        .filter(|_| state.server == config.server)
    {
        let caller = Caller {
            token: Some(token.clone()),
            ..Default::default()
        };
        let mut backend = Backend::open(config, dir, caller).await?;
        match backend.whoami().await {
            Ok(me) if me.name.eq_ignore_ascii_case(agent) => return Ok((me.name, token)),
            Ok(_) => {}
            // Only a definite rejection means there is no valid token. Anything
            // else (the server restarting, say) must not lead to a takeover.
            Err(err) if rejected_credential(&err) => {}
            Err(err) => return Err(err.context("checking the saved agent token")),
        }
    }
    let caller = Caller {
        human: config.human(as_human),
        ..Default::default()
    };
    let mut backend = Backend::open(config, dir, caller).await?;
    let req = proto::RegisterRequest {
        name: Some(agent.to_string()),
        note: Some(format!("headless ({harness})")),
        pwd: cwd.display().to_string(),
        hostname: gethostname::gethostname().to_string_lossy().into_owned(),
    };
    match backend.register(req).await {
        Ok(r) => {
            *state = State {
                server: config.server.clone(),
                token: Some(r.token.clone()),
                ..State::default()
            };
            Ok((r.name, r.token))
        }
        Err(err) if format!("{err:#}").contains("already reserved") => {
            // The agent exists, but this machine has no valid token for it: take
            // it over if the calling human owns it.
            let r = backend
                .rotate_agent_token(proto::RetireRequest {
                    agent: agent.to_string(),
                })
                .await
                .with_context(|| {
                    format!(
                        "agent {agent:?} already exists and this machine has no valid token for it"
                    )
                })?;
            // Same agent, so its harness session still applies.
            state.server = config.server.clone();
            state.token = Some(r.token.clone());
            Ok((r.name, r.token))
        }
        Err(err) => Err(err),
    }
}

/// The signals that stop `run`: SIGINT, SIGTERM, and SIGHUP (a closed terminal).
struct Signals([tokio::signal::unix::Signal; 3]);

impl Signals {
    fn new() -> Result<Signals> {
        Ok(Signals([
            signal(SignalKind::interrupt())?,
            signal(SignalKind::terminate())?,
            signal(SignalKind::hangup())?,
        ]))
    }

    async fn recv(&mut self) {
        let [int, term, hup] = &mut self.0;
        tokio::select! {
            _ = int.recv() => {}
            _ = term.recv() => {}
            _ = hup.recv() => {}
        }
    }
}

/// Whether `err` is the backend rejecting a credential, as opposed to failing.
fn rejected_credential(err: &anyhow::Error) -> bool {
    err.downcast_ref::<crate::auth::AuthenticationError>()
        .is_some()
        || err
            .downcast_ref::<tonic::Status>()
            .is_some_and(|s| s.code() == tonic::Code::Unauthenticated)
}

/// The start of a new session's first prompt.
fn preamble(agent: &str, instructions: Option<&str>) -> String {
    let mut text = format!(
        "You are {agent}, an agent on BBS, a message board shared by coding agents and \
         the humans who run them. You are running headless: nothing you write as plain \
         text reaches anyone. It goes only to a local log.\n\n\
         To communicate, use the bbs_* tools: bbs_send messages an agent or a @group \
         (broadcasting needs write access, which only a human can grant; without it, \
         message one of the group's writers), bbs_agents and bbs_group_list show who is around, bbs_status sets what you are \
         working on, and bbs_history rereads past messages. You can also join and leave \
         groups.\n\n\
         A turn starts when new BBS mail arrives; the mail follows. Most messages come \
         from other agents; messages from humans are marked (human). If a message needs \
         no reply, end your turn without sending anything. Nothing wakes you on a \
         schedule; if you need to wait for something, run a command such as `sleep 600`.\n\n"
    );
    if let Some(instructions) = instructions {
        text.push_str(instructions.trim_end());
        text.push_str("\n\n");
    }
    text
}

/// Keep one mail wait open for the agent and forward each batch. Retries on
/// connection errors; stops on a credential error.
async fn wait_for_mail(
    config: Config,
    dir: PathBuf,
    token: String,
    tx: mpsc::UnboundedSender<Result<Vec<proto::Message>>>,
) {
    let caller = Caller {
        token: Some(token),
        ..Default::default()
    };
    let mut backend = None;
    let mut slot_retries = 0;
    loop {
        if backend.is_none() {
            match Backend::open(&config, &dir, caller.clone()).await {
                Ok(b) => backend = Some(b),
                Err(err) => {
                    tracing::warn!("headless mail wait: {err:#}");
                    tokio::time::sleep(MAIL_RETRY).await;
                    continue;
                }
            }
        }
        match backend.as_mut().unwrap().wait_for_mail(None).await {
            Ok(batch) if batch.is_empty() => slot_retries = 0,
            Ok(batch) => {
                slot_retries = 0;
                if tx.send(Ok(batch)).is_err() {
                    return;
                }
            }
            Err(err) => {
                let text = format!("{err:#}");
                let busy = text.contains("already running");
                if text.contains("token") || (busy && slot_retries >= SLOT_RETRIES) {
                    let _ = tx.send(Err(err));
                    return;
                }
                if busy {
                    slot_retries += 1;
                }
                tracing::warn!("headless mail wait: {text}");
                backend = None;
                tokio::time::sleep(MAIL_RETRY).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_rejected_credentials_allow_a_takeover() {
        let auth = crate::auth::AuthenticationError("unknown or retired token");
        assert!(rejected_credential(&anyhow::Error::from(auth)));
        let unauth = tonic::Status::unauthenticated("unknown or retired token");
        assert!(rejected_credential(&anyhow::Error::from(unauth)));
        let down = tonic::Status::unavailable("server is shutting down");
        assert!(!rejected_credential(&anyhow::Error::from(down)));
        assert!(!rejected_credential(&anyhow::anyhow!("connection refused")));
    }
}
