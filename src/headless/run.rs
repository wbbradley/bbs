//! `bbs headless run`: drive one agent's harness from its BBS mail.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::json;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::acp::McpServer;
use super::effort;
use super::harness::{BbsTools, EffortUnconfirmed, Event, Harness, PromptNotStarted, Resume};
use super::log::Log;
use super::mcp::{TOKEN_ENV, Tools};
use super::outbox::{self, Outbox};
use super::settings::{DEFAULT_MAX_TURN_ERRORS, HarnessType, McpEnv, MidTurn, Settings};
use super::state::{AgentDir, State};
use super::{share, side};
use crate::app::format_messages;
use crate::backend::Backend;
use crate::cli::HeadlessRunArgs;
use crate::config::Config;
use crate::proto;
use crate::service::Caller;

/// Wait before retrying after the mail wait fails.
/// How long a stopping `run` waits for shared activity to be sent.
const SHARE_FLUSH: Duration = Duration::from_secs(2);
const MAIL_RETRY: Duration = Duration::from_secs(10);
/// "already running" retries: after a token takeover the old token's waiter can
/// hold the inbox briefly. Persisting means another process drives this agent.
const SLOT_RETRIES: u32 = 3;

pub async fn run(dir: &Path, as_human: Option<&str>, args: HeadlessRunArgs) -> Result<()> {
    let settings = Settings::load()?;
    let (harness, mut harness_config) = settings.harness(args.harness.as_deref())?;
    harness_config.check_mcp_env(
        std::env::var("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB")
            .ok()
            .as_deref(),
    )?;
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
    let max_turn_errors = args
        .max_turn_errors
        .or(settings.max_turn_errors)
        .unwrap_or(DEFAULT_MAX_TURN_ERRORS)
        .get();
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
    anyhow::ensure!(
        server_has(&config, dir, crate::backend::AUTO_REPLY).await,
        "this headless runner requires server capability auto-reply.v1; upgrade the server before starting it"
    );
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
    let mut identity_backend = Backend::open(
        &config,
        dir,
        Caller {
            token: Some(token.clone()),
            ..Default::default()
        },
    )
    .await?;
    let identity = identity_backend.whoami().await?;
    let owner = identity
        .owner
        .context("headless automatic replies require a registered owning human")?;
    let board = match &config.server {
        Some(server) => format!("remote:{server}"),
        None => format!("local:{}", std::fs::canonicalize(dir)?.display()),
    };
    let mut outbox = Outbox::open(&agent_dir.dir.join("owner-outbox"), board, identity.id)?;

    let mut log = Log::open(&agent_dir.log_path())?;
    let (share_warn, mut share_warnings) = mpsc::unbounded_channel();
    let sharing = harness_config.shares_activity().then(|| {
        let (queue, task) = share::start(
            config.clone(),
            dir.to_path_buf(),
            token.clone(),
            share_warn.clone(),
        );
        log.share(queue);
        task
    });
    log.event(
        "start",
        json!({ "agent": agent, "harness": harness, "cwd": cwd, "pid": std::process::id() }),
    );

    let mut mcp_args = vec!["headless".into(), "mcp".into(), "--auto-owner-reply".into()];
    if harness_config.mcp_env == McpEnv::Process && config.server.is_none() {
        mcp_args.extend([
            "--local-state-dir".into(),
            std::fs::canonicalize(dir)?.display().to_string(),
        ]);
    }
    let mcp = McpServer {
        name: "bbs".into(),
        command: std::env::current_exe()?.display().to_string(),
        args: mcp_args,
        env: std::iter::once((TOKEN_ENV.to_string(), token.clone()))
            .chain(
                config
                    .server
                    .clone()
                    .or_else(|| (harness_config.mcp_env == McpEnv::Process).then(String::new))
                    .map(|s| ("BBS_SERVER".to_string(), s)),
            )
            .collect(),
    };
    let saved = state.harness.as_deref();
    if let Some(from) = &args.adopt_session
        && saved != Some(from.as_str())
        && saved != Some(harness.as_str())
    {
        bail!(
            "no session saved under harness {from:?} to adopt (saved under {})",
            saved.map_or("none".to_string(), |s| format!("{s:?}"))
        );
    }
    let adopting = args.adopt_session.is_some();
    let resume = state
        .session_id
        .as_deref()
        .filter(|_| {
            !args.new_session
                && (saved == Some(harness.as_str()) || saved == args.adopt_session.as_deref())
        })
        .map(|id| Resume {
            id,
            dynamic_tools: state.dynamic_tools,
            // Adopting, or a Codex app-server resuming a thread opened through
            // ACP: a new session would lose the conversation, so fail instead.
            strict: adopting
                || (harness_config.kind == HarnessType::CodexAppServer && !state.dynamic_tools),
        });
    // Listen before starting the harness: a signal during setup drops the
    // start, which kills the harness's process group.
    let mut signals = Signals::new()?;
    let stderr = agent_dir.stderr_path();
    let side_session = match side::Session::begin(&config, dir, &token).await {
        Ok(session) => session,
        Err(error) => {
            let _ = share_warn.send(format!(
                "optional human input/context unavailable at startup: {error:#}"
            ));
            None
        }
    };
    let started = tokio::select! {
        started = Harness::start(
            &harness_config,
            &cwd,
            &stderr,
            BbsTools { mcp, tools: Tools::new(dir, token.clone()).auto_owner_reply(), input_enabled: side_session.is_some() },
            resume,
        ) => started,
        _ = signals.recv() => Err(anyhow::anyhow!("stopped during harness setup")),
    };
    let (agent_harness, mut process, mut events, warnings) = match started {
        Ok(started) => started,
        Err(err) => {
            log.event("stop", json!({ "error": format!("{err:#}") }));
            return Err(err);
        }
    };
    let mut interactions = if let Some(s) = side_session {
        Some(s.start(agent_harness.clone(), share_warn.clone()).await)
    } else {
        None
    };
    state.harness = Some(harness.clone());
    state.session_id = Some(agent_harness.session_id().to_string());
    state.dynamic_tools = agent_harness.dynamic_tools();
    agent_dir.save(&state)?;
    log.event(
        "session",
        json!({ "id": agent_harness.session_id(), "resumed": agent_harness.session_resumed() }),
    );
    for warning in warnings {
        eprintln!("bbs: warning: {warning}");
        log.event("warning", json!({ "message": warning }));
    }
    if explicit_steer && !agent_harness.can_steer() {
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

    let steer = mid_turn == MidTurn::Steer && agent_harness.can_steer();
    let mut preamble = if agent_harness.session_resumed() {
        None
    } else {
        let timers = server_has(&config, dir, crate::backend::TIMERS).await;
        Some(preamble(&agent, &owner, instructions.as_deref(), timers))
    };
    let (effort_reports, reporting) = effort::reporter(
        config.clone(),
        dir.to_path_buf(),
        token.clone(),
        share_warn.clone(),
    );
    let mut effort_control = effort::Control::default();
    let _ = effort_reports.send(effort_control.report(agent_harness.effort_state()));
    let mut delivery =
        outbox::Sender::start(config.clone(), dir.to_path_buf(), token.clone(), share_warn);
    let mut delivering = false;
    let mut completed_turn = None;
    let (mail_tx, mut mail_rx) = mpsc::unbounded_channel();
    let mail_task = tokio::spawn(wait_for_mail(
        config.clone(),
        dir.to_path_buf(),
        token,
        mail_tx,
    ));

    let (drain_reports, drain_rx) =
        tokio::sync::watch::channel(proto::ReportDrainRequest::default());
    let mut side_drain = interactions.as_ref().and_then(|side| side.drain_progress());
    let drain_reporting = if server_has(&config, dir, "agent-drain.v1").await {
        Some(tokio::spawn(report_drain(identity_backend, drain_rx)))
    } else {
        None
    };
    let mut received_through = 0_i64;
    // Failed/ambiguous turns can leave fetched mail unprocessed. A later
    // successful turn cannot acknowledge that older work as completed.
    let mut drain_failed = false;
    let mut queue: Vec<proto::Message> = Vec::new();
    let mut turn: Option<JoinHandle<Result<String>>> = None;
    let mut active_mail = Vec::new();
    let mut active_preamble = None;
    let mut active_effort = false;
    let mut last_start: Option<Instant> = None;
    let mut turn_errors = 0;
    // A resumed session that never completes a turn may be what fails.
    let mut session_proven = !agent_harness.session_resumed();
    let mut result = loop {
        // A read receipt only means the mail reached this process. A restart
        // acknowledgement waits for all queued/steered turns and durable owner
        // replies to finish. Never acknowledge on error or shutdown.
        if !drain_failed
            && turn.is_none()
            && queue.is_empty()
            && outbox.is_empty()
            && mail_rx.is_empty()
        {
            let side_completed_through = side_drain.as_ref().map_or(0, |r| *r.borrow());
            drain_reports.send_if_modified(|report| {
                if received_through > report.completed_through
                    || side_completed_through > report.side_completed_through
                {
                    report.completed_through = received_through;
                    report.side_completed_through = side_completed_through;
                    true
                } else {
                    false
                }
            });
        }
        if !delivering && let Some(part) = outbox.ready() {
            if delivery.queue.send(part.clone()).await.is_err() {
                break Err(anyhow::anyhow!(
                    "owner reply sender stopped; outbox retained"
                ));
            }
            delivering = true;
        }
        if outbox.is_empty()
            && let Some(stop) = completed_turn.take()
        {
            log.event("turn_end", json!({ "stop_reason": stop }));
        }
        if outbox.bytes() >= outbox::HIGH_WATER {
            break Err(anyhow::anyhow!(
                "owner reply outbox reached 16 MiB; stopping generation, retaining text for delivery on restart"
            ));
        }
        let ready_at = last_start
            .map(|t| t + interval)
            .unwrap_or_else(Instant::now);
        if turn.is_none() && outbox.is_empty() && !queue.is_empty() && Instant::now() >= ready_at {
            active_preamble = preamble.take();
            let mut prompt = active_preamble.clone().unwrap_or_default();
            active_mail = take_prompt_mail(&mut queue, &owner);
            prompt.push_str(&mail_prompt(&active_mail, &owner)?);
            log.event(
                "prompt",
                json!({ "text": prompt, "mail_text": format_messages(&active_mail, false)?, "mail_ids": mail_ids(&active_mail) }),
            );
            let agent_harness = agent_harness.clone();
            let requested_effort = effort_control.dispatch();
            active_effort = requested_effort.is_some();
            turn = Some(tokio::spawn(async move {
                agent_harness.prompt(&prompt, requested_effort).await
            }));
            last_start = Some(Instant::now());
            continue;
        }
        let throttled = turn.is_none() && outbox.is_empty() && !queue.is_empty();
        tokio::select! {
            mail = mail_rx.recv() => match mail {
                Some(Ok(mut batch)) => {
                    received_through = batch.iter().map(|m| m.id).max().unwrap_or(0).max(received_through);
                    // Control messages act now and are not prompts. The server
                    // accepts them only from the agent's owner.
                    for message in &batch {
                        if message.control() == proto::Control::SetEffort {
                            effort_control.request(proto::EffortRequest {
                                request_id: message.id, value: message.body.clone(),
                            }, &agent_harness.effort_state());
                            let _ = effort_reports.send(effort_control.report(agent_harness.effort_state()));
                        }
                    }
                    let cancels = batch.iter().filter(|m| m.control() == proto::Control::Cancel).count();
                    batch.retain(|m| m.control.is_none());
                    if cancels > 0 {
                        let running = turn.is_some();
                        if running && let Err(err) = agent_harness.cancel().await {
                            log.event("error", json!({ "message": format!("cancel: {err:#}") }));
                        }
                        log.event("cancel", json!({ "turn_running": running }));
                    }
                    if batch.is_empty() {
                        continue;
                    }
                    while steer && turn.is_some() && effort_control.pending.is_none() && queue.is_empty() && !batch.is_empty() {
                        // Steering fails when the turn is ending; the mail is
                        // then queued for the next turn.
                        let mut next = take_prompt_mail(&mut batch, &owner);
                        let text = mail_prompt(&next, &owner)?;
                        match agent_harness.steer(&text).await {
                            Ok(()) => {
                                let from: Vec<_> = next.iter().map(|m| m.sender_display_name()).collect();
                                log.event("mail", json!({ "count": next.len(), "from": from, "queued": false, "mail_ids": mail_ids(&next) }));
                                log.event("steer", json!({ "text": text, "mail_text": format_messages(&next, false)?, "mail_ids": mail_ids(&next) }));
                            }
                            Err(err) => {
                                log.event("warning", json!({
                                    "message": format!("steering failed, so the mail is queued: {err:#}"),
                                }));
                                next.append(&mut batch);
                                batch = next;
                                break;
                            }
                        }
                    }
                    if batch.is_empty() {
                        continue;
                    }
                    let from: Vec<_> = batch.iter().map(|m| m.sender_display_name()).collect();
                    let waiting = turn.is_some() || Instant::now() < ready_at;
                    log.event("mail", json!({ "count": batch.len(), "from": from, "queued": waiting, "mail_ids": mail_ids(&batch) }));
                    queue.extend(batch);
                }
                Some(Err(err)) => break Err(err),
                None => break Err(anyhow::anyhow!("the mail wait stopped")),
            },
            done = async { turn.as_mut().unwrap().await }, if turn.is_some() => {
                turn = None;
                // The wire reader queues notifications before resolving the
                // response. Persist those final deltas before clearing the turn.
                let mut drain_error = None;
                while let Ok(event) = events.try_recv() {
                    match event {
                        Event::EffortOutcome(outcome) => effort_control.complete(outcome),
                        Event::EffortChanged => {},
                        other => if let Err(err) = record_side(&mut log, &mut outbox, other, interactions.as_ref()) { drain_error = Some(err); break; },
                    }
                }
                let _ = effort_reports.send(effort_control.report(agent_harness.effort_state()));
                if let Some(err) = drain_error { break Err(err); }
                if let Err(err) = outbox.seal() { break Err(err); }
                let message = match done {
                    Ok(Ok(stop)) => {
                        active_mail.clear();
                        active_preamble = None;
                        completed_turn = Some(stop);
                        turn_errors = 0;
                        session_proven = true;
                        continue;
                    }
                    Ok(Err(err)) => {
                        drain_failed |= err.downcast_ref::<PromptNotStarted>().is_none();
                        if let Some(uncertain) = err.downcast_ref::<EffortUnconfirmed>() {
                            log.event(if uncertain.prompt_sent { "delivery_uncertain" } else { "unprocessed" }, json!({ "text": format_messages(&active_mail, false)?, "acceptance": if uncertain.prompt_sent { "unknown" } else { "not_sent" } }));
                            active_mail.clear();
                            break Err(err);
                        }
                        if err.downcast_ref::<PromptNotStarted>().is_some() {
                            // The override was definitively rejected before turn acceptance.
                            // Retry only the ordinary mail, never the rejected control.
                            active_mail.append(&mut queue);
                            queue = std::mem::take(&mut active_mail);
                            preamble = active_preamble.take();
                        } else if active_effort && !active_mail.is_empty() {
                            // Acceptance may be ambiguous: preserve evidence without resend.
                            log.event("delivery_uncertain", json!({ "text": format_messages(&active_mail, false)?, "acceptance": "unknown" }));
                            active_mail.clear();
                        }
                        format!("{err:#}")
                    },
                    Err(err) => { drain_failed = true; err.to_string() },
                };
                log.event("turn_error", json!({ "message": message }));
                turn_errors += 1;
                if turn_errors >= max_turn_errors {
                    if !session_proven {
                        // Start a new session next time instead of this one.
                        state.session_id = None;
                        if let Err(err) = agent_dir.save(&state) {
                            log.event("warning", json!({ "message": format!("{err:#}") }));
                        }
                    }
                    break Err(anyhow::anyhow!(
                        "{turn_errors} turns failed in a row; the last: {message}"
                    ));
                }
            }
            event = events.recv() => match event {
                Some(Event::Exited) | None => break Err(anyhow::anyhow!("the harness exited")),
                Some(Event::EffortChanged) => {
                    let _ = effort_reports.send(effort_control.report(agent_harness.effort_state()));
                }
                Some(Event::EffortOutcome(outcome)) => {
                    if !outcome.detail.is_empty() {
                        log.event("warning", json!({ "message": outcome.detail, "effort_request": outcome.request_id }));
                    }
                    effort_control.complete(outcome);
                    let _ = effort_reports.send(effort_control.report(agent_harness.effort_state()));
                }
                Some(event) => if let Err(err) = record_side(&mut log, &mut outbox, event, interactions.as_ref()) { break Err(err); },
            },
            reply = delivery.replies.recv(), if delivering => {
                match reply {
                    Some(Ok((key, id))) => {
                        if let Err(err) = delivered(&mut outbox, &mut log, &key, id) { break Err(err); }
                        delivering = false;
                    }
                    Some(Err(err)) => break Err(err),
                    None => break Err(anyhow::anyhow!("owner reply sender stopped; outbox retained")),
                }
            }
            changed = async { side_drain.as_mut().unwrap().changed().await }, if side_drain.is_some() => {
                if changed.is_err() { side_drain = None; }
            }
            _ = tokio::time::sleep_until(ready_at), if throttled => {}
            status = process.wait() => break Err(match status {
                Ok(status) => anyhow::anyhow!("the harness exited ({status})"),
                Err(err) => anyhow::anyhow!("waiting for the harness: {err}"),
            }),
            Some(warning) = share_warnings.recv() => {
                eprintln!("bbs: warning: {warning}");
                log.event("warning", json!({ "message": warning }));
            }
            _ = signals.recv() => break Ok(()),
        }
    };
    mail_task.abort();
    if let Some(task) = drain_reporting {
        task.abort();
    }
    // Mail the wait already took but the loop had not yet received.
    while let Ok(Ok(batch)) = mail_rx.try_recv() {
        queue.extend(batch.into_iter().filter(|m| m.control.is_none()));
    }
    if turn.is_some() {
        let _ = agent_harness.cancel().await;
    }
    // Retained side children must finish while the provider can still reply.
    if let Some(interactions) = interactions.as_mut() {
        interactions.finish_btw().await;
    }
    // Stop the producer before draining, so late cancellation text is retained.
    if !process.stop().await {
        result = Err(anyhow::anyhow!(
            "harness output reader did not finish during shutdown; persisted owner text retained, unread output may be incomplete"
        ));
    }
    if let Some(mut task) = turn.take() {
        // Allow a received start ACK to bind/drain buffered Codex text after
        // process exit. This does not submit or retry any prompt.
        if tokio::time::timeout(Duration::from_secs(1), &mut task)
            .await
            .is_err()
        {
            task.abort();
        }
    }
    log.flush();
    if !queue.is_empty() {
        // Already marked read on the server; keep them where the owner can find them.
        log.event(
            "unprocessed",
            json!({ "text": format_messages(&queue, false)? }),
        );
    }
    // Drain effort ACKs already emitted before harness exit/turn completion.
    while let Ok(event) = events.try_recv() {
        match event {
            Event::EffortOutcome(outcome) => effort_control.complete(outcome),
            Event::EffortChanged => {}
            other => {
                if let Err(err) = record_side(&mut log, &mut outbox, other, interactions.as_ref()) {
                    result = Err(err);
                }
            }
        }
    }
    if let Some(interactions) = interactions {
        interactions.finish().await;
    }
    if let Err(err) = outbox.seal() {
        result = Err(err);
    }
    let flush = async {
        while !outbox.is_empty() {
            if !delivering && let Some(part) = outbox.ready() {
                delivery
                    .queue
                    .send(part.clone())
                    .await
                    .context("owner sender stopped")?;
                delivering = true;
            }
            let (key, id) = delivery
                .replies
                .recv()
                .await
                .context("owner sender stopped")??;
            delivered(&mut outbox, &mut log, &key, id)?;
            delivering = false;
        }
        Ok::<_, anyhow::Error>(())
    };
    match tokio::time::timeout(Duration::from_secs(5), flush).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => result = Err(err),
        Err(_) => {
            eprintln!("bbs: owner reply delivery pending; private outbox retained for the next run")
        }
    }
    if outbox.is_empty()
        && let Some(stop) = completed_turn.take()
    {
        log.event("turn_end", json!({ "stop_reason": stop }));
    }
    let mut final_effort = agent_harness.effort_state();
    if effort_control.interrupted() {
        final_effort.current = None;
        final_effort.reason = "driver stopped before effort confirmation".into();
    }
    if active_effort && !active_mail.is_empty() {
        log.event(
            "delivery_uncertain",
            json!({ "text": format_messages(&active_mail, false)?, "acceptance": "unknown" }),
        );
    }
    let _ = effort_reports.send(effort_control.report(final_effort));
    drop(effort_reports);
    match &result {
        Ok(()) => log.event("stop", json!({})),
        Err(err) => log.event("stop", json!({ "error": format!("{err:#}") })),
    }

    log.unshare();
    if let Some(task) = sharing {
        // Let the last records, "stop" among them, reach the server.
        let _ = tokio::time::timeout(SHARE_FLUSH, task).await;
    }
    let _ = tokio::time::timeout(SHARE_FLUSH, reporting).await;
    result
}

fn record_side(
    log: &mut Log,
    outbox: &mut Outbox,
    event: Event,
    side: Option<&side::Side>,
) -> Result<()> {
    // A native question can block the provider without a separate Tool event.
    // Commit preceding assistant text for delivery before exposing the form.
    if matches!(&event, Event::OwnerInput(_)) {
        outbox.seal()?;
    }
    if let Some(side) = side {
        side.event(&event);
    }
    record(log, outbox, event)
}

fn record(log: &mut Log, outbox: &mut Outbox, event: Event) -> Result<()> {
    match event {
        Event::Text(source, text) => {
            outbox.append(source.clone(), &text)?;
            log.local_event("text", json!({ "text": text, "source": source }));
        }
        Event::TextEnd(source, fallback) => {
            if let Some(text) = fallback {
                outbox.append(source.clone(), &text)?;
                log.local_event("text", json!({ "text": text, "source": source }));
            }
            outbox.seal()?;
        }
        Event::Thought(t) => {
            outbox.seal()?;
            log.chunk("thought", &t);
        }
        Event::Tool(id, mut fields) => {
            outbox.seal()?;
            fields["id"] = id.into();
            log.event("tool", fields);
        }
        Event::Permission(fields) => {
            outbox.seal()?;
            log.event("permission", fields);
        }
        Event::Warning(message) => log.event("warning", json!({ "message": message })),
        Event::Other(kind) => log.event("update", json!({ "kind": kind })),
        Event::Exited
        | Event::EffortChanged
        | Event::EffortOutcome(_)
        | Event::OwnerInput(_)
        | Event::InputClosed(_)
        | Event::ContextChanged(_) => {}
    }
    Ok(())
}

fn delivered(outbox: &mut Outbox, log: &mut Log, key: &str, id: i64) -> Result<()> {
    let part = outbox.acknowledge(key)?;
    log.local_event(
        "owner_delivery",
        json!({ "key": key, "mail_id": id, "source": part.source }),
    );
    log.shared_event(
        "text",
        json!({ "text": part.body, "mail_id": id, "source": part.source, "ts": part.emitted_at }),
    );
    Ok(())
}

fn mail_ids(mail: &[proto::Message]) -> Vec<i64> {
    mail.iter().map(|m| m.id).collect()
}

fn owner_direct(message: &proto::Message, owner: &str) -> bool {
    message.from_kind() == proto::Kind::Human
        && message.from_name.eq_ignore_ascii_case(owner)
        && message.to_group.is_none()
        && message.timer_id.is_none()
        && !message.body.trim().is_empty()
}

/// One owner DM per input; consecutive other mail stays batched in order.
fn take_prompt_mail(queue: &mut Vec<proto::Message>, owner: &str) -> Vec<proto::Message> {
    let count = if queue.first().is_some_and(|m| owner_direct(m, owner)) {
        1
    } else {
        queue
            .iter()
            .position(|m| owner_direct(m, owner))
            .unwrap_or(queue.len())
    };
    queue.drain(..count).collect()
}

fn mail_prompt(mail: &[proto::Message], owner: &str) -> Result<String> {
    if let [message] = mail
        && owner_direct(message, owner)
    {
        return Ok(message.body.clone());
    }
    Ok(format!(
        "New BBS mail ({} message{}):\n\n{}",
        mail.len(),
        if mail.len() == 1 { "" } else { "s" },
        format_messages(mail, false)?
    ))
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
/// Whether the server advertises `capability`; false if it cannot be asked.
async fn server_has(config: &Config, dir: &Path, capability: &str) -> bool {
    let info = async {
        Backend::open(config, dir, Caller::default())
            .await?
            .server_info()
            .await
    };
    match info.await {
        Ok(info) => info.capabilities.iter().any(|c| c == capability),
        Err(err) => {
            tracing::warn!("asking the server for {capability}: {err:#}");
            false
        }
    }
}

/// How the agent waits: a timer where the server has them, else a long command.
fn wake_sentence(timers: bool) -> &'static str {
    if timers {
        "To be woken later, set a timer for yourself with bbs_timer_add (bbs_timer_list \
         and bbs_timer_rm manage them); its message arrives as new mail, marked (timer N)."
    } else {
        "Nothing wakes you on a schedule; if you need to wait for something, run a \
         command such as `sleep 600`."
    }
}

fn preamble(agent: &str, owner: &str, instructions: Option<&str>, timers: bool) -> String {
    let mut text = format!(
        "You are {agent}, an agent on BBS, a message board shared by coding agents and \
         the humans who run them. Your owning human is {owner}. Speak naturally: your ordinary \
         commentary and final replies are delivered automatically to {owner}. Do not use \
         bbs_send to message {owner}; write your ordinary reply instead. Thoughts, tool calls, \
         and permission events are not sent as chat.\n\n\
         To contact other recipients, use the bbs_* tools: bbs_send messages an agent, another human, or a @group \
         (broadcasting needs write access, which only a human can grant; without it, \
         message one of the group's writers), bbs_agents and bbs_group_list show who is around, bbs_status sets what you are \
         working on, and bbs_history rereads past messages. bbs_search searches the whole board \
         by literal message text, including other conversations. You can also join and leave \
         groups. bbs_blockers lists group help requests; bbs_blocker_put creates or replaces \
         one with immutable history, bbs_blocker_get reads its chain, and bbs_blocker_cancel \
         cancels it. Creation sends no ordinary mail; a human answer arrives as mail.\n\n\
         Direct messages from your owning human arrive as plain user input. Other mail \
         includes BBS sender and recipient headers; humans there are marked (human). If a message needs \
         no reply, end your turn without sending anything. {}\n\n",
        wake_sentence(timers)
    );
    if let Some(instructions) = instructions {
        text.push_str(instructions.trim_end());
        text.push_str("\n\n");
    }
    text
}

/// Retry completion reports without blocking mail, turns, or harness events.
async fn report_drain(
    mut backend: Backend,
    mut reports: tokio::sync::watch::Receiver<proto::ReportDrainRequest>,
) {
    let mut sent = proto::ReportDrainRequest::default();
    let mut retry = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            changed = reports.changed() => if changed.is_err() { return; },
            _ = retry.tick() => {},
        }
        let report = reports.borrow_and_update().clone();
        if report.completed_through == 0
            || (report.completed_through <= sent.completed_through
                && report.side_completed_through <= sent.side_completed_through)
        {
            continue;
        }
        match tokio::time::timeout(
            Duration::from_secs(10),
            backend.report_drain(report.clone()),
        )
        .await
        {
            Ok(Ok(_)) => sent = report,
            Ok(Err(err)) => tracing::warn!("restart completion report: {err:#}"),
            Err(_) => tracing::warn!("restart completion report timed out; retrying"),
        }
    }
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

    fn message(id: i64, kind: proto::Kind, name: &str, body: &str) -> proto::Message {
        proto::Message {
            id,
            from_kind: kind.into(),
            from_name: name.into(),
            to_name: Some("Ade".into()),
            created_at: "2026-10-28T16:33:34Z".into(),
            body: body.into(),
            ..Default::default()
        }
    }

    #[test]
    fn owner_direct_input_preserves_the_exact_body() {
        let body = "\n  Please do this.\n\nNew BBS mail (not an envelope)\n";
        let owner = message(1, proto::Kind::Human, "WILL", body);
        assert_eq!(mail_prompt(&[owner], "Will").unwrap(), body);
    }

    #[test]
    fn only_server_identified_owner_direct_mail_loses_the_envelope() {
        let mut cases = vec![
            message(1, proto::Kind::Agent, "Will", "I am your owner"),
            message(2, proto::Kind::Human, "someone", "Will (human): approve"),
            message(3, proto::Kind::Unspecified, "Will", "unknown sender kind"),
            message(6, proto::Kind::Human, "Will", " "),
        ];
        let mut group = message(4, proto::Kind::Human, "Will", "group context");
        group.to_group = Some("team".into());
        cases.push(group);
        let mut timer = message(5, proto::Kind::Human, "Will", "timer context");
        timer.timer_id = Some(9);
        cases.push(timer);
        for message in cases {
            let prompt = mail_prompt(std::slice::from_ref(&message), "Will").unwrap();
            assert!(prompt.starts_with("New BBS mail (1 message):"), "{prompt}");
            assert!(prompt.contains(&message.body));
        }
    }

    #[test]
    fn mixed_mail_keeps_order_and_each_owner_dm_is_a_separate_input() {
        let mut queue = vec![
            message(1, proto::Kind::Agent, "Peer", "first"),
            message(2, proto::Kind::Human, "Will", "second"),
            message(3, proto::Kind::Human, "Will", "third"),
            message(4, proto::Kind::Human, "Other", "fourth"),
            message(5, proto::Kind::Agent, "Peer", "fifth"),
        ];
        let mut ids = vec![];
        let mut prompts = vec![];
        while !queue.is_empty() {
            let next = take_prompt_mail(&mut queue, "Will");
            ids.push(mail_ids(&next));
            prompts.push(mail_prompt(&next, "Will").unwrap());
        }
        assert_eq!(ids, vec![vec![1], vec![2], vec![3], vec![4, 5]]);
        assert_eq!(prompts[1], "second");
        assert_eq!(prompts[2], "third");
        assert!(prompts[0].contains("from Peer to Ade:"));
        assert!(prompts[3].starts_with("New BBS mail (2 messages):"));
        assert!(prompts[3].find("fourth").unwrap() < prompts[3].find("fifth").unwrap());
    }

    #[test]
    fn the_preamble_offers_timers_only_where_the_server_has_them() {
        let with = preamble("Ade", "Will", None, true);
        assert!(with.contains("bbs_timer_add"), "{with}");
        assert!(!with.contains("sleep 600"), "{with}");
        let without = preamble("Ade", "Will", None, false);
        assert!(without.contains("sleep 600"), "{without}");
        assert!(!without.contains("bbs_timer"), "{without}");
    }

    #[test]
    fn final_text_persists_before_completion_and_only_acknowledged_text_is_shared() {
        let dir = tempfile::tempdir().unwrap();
        let mut outbox = Outbox::open(&dir.path().join("outbox"), "board".into(), 1).unwrap();
        let mut log = Log::open(&dir.path().join("log")).unwrap();
        let (tx, mut rx) = mpsc::channel(20);
        log.share(tx);
        let source = outbox::Source {
            session: "parent".into(),
            message: Some("m".into()),
            ..Default::default()
        };
        record(
            &mut log,
            &mut outbox,
            Event::Text(source.clone(), "first ".into()),
        )
        .unwrap();
        record(
            &mut log,
            &mut outbox,
            Event::Text(source.clone(), "last\n".into()),
        )
        .unwrap();
        assert!(rx.try_recv().is_err());
        record(&mut log, &mut outbox, Event::TextEnd(source, None)).unwrap();
        let key = outbox.ready().unwrap().key.clone();
        let emitted_at = outbox.ready().unwrap().emitted_at.clone();
        assert_eq!(outbox.ready().unwrap().body, "first last\n");
        delivered(&mut outbox, &mut log, &key, 42).unwrap();
        log.event("turn_end", json!({}));
        let text: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(text["mail_id"], 42);
        assert_eq!(text["ts"], emitted_at);
        assert_eq!(text["text"], "first last\n");
        let end: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(end["event"], "turn_end");
        assert!(rx.try_recv().is_err());
    }

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
