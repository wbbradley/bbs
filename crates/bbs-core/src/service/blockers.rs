use anyhow::{Context, Result, bail};
use rusqlite::Connection;

use super::{Caller, Ctx, principal, with_ctx};
use crate::db::{self, Kind, Principal, blockers as store};
use crate::{blocker_choices, proto};

const MAX_TEXT: usize = 8192;
const MAX_PAGE: u32 = 100;

fn text(value: &str, what: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > MAX_TEXT {
        bail!("blocker {what} must be nonblank and at most {MAX_TEXT} UTF-8 bytes");
    }
    Ok(())
}

fn key(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 128 {
        bail!("blocker request key must be 1 to 128 UTF-8 bytes");
    }
    Ok(())
}

fn positive(id: i64, what: &str) -> Result<()> {
    if id <= 0 {
        bail!("blocker {what} must be positive");
    }
    Ok(())
}

fn page(limit: Option<u32>, default: u32) -> Result<usize> {
    let limit = limit.unwrap_or(default);
    if limit == 0 || limit > MAX_PAGE {
        bail!("blocker limit must be 1 to {MAX_PAGE}");
    }
    Ok(limit as usize)
}

/// Only the creating agent's owner answers its blockers.
fn may_answer(ctx: &Ctx<'_>, actor: &Principal, row: &store::Row) -> Result<bool> {
    Ok(
        actor.kind == Kind::Human
            && db::agent_owner_id(ctx.conn, row.creator.id)? == Some(actor.id),
    )
}

/// The creating agent, its owner, or another agent with the same owner
/// cancels.
fn may_cancel(ctx: &Ctx<'_>, actor: &Principal, row: &store::Row) -> Result<bool> {
    let owner = db::agent_owner_id(ctx.conn, row.creator.id)?;
    Ok(match actor.kind {
        Kind::Human => owner == Some(actor.id),
        Kind::Agent => {
            actor.id == row.creator.id
                || (owner.is_some() && db::agent_owner_id(ctx.conn, actor.id)? == owner)
        }
    })
}

fn require_row(ctx: &Ctx<'_>, id: i64) -> Result<store::Row> {
    store::get(ctx.conn, id)?.with_context(|| format!("no such blocker #{id}"))
}

fn require_open(row: &store::Row) -> Result<()> {
    if row.outcome.is_some() || row.replaced_by.is_some() {
        bail!(
            "blocker #{} is no longer open/current; refresh before submitting",
            row.id
        );
    }
    Ok(())
}

/// The goal a blocker version proposes; none for other blockers.
fn proposed_goal(row: &store::Row) -> Result<Option<proto::BlockerGoal>> {
    Ok(row
        .choices
        .as_deref()
        .map(blocker_choices::goal_from_json)
        .transpose()?
        .flatten())
}

fn selected(json: Option<&str>) -> Result<Vec<String>> {
    Ok(json
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default())
}

fn as_proto(ctx: &Ctx<'_>, actor: &Principal, row: store::Row) -> Result<proto::Blocker> {
    let open = row.outcome.is_none() && row.replaced_by.is_none();
    let can_cancel = open && may_cancel(ctx, actor, &row)?;
    let goal = proposed_goal(&row)?;
    let can_answer =
        open && may_answer(ctx, actor, &row)? && db::agent_is_live(ctx.conn, row.creator.id)?;
    let status = match row.outcome.as_ref().map(|e| e.kind.as_str()) {
        Some("answered") => proto::BlockerStatus::BlockerAnswered,
        Some("cancelled") => proto::BlockerStatus::BlockerCancelled,
        Some("superseded") => proto::BlockerStatus::BlockerSuperseded,
        _ => proto::BlockerStatus::BlockerOpen,
    };
    let mut creator = principal(row.creator);
    creator.owner = row.creator_owner;
    let choices = row
        .choices
        .as_deref()
        .map(blocker_choices::from_json)
        .transpose()?;
    let outcome = row
        .outcome
        .map(|o| -> Result<_> {
            Ok(proto::BlockerOutcome {
                status: status.into(),
                selected: selected(o.selected.as_deref())?,
                actor: Some(proto::Principal {
                    owner: o.actor_owner,
                    ..principal(o.actor)
                }),
                created_at: o.created_at,
                answer: o.answer,
                message_id: o.message_id,
            })
        })
        .transpose()?;
    Ok(proto::Blocker {
        id: row.id,
        creator: Some(creator),
        body: row.body,
        created_at: row.created_at,
        root_id: row.root_id,
        replaces: row.replaces,
        replaced_by: row.replaced_by,
        status: status.into(),
        outcome,
        can_answer,
        can_cancel,
        choices,
        goal,
        // Blockers have no group; old clients get it empty.
        ..Default::default()
    })
}

pub fn list_blockers(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::ListBlockersRequest,
) -> Result<proto::ListBlockersResponse> {
    let limit = page(req.limit, 50)?;
    if req.group.is_some() {
        bail!("blockers have no groups; list by agent or mine instead");
    }
    if req.agent.is_some() && req.mine {
        bail!("blocker agent and mine scopes are mutually exclusive");
    }
    if let Some(id) = req.after_root_id {
        positive(id, "root cursor")?;
    }
    if let Some(id) = req.before_root_id {
        positive(id, "root cursor")?;
    }
    if req.closed_only && req.after_root_id.is_some() {
        bail!("closed-only blockers use before_root_id, not after_root_id");
    }
    if !req.closed_only && req.before_root_id.is_some() {
        bail!("before_root_id requires closed_only");
    }
    with_ctx(conn, caller, |ctx| {
        let actor = ctx.actor()?;
        let human = if req.mine {
            Some(ctx.human()?.id)
        } else {
            None
        };
        let agent = req
            .agent
            .as_ref()
            .map(|name| {
                db::live_agent_by_name(ctx.conn, name)?
                    .with_context(|| format!("no such live agent {name}"))
                    .map(|a| a.id)
            })
            .transpose()?;
        let mut rows = store::list(
            ctx.conn,
            agent,
            human,
            if req.closed_only {
                store::ListMode::Closed {
                    before: req.before_root_id,
                }
            } else {
                store::ListMode::All {
                    include_closed: req.include_closed,
                    after: req.after_root_id.unwrap_or(0),
                }
            },
            limit + 1,
        )?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let cursor = has_more.then(|| rows.last().expect("nonempty page").root_id);
        Ok(proto::ListBlockersResponse {
            blockers: rows
                .into_iter()
                .map(|r| as_proto(ctx, &actor, r))
                .collect::<Result<_>>()?,
            active_count: store::active_count(ctx.conn, agent, human)?,
            has_more,
            next_after_root_id: if req.closed_only { None } else { cursor },
            next_before_root_id: if req.closed_only { cursor } else { None },
        })
    })
}

pub fn get_blocker(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::GetBlockerRequest,
) -> Result<proto::GetBlockerResponse> {
    positive(req.id, "ID")?;
    if let Some(id) = req.before_id {
        positive(id, "history cursor")?;
    }
    let limit = page(req.limit, 20)?;
    with_ctx(conn, caller, |ctx| {
        let actor = ctx.actor()?;
        let row = require_row(ctx, req.id)?;
        let current = store::head(ctx.conn, row.root_id)?;
        let mut history = store::history(
            ctx.conn,
            row.root_id,
            req.before_id.unwrap_or(i64::MAX),
            limit + 1,
        )?;
        let has_more = history.len() > limit;
        history.truncate(limit);
        let next_before_id = has_more.then(|| history.last().expect("nonempty page").id);
        Ok(proto::GetBlockerResponse {
            blocker: Some(as_proto(ctx, &actor, row)?),
            current: Some(as_proto(ctx, &actor, current)?),
            history: history
                .into_iter()
                .map(|r| as_proto(ctx, &actor, r))
                .collect::<Result<_>>()?,
            has_more,
            next_before_id,
        })
    })
}

fn replay(
    ctx: &Ctx<'_>,
    actor: &Principal,
    key: &str,
    same_request: impl FnOnce(&store::Operation) -> bool,
) -> Result<Option<proto::BlockerMutationResponse>> {
    let Some(operation) = store::operation(ctx.conn, actor, key)? else {
        return Ok(None);
    };
    if !same_request(&operation) {
        bail!("blocker request key was already used for a different operation");
    }
    Ok(Some(proto::BlockerMutationResponse {
        blocker: Some(as_proto(ctx, actor, require_row(ctx, operation.id())?)?),
        replayed: true,
        message_id: operation.message_id(),
    }))
}

fn finish(
    ctx: &Ctx<'_>,
    actor: &Principal,
    id: i64,
    message_id: Option<i64>,
) -> Result<proto::BlockerMutationResponse> {
    Ok(proto::BlockerMutationResponse {
        blocker: Some(as_proto(ctx, actor, require_row(ctx, id)?)?),
        replayed: false,
        message_id,
    })
}

pub fn put_blocker(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::PutBlockerRequest,
) -> Result<proto::BlockerMutationResponse> {
    crate::blocker_limits::validate_body(&req.body)?;
    key(&req.request_key)?;
    if let Some(goal) = &req.goal {
        blocker_choices::validate_goal(goal)?;
    }
    let body = match &req.goal {
        Some(goal) => blocker_choices::goal_body(&req.body, goal)?,
        None => req.body.clone(),
    };
    if let Some(id) = req.replaces {
        positive(id, "predecessor ID")?;
    }
    let choices = match &req.goal {
        Some(_) if req.choices.is_some() => {
            bail!("a goal proposal's choices are fixed; leave choices unset")
        }
        Some(goal) => Some(blocker_choices::stored_json(
            &blocker_choices::goal_choices(),
            Some(goal),
        )),
        None => req
            .choices
            .as_ref()
            .map(|c| blocker_choices::validate(c).map(|c| blocker_choices::to_json(&c)))
            .transpose()?,
    };
    with_ctx(conn, caller, |ctx| {
        let actor = ctx.agent()?;
        // `group` is ignored: blockers belong to their creator.
        if let Some(response) = replay(ctx, &actor, &req.request_key, |op| {
            matches!(op,
            store::Operation::Put { body: old_body, replaces, choices: old_choices, .. }
            if *old_body == body && *replaces == req.replaces && *old_choices == choices)
        })? {
            return Ok(response);
        }
        if db::agent_owner_id(ctx.conn, actor.id)?.is_none() {
            bail!("only an agent with an owner can create blockers, since its owner answers them");
        }
        let prior = req.replaces.map(|id| require_row(ctx, id)).transpose()?;
        if let Some(prior) = &prior {
            if prior.creator.id != actor.id {
                bail!("only blocker #{}'s creator can replace it", prior.id);
            }
            if prior.replaced_by.is_some() {
                bail!(
                    "blocker #{} was already replaced; refresh before replacing",
                    prior.id
                );
            }
        }
        let id = store::insert(
            ctx.conn,
            actor.id,
            store::Version {
                body: &body,
                choices: choices.as_deref(),
            },
            prior.as_ref(),
            &ctx.now_s,
            &req.request_key,
        )?;
        store::event(
            ctx.conn, id, "created", &actor, &ctx.now_s, None, None, None,
        )?;
        if let Some(prior) = &prior
            && prior.outcome.is_none()
        {
            store::event(
                ctx.conn,
                prior.id,
                "superseded",
                &actor,
                &ctx.now_s,
                None,
                None,
                None,
            )?;
        }
        finish(ctx, &actor, id, None)
    })
}

pub fn answer_blocker(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::AnswerBlockerRequest,
) -> Result<proto::BlockerMutationResponse> {
    positive(req.id, "ID")?;
    if req.selected.is_empty() {
        text(&req.answer, "answer")?;
    } else if req.answer.len() > MAX_TEXT {
        bail!("blocker answer must be at most {MAX_TEXT} UTF-8 bytes");
    }
    key(&req.request_key)?;
    with_ctx(conn, caller, |ctx| {
        let actor = ctx.human()?;
        let row = require_row(ctx, req.id)?;
        let choices = row
            .choices
            .as_deref()
            .map(blocker_choices::from_json)
            .transpose()?;
        // Replays compare the canonical answer, so a selection retried in
        // another order is the same request.
        let answer = blocker_choices::answer(choices.as_ref(), &req.selected, &req.answer);
        if let Some(response) = replay(ctx, &actor, &req.request_key, |op| {
            matches!((op, &answer),
            (store::Operation::Answer { id, answer: old, selected, .. }, Ok(new))
            if *id == req.id && *old == new.text && *selected == new.selected_json())
        })? {
            return Ok(response);
        }
        if !may_answer(ctx, &actor, &row)? {
            bail!("only {}'s owner can answer its blockers", row.creator.name);
        }
        require_open(&row)?;
        if !db::agent_is_live(ctx.conn, row.creator.id)? {
            bail!("creating agent is retired; cancel or replace this blocker instead");
        }
        let goal = proposed_goal(&row)?;
        let answer = answer?;
        text(&answer.text, "answer")?;
        // Approval sets the goal as the owner's own goal-set would, before the
        // answer mail, so the runner has the goal when the agent reads it.
        if let Some(goal) = goal
            && answer.selected == [blocker_choices::GOAL_APPROVE]
        {
            let to = row.creator.id;
            super::insert_control(ctx, &actor, to, &goal.objective, "goal-set")?;
            if let Some(seconds) = goal.duration_s {
                let deadline = (ctx.now + chrono::Duration::seconds(seconds as i64))
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                super::insert_control(ctx, &actor, to, &deadline, "goal-deadline")?;
            }
        }
        // The agent has the blocker; cite it rather than repeat its body.
        let body = format!(
            "Blocker #{} was answered by {}.\n\nAnswer:\n{}",
            row.id, actor.name, answer.text
        );
        super::validate_body(&body)?;
        let message_id = db::insert_message(
            ctx.conn,
            &actor,
            db::To::Direct(Kind::Agent, row.creator.id),
            &body,
            &ctx.now_s,
        )?;
        db::record_human_conversation(ctx.conn, row.creator.id, actor.id, &ctx.now_s)?;
        store::event(
            ctx.conn,
            row.id,
            "answered",
            &actor,
            &ctx.now_s,
            Some(store::Answer {
                text: &answer.text,
                selected: answer.selected_json().as_deref(),
            }),
            Some(message_id),
            Some(&req.request_key),
        )?;
        finish(ctx, &actor, row.id, Some(message_id))
    })
}

pub fn cancel_blocker(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::CancelBlockerRequest,
) -> Result<proto::BlockerMutationResponse> {
    positive(req.id, "ID")?;
    key(&req.request_key)?;
    with_ctx(conn, caller, |ctx| {
        let actor = ctx.actor()?;
        if let Some(response) = replay(ctx, &actor, &req.request_key, |op| {
            matches!(op,
            store::Operation::Cancel { id, .. } if *id == req.id)
        })? {
            return Ok(response);
        }
        let row = require_row(ctx, req.id)?;
        if !may_cancel(ctx, &actor, &row)? {
            bail!(
                "only {}, its owner, or another of its owner's agents can cancel its blockers",
                row.creator.name
            );
        }
        require_open(&row)?;
        // Tell the creator, as an answer would, unless it cancelled its own.
        let mut message_id = None;
        if db::agent_is_live(ctx.conn, row.creator.id)?
            && super::tell_agent(ctx, &actor, row.creator.id)?
        {
            let body = format!("Blocker #{} was cancelled by {}.", row.id, actor.name);
            let id = db::insert_message(
                ctx.conn,
                &actor,
                db::To::Direct(Kind::Agent, row.creator.id),
                &body,
                &ctx.now_s,
            )?;
            if actor.kind == Kind::Human {
                db::record_human_conversation(ctx.conn, row.creator.id, actor.id, &ctx.now_s)?;
            }
            message_id = Some(id);
        }
        store::event(
            ctx.conn,
            row.id,
            "cancelled",
            &actor,
            &ctx.now_s,
            None,
            message_id,
            Some(&req.request_key),
        )?;
        finish(ctx, &actor, row.id, message_id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service;

    struct Board {
        conn: Connection,
        humans: [Caller; 3],
        agents: [Caller; 3],
        ids: [i64; 3],
    }

    impl Board {
        fn new(conn: Connection) -> Self {
            let mut conn = conn;
            db::migrate(&mut conn).unwrap();
            let humans = ["will", "ada", "external"].map(|name| {
                service::ensure_human(&mut conn, proto::EnsureHumanRequest { name: name.into() })
                    .unwrap();
                Caller {
                    human: Some(name.into()),
                    ..Default::default()
                }
            });
            let mut ids = Vec::new();
            // will owns Mateo and Zed, ada owns Lu, and external owns none.
            let agents = [("Mateo", 0), ("Zed", 0), ("Lu", 1)]
                .into_iter()
                .map(|(name, owner)| {
                    let registration = service::register(
                        &mut conn,
                        &humans[owner],
                        proto::RegisterRequest {
                            name: Some(name.into()),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                    let caller = Caller {
                        token: Some(registration.token),
                        ..Default::default()
                    };
                    ids.push(service::whoami(&mut conn, &caller).unwrap().id);
                    caller
                })
                .collect::<Vec<_>>()
                .try_into()
                .unwrap_or_else(|_| panic!("three fixture agents"));
            Self {
                conn,
                humans,
                agents,
                ids: ids.try_into().unwrap(),
            }
        }

        fn memory() -> Self {
            Self::new(Connection::open_in_memory().unwrap())
        }

        fn put(
            &mut self,
            agent: usize,
            body: &str,
            replaces: Option<i64>,
            key: &str,
        ) -> proto::Blocker {
            put_blocker(
                &mut self.conn,
                &self.agents[agent],
                proto::PutBlockerRequest {
                    goal: None,
                    choices: None,
                    body: body.into(),
                    replaces,
                    request_key: key.into(),
                    ..Default::default()
                },
            )
            .unwrap()
            .blocker
            .unwrap()
        }

        fn answer(
            &mut self,
            human: usize,
            id: i64,
            key: &str,
        ) -> Result<proto::BlockerMutationResponse> {
            answer_blocker(
                &mut self.conn,
                &self.humans[human],
                proto::AnswerBlockerRequest {
                    selected: Vec::new(),
                    id,
                    answer: "Approved; continue with option B.".into(),
                    request_key: key.into(),
                },
            )
        }

        fn messages(&self) -> i64 {
            self.conn
                .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
                .unwrap()
        }

        fn list(&mut self) -> proto::ListBlockersResponse {
            list_blockers(
                &mut self.conn,
                &self.humans[2],
                proto::ListBlockersRequest::default(),
            )
            .unwrap()
        }
    }

    #[test]
    fn closed_listing_filters_before_paging_newest_roots_and_keeps_current_scope() {
        let mut b = Board::memory();
        let will = b.humans[0].clone();
        let mut roots = Vec::new();
        for n in 0..65 {
            let row = b.put(0, "closed question", None, &format!("closed-{n}"));
            if n % 2 == 0 {
                b.answer(0, row.id, &format!("answer-{n}")).unwrap();
            } else {
                cancel_blocker(
                    &mut b.conn,
                    &b.agents[0],
                    proto::CancelBlockerRequest {
                        id: row.id,
                        request_key: format!("cancel-{n}"),
                    },
                )
                .unwrap();
            }
            roots.push(row.id);
            b.put(0, "still open", None, &format!("open-{n}"));
        }
        // A replacement stays at its root's position, not its newer version ID.
        let replacement = b.put(
            0,
            "replaced oldest question",
            Some(roots[0]),
            "replace-oldest",
        );
        b.answer(0, replacement.id, "answer-replacement").unwrap();
        // Another owner's closed blocker stays out of will's queue.
        let hidden = put_blocker(
            &mut b.conn,
            &b.agents[2],
            proto::PutBlockerRequest {
                body: "not your queue".into(),
                request_key: "hidden".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .blocker
        .unwrap();
        b.answer(1, hidden.id, "answer-hidden").unwrap();
        let list = |b: &mut Board, before_root_id| {
            list_blockers(
                &mut b.conn,
                &will,
                proto::ListBlockersRequest {
                    mine: true,
                    closed_only: true,
                    before_root_id,
                    limit: Some(50),
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let first = list(&mut b, None);
        assert_eq!(first.blockers.len(), 50);
        assert_eq!(first.active_count, 65);
        assert!(first.has_more);
        assert_eq!(first.next_after_root_id, None);
        assert_eq!(first.next_before_root_id, Some(roots[15]));
        assert_eq!(
            first.blockers.iter().map(|r| r.root_id).collect::<Vec<_>>(),
            roots[15..].iter().rev().copied().collect::<Vec<_>>()
        );
        assert!(
            first
                .blockers
                .iter()
                .all(|r| r.outcome.is_some() && !r.can_answer && !r.can_cancel)
        );
        // A newly closed root appears on refresh, without shifting older pages.
        let fresh = b.put(0, "newest", None, "fresh");
        b.answer(0, fresh.id, "answer-fresh").unwrap();
        let second = list(&mut b, first.next_before_root_id);
        assert_eq!(second.blockers.len(), 15);
        assert!(!second.has_more);
        assert_eq!(second.next_before_root_id, None);
        assert_eq!(second.blockers.last().unwrap().id, replacement.id);
        assert_eq!(
            second
                .blockers
                .iter()
                .map(|r| r.root_id)
                .collect::<Vec<_>>(),
            roots[..15].iter().rev().copied().collect::<Vec<_>>()
        );
        assert_eq!(list(&mut b, None).blockers[0].id, fresh.id);
        let old_queue = list_blockers(
            &mut b.conn,
            &will,
            proto::ListBlockersRequest {
                mine: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            old_queue
                .blockers
                .windows(2)
                .all(|p| p[0].root_id < p[1].root_id)
        );
        assert!(
            list_blockers(
                &mut b.conn,
                &b.agents[0],
                proto::ListBlockersRequest {
                    mine: true,
                    closed_only: true,
                    ..Default::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn closed_listing_rejects_mixed_or_invalid_cursors() {
        let mut b = Board::memory();
        for req in [
            proto::ListBlockersRequest {
                closed_only: true,
                after_root_id: Some(1),
                ..Default::default()
            },
            proto::ListBlockersRequest {
                before_root_id: Some(1),
                ..Default::default()
            },
            proto::ListBlockersRequest {
                closed_only: true,
                before_root_id: Some(0),
                ..Default::default()
            },
            proto::ListBlockersRequest {
                closed_only: true,
                before_root_id: Some(-1),
                ..Default::default()
            },
        ] {
            assert!(list_blockers(&mut b.conn, &b.humans[0], req).is_err());
        }
    }

    #[test]
    fn put_size_limits_reject_new_versions_without_changing_the_chain() {
        let mut b = Board::memory();
        let body = "é".repeat(1024);
        let original = b.put(0, &body, None, "boundary");
        assert_eq!(original.body, body);
        let before = b.messages();
        for replaces in [None, Some(original.id)] {
            let req = proto::PutBlockerRequest {
                goal: None,
                body: format!("{body}x"),
                replaces,
                request_key: "oversized".into(),
                choices: None,
                ..Default::default()
            };
            let error = put_blocker(&mut b.conn, &b.agents[0], req)
                .unwrap_err()
                .to_string();
            assert!(error.contains("2 KiB (2048 UTF-8 bytes)"), "{error}");
            assert!(error.contains("received 2049 bytes"), "{error}");
            let options = ["a", "b"].map(|id| proto::BlockerOption {
                id: id.into(),
                label: id.into(),
                detail: Some("x".repeat(500)),
            });
            let req = proto::PutBlockerRequest {
                goal: None,
                body: "short".into(),
                replaces,
                request_key: "oversized".into(),
                choices: Some(proto::BlockerChoices {
                    mode: proto::BlockerChoiceMode::BlockerSelectOne.into(),
                    options: options.to_vec(),
                    min: None,
                    max: None,
                }),
                ..Default::default()
            };
            let error = put_blocker(&mut b.conn, &b.agents[0], req)
                .unwrap_err()
                .to_string();
            assert!(error.contains("1 KiB (1024 UTF-8 bytes"), "{error}");
        }
        assert_eq!(b.messages(), before);
        let listing = b.list();
        assert_eq!(listing.blockers.len(), 1);
        assert_eq!(listing.blockers[0].id, original.id);
        assert_eq!(listing.blockers[0].body, body);
        assert!(listing.blockers[0].outcome.is_none());
        assert_eq!(
            store::history(&b.conn, original.root_id, i64::MAX, 10)
                .unwrap()
                .len(),
            1
        );
        // A refused size does not consume its request key or supersede anything.
        let updated = b.put(0, "short enough", Some(original.id), "oversized");
        assert_eq!(updated.replaces, Some(original.id));
    }

    #[test]
    fn legacy_large_blockers_remain_readable_and_answer_limits_do_not_shrink() {
        let mut b = Board::memory();
        let choices = proto::BlockerChoices {
            mode: proto::BlockerChoiceMode::BlockerSelectOne.into(),
            options: ["a", "b"]
                .into_iter()
                .map(|id| proto::BlockerOption {
                    id: id.into(),
                    label: id.into(),
                    detail: Some("x".repeat(500)),
                })
                .collect(),
            min: None,
            max: None,
        };
        let json = blocker_choices::to_json(&choices);
        assert!(json.len() > crate::blocker_limits::MAX_CHOICES_BYTES);
        let body = "legacy".repeat(500);
        let now = "2026-11-02T00:00:00Z";
        let id = store::insert(
            &b.conn,
            b.ids[0],
            store::Version {
                body: &body,
                choices: Some(&json),
            },
            None,
            now,
            "legacy",
        )
        .unwrap();
        let actor = db::Principal {
            kind: db::Kind::Agent,
            id: b.ids[0],
            name: "Mateo".into(),
        };
        store::event(&b.conn, id, "created", &actor, now, None, None, None).unwrap();
        let got = get_blocker(
            &mut b.conn,
            &b.humans[0],
            proto::GetBlockerRequest {
                id,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(got.blocker.unwrap().body, body);
        assert_eq!(b.list().blockers[0].choices.as_ref(), Some(&choices));
        let answer = "y".repeat(8192);
        let result = answer_blocker(
            &mut b.conn,
            &b.humans[0],
            proto::AnswerBlockerRequest {
                id,
                answer: answer.clone(),
                request_key: "legacy-answer".into(),
                selected: vec![],
            },
        )
        .unwrap();
        assert_eq!(
            result.blocker.unwrap().outcome.unwrap().answer,
            Some(answer)
        );
    }

    #[test]
    fn personal_queue_filters_before_paging_and_tracks_owned_agents() {
        let mut b = Board::memory();
        let list = |b: &mut Board, who: &Caller, after: Option<i64>, closed: bool| {
            list_blockers(
                &mut b.conn,
                who,
                proto::ListBlockersRequest {
                    mine: true,
                    limit: Some(1),
                    after_root_id: after,
                    include_closed: closed,
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let will = b.humans[0].clone();
        let ada = b.humans[1].clone();
        let zed = b.agents[1].clone();
        let other = b.put(2, "oldest but not mine", None, "other");
        let mateo = b.put(0, "Mateo's question", None, "mateo");
        let zeds = b.put(1, "Zed's question", None, "zed");
        // Both owned agents, paged in root order, and nobody else's.
        let first = list(&mut b, &will, None, false);
        assert_eq!(first.active_count, 2);
        assert_eq!(
            first.blockers.iter().map(|b| b.id).collect::<Vec<_>>(),
            [mateo.id]
        );
        assert!(first.has_more);
        assert!(first.blockers[0].can_answer && first.blockers[0].can_cancel);
        let second = list(&mut b, &will, first.next_after_root_id, false);
        assert_eq!(second.blockers[0].id, zeds.id);
        assert!(!second.has_more);
        let theirs = list(&mut b, &ada, None, false);
        assert_eq!(theirs.active_count, 1);
        assert_eq!(theirs.blockers[0].id, other.id);
        // Replace in place: only the open head is returned at its original age.
        let current = b.put(1, "updated question", Some(zeds.id), "replace-personal");
        let second = list(&mut b, &will, first.next_after_root_id, false);
        assert_eq!(second.active_count, 2);
        assert_eq!(second.blockers[0].id, current.id);
        assert_eq!(second.blockers[0].root_id, zeds.root_id);
        // A retired creator's blocker stays with its owner, to cancel.
        service::bye(&mut b.conn, &zed).unwrap();
        let second = list(&mut b, &will, first.next_after_root_id, false);
        assert_eq!(second.active_count, 2);
        assert!(!second.blockers[0].can_answer && second.blockers[0].can_cancel);
        cancel_blocker(
            &mut b.conn,
            &will,
            proto::CancelBlockerRequest {
                id: current.id,
                request_key: "cancel-personal".into(),
            },
        )
        .unwrap();
        assert_eq!(list(&mut b, &will, None, false).active_count, 1);
        let closed = list(&mut b, &will, first.next_after_root_id, true);
        assert_eq!(
            closed.blockers[0].status(),
            proto::BlockerStatus::BlockerCancelled
        );
        // Board-wide reads remain board-wide.
        assert_eq!(b.list().active_count, 2);
    }

    #[test]
    fn personal_queue_is_human_only_and_exclusive_with_other_scopes() {
        let mut b = Board::memory();
        b.put(0, "help", None, "personal-validation");
        let req = proto::ListBlockersRequest {
            mine: true,
            agent: Some("Mateo".into()),
            ..Default::default()
        };
        assert!(
            list_blockers(&mut b.conn, &b.humans[0], req)
                .unwrap_err()
                .to_string()
                .contains("mutually exclusive")
        );
        // Old clients that still list by group hear that groups are gone.
        let req = proto::ListBlockersRequest {
            group: Some("crew".into()),
            ..Default::default()
        };
        assert!(
            list_blockers(&mut b.conn, &b.humans[0], req)
                .unwrap_err()
                .to_string()
                .contains("no groups")
        );
        assert!(
            list_blockers(
                &mut b.conn,
                &b.agents[0],
                proto::ListBlockersRequest {
                    mine: true,
                    ..Default::default()
                }
            )
            .unwrap_err()
            .to_string()
            .contains("human")
        );
        // A human who owns no agents gets an empty personal queue, not the board.
        let empty = list_blockers(
            &mut b.conn,
            &b.humans[2],
            proto::ListBlockersRequest {
                mine: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(empty.active_count, 0);
        assert!(empty.blockers.is_empty());
        assert_eq!(b.list().active_count, 1);
    }

    #[test]
    fn agent_counts_match_conversation_scope_through_the_blocker_lifecycle() {
        let mut b = Board::memory();
        let check = |b: &mut Board, expected: [u64; 3]| {
            let agents = service::list_agents(
                &mut b.conn,
                &b.humans[0],
                proto::ListAgentsRequest::default(),
            )
            .unwrap()
            .agents;
            let summaries =
                service::agent_summary(&mut b.conn, &b.humans[0], proto::AgentSummaryRequest {})
                    .unwrap()
                    .agents;
            for (name, expected) in ["Mateo", "Zed", "Lu"].into_iter().zip(expected) {
                assert_eq!(
                    agents
                        .iter()
                        .find(|a| a.name == name)
                        .unwrap()
                        .blocker_count,
                    Some(expected),
                    "list {name}"
                );
                assert_eq!(
                    summaries
                        .iter()
                        .find(|a| a.agent == name)
                        .unwrap()
                        .blocker_count,
                    Some(expected),
                    "summary {name}"
                );
                let scope = list_blockers(
                    &mut b.conn,
                    &b.humans[0],
                    proto::ListBlockersRequest {
                        agent: Some(name.into()),
                        limit: Some(1),
                        ..Default::default()
                    },
                )
                .unwrap();
                assert_eq!(scope.active_count, expected, "conversation {name}");
            }
        };
        check(&mut b, [0, 0, 0]);
        let a = b.put(0, "a", None, "a");
        check(&mut b, [1, 0, 0]);
        let a = b.put(0, "updated a", Some(a.id), "a2");
        check(&mut b, [1, 0, 0]); // one open version, not two
        let c = b.put(0, "c", None, "c");
        check(&mut b, [2, 0, 0]); // count all, not just the returned page
        b.answer(0, a.id, "answer").unwrap();
        check(&mut b, [1, 0, 0]);
        cancel_blocker(
            &mut b.conn,
            &b.agents[1],
            proto::CancelBlockerRequest {
                id: c.id,
                request_key: "cancel".into(),
            },
        )
        .unwrap();
        check(&mut b, [0, 0, 0]);
        // Each agent counts the blockers it created, whoever owns it.
        b.put(1, "Zed's issue", None, "zed");
        b.put(2, "Lu's issue", None, "lu");
        check(&mut b, [0, 1, 1]);
    }

    #[test]
    fn the_summary_revision_grows_on_every_blocker_change_and_only_then() {
        let mut b = Board::memory();
        let revision = |b: &mut Board| {
            service::agent_summary(&mut b.conn, &b.humans[0], proto::AgentSummaryRequest {})
                .unwrap()
                .blockers_revision
                .unwrap()
        };
        let mut last = revision(&mut b);
        assert_eq!(last, 0);
        let mut grew = |b: &mut Board| {
            let now = revision(b);
            let grew = now > last;
            last = now;
            grew
        };
        let a = b.put(0, "a", None, "a");
        assert!(grew(&mut b), "created");
        let a = b.put(0, "updated a", Some(a.id), "a2");
        assert!(grew(&mut b), "superseded");
        b.answer(0, a.id, "answer").unwrap();
        assert!(grew(&mut b), "answered");
        let c = b.put(0, "c", None, "c");
        assert!(grew(&mut b), "created again");
        cancel_blocker(
            &mut b.conn,
            &b.agents[0],
            proto::CancelBlockerRequest {
                id: c.id,
                request_key: "cancel".into(),
            },
        )
        .unwrap();
        assert!(grew(&mut b), "cancelled");
        service::set_status(
            &mut b.conn,
            &b.agents[0],
            proto::SetStatusRequest {
                text: "busy".into(),
            },
        )
        .unwrap();
        assert!(!grew(&mut b), "an unrelated write");
    }

    #[test]
    fn retired_agent_counts_do_not_inherit_a_reused_names_blockers() {
        let mut b = Board::memory();
        b.put(0, "original agent", None, "old");
        service::bye(&mut b.conn, &b.agents[0]).unwrap();
        crate::db::date_retired_entity_ids(&b.conn);
        let registered = service::register(
            &mut b.conn,
            &b.humans[0],
            proto::RegisterRequest {
                name: Some("Mateo".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let mateo = Caller {
            token: Some(registered.token),
            ..Default::default()
        };
        put_blocker(
            &mut b.conn,
            &mateo,
            proto::PutBlockerRequest {
                goal: None,
                choices: None,
                body: "new agent".into(),
                request_key: "new".into(),
                replaces: None,
                ..Default::default()
            },
        )
        .unwrap();
        let agents = service::list_agents(
            &mut b.conn,
            &b.humans[0],
            proto::ListAgentsRequest { all: true },
        )
        .unwrap()
        .agents;
        let reused: Vec<_> = agents.iter().filter(|a| a.name == "Mateo").collect();
        assert_eq!(reused.len(), 2);
        assert_eq!(
            reused.iter().find(|a| a.retired).unwrap().blocker_count,
            Some(0)
        );
        assert_eq!(
            reused.iter().find(|a| !a.retired).unwrap().blocker_count,
            Some(1)
        );
        let summaries =
            service::agent_summary(&mut b.conn, &b.humans[0], proto::AgentSummaryRequest {})
                .unwrap()
                .agents;
        assert_eq!(summaries.iter().filter(|s| s.agent == "Mateo").count(), 1);
        assert_eq!(
            summaries
                .iter()
                .find(|s| s.agent == "Mateo")
                .unwrap()
                .blocker_count,
            Some(1)
        );
    }

    #[test]
    fn outcome_actors_carry_their_owner_like_creators_do() {
        let mut b = Board::memory();
        let owner_of = |b: &mut Board, id: i64| {
            let outcome = get_blocker(
                &mut b.conn,
                &b.humans[2],
                proto::GetBlockerRequest {
                    id,
                    ..Default::default()
                },
            )
            .unwrap()
            .blocker
            .unwrap()
            .outcome
            .unwrap();
            let actor = outcome.actor.unwrap();
            (actor.name, actor.owner)
        };
        // Lu, ada's agent, supersedes its blocker; Zed, will's, cancels Mateo's.
        let first = b.put(2, "Which option?", None, "first");
        b.put(2, "Which option, now with costs?", Some(first.id), "second");
        assert_eq!(
            owner_of(&mut b, first.id),
            ("Lu".into(), Some("ada".into()))
        );
        let second = b.put(0, "Which branch?", None, "branch");
        cancel_blocker(
            &mut b.conn,
            &b.agents[1],
            proto::CancelBlockerRequest {
                id: second.id,
                request_key: "cancel".into(),
            },
        )
        .unwrap();
        assert_eq!(
            owner_of(&mut b, second.id),
            ("Zed".into(), Some("will".into()))
        );
        // A human actor has no owner.
        let third = b.put(0, "Ship today?", None, "third");
        b.answer(0, third.id, "answer").unwrap();
        assert_eq!(owner_of(&mut b, third.id), ("will".into(), None));
    }

    #[test]
    fn blockers_keep_immutable_versions_and_answer_once_to_the_current_creator() {
        let mut b = Board::memory();
        let before = b.messages();
        let old = b.put(0, "  Need a choice\nOption A or B?", None, "first");
        let next = b.put(
            0,
            "Need a choice, with updated estimates",
            Some(old.id),
            "replace",
        );
        assert_eq!(b.messages(), before);
        assert_eq!(next.root_id, old.id);
        assert_eq!(next.replaces, Some(old.id));
        assert_eq!(next.creator.as_ref().unwrap().id, b.ids[0]);
        assert_eq!(b.list().active_count, 1);
        assert!(b.answer(0, old.id, "stale").is_err());
        assert_eq!(b.messages(), before);
        let answered = b.answer(0, next.id, "answer").unwrap();
        let again = b.answer(0, next.id, "answer").unwrap();
        assert!(again.replayed);
        assert_eq!(answered.message_id, again.message_id);
        assert_eq!(b.messages(), before + 1);
        assert_eq!(b.list().active_count, 0);
        let recipient: i64 = b
            .conn
            .query_row(
                "SELECT b.agent_id FROM deliveries d
             JOIN entity_addresses a ON a.id = d.recipient_address_id
             JOIN entity_local_bindings b ON b.entity_id = a.entity_id
             WHERE d.message_id = ?1 AND b.agent_id IS NOT NULL",
                [answered.message_id.unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(recipient, b.ids[0]);
        let history = get_blocker(
            &mut b.conn,
            &b.humans[2],
            proto::GetBlockerRequest {
                id: old.id,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(history.history.len(), 2);
        assert_eq!(history.blocker.unwrap().body, old.body);
        assert_eq!(
            history.history[1].status(),
            proto::BlockerStatus::BlockerSuperseded
        );
        assert_eq!(
            history.history[0]
                .outcome
                .as_ref()
                .unwrap()
                .answer
                .as_deref(),
            Some("Approved; continue with option B.")
        );
        assert!(
            b.conn
                .execute(
                    "UPDATE blockers SET body = 'edited' WHERE id = ?1",
                    [old.id]
                )
                .is_err()
        );
        assert!(
            b.conn
                .execute("DELETE FROM blockers WHERE id = ?1", [old.id])
                .is_err()
        );
        assert!(b.conn.execute("DELETE FROM blocker_events", []).is_err());
        let reopened = b.put(
            0,
            "The answer left a second issue",
            Some(next.id),
            "new-version",
        );
        assert_eq!(reopened.root_id, old.id);
        assert_eq!(b.list().active_count, 1);
        assert_eq!(
            store::get(&b.conn, next.id)
                .unwrap()
                .unwrap()
                .outcome
                .unwrap()
                .kind,
            "answered"
        );
    }

    #[test]
    fn only_the_owner_answers_and_only_the_creator_replaces() {
        let mut b = Board::memory();
        let first = b.put(0, "Need help", None, "first");
        // Neither another owner, a human who owns no agents, nor an agent answers.
        assert!(b.answer(1, first.id, "other-owner").is_err());
        assert!(b.answer(2, first.id, "no-agents").is_err());
        assert!(
            answer_blocker(
                &mut b.conn,
                &b.agents[0],
                proto::AnswerBlockerRequest {
                    selected: Vec::new(),
                    id: first.id,
                    answer: "Pretend to be human".into(),
                    request_key: "wrong-kind".into(),
                }
            )
            .is_err()
        );
        assert!(
            put_blocker(
                &mut b.conn,
                &b.humans[0],
                proto::PutBlockerRequest {
                    body: "human create".into(),
                    request_key: "human".into(),
                    ..Default::default()
                }
            )
            .is_err()
        );
        // Even an agent with the same owner cannot replace another's blocker.
        let error = put_blocker(
            &mut b.conn,
            &b.agents[1],
            proto::PutBlockerRequest {
                body: "Updated by a sibling".into(),
                replaces: Some(first.id),
                request_key: "replace".into(),
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("creator can replace"), "{error}");
        // A group named by an old client is ignored.
        let grouped = put_blocker(
            &mut b.conn,
            &b.agents[2],
            proto::PutBlockerRequest {
                group: "no-such-group".into(),
                body: "Still mine".into(),
                request_key: "grouped".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .blocker
        .unwrap();
        assert!(grouped.group.is_empty());
        assert!(b.answer(1, grouped.id, "own-agent").is_ok());
        // An agent without an owner has nobody to answer it.
        b.conn
            .execute("DELETE FROM agent_owners WHERE agent_id = ?1", [b.ids[2]])
            .unwrap();
        let error = put_blocker(
            &mut b.conn,
            &b.agents[2],
            proto::PutBlockerRequest {
                body: "Anyone?".into(),
                request_key: "orphan".into(),
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("with an owner"), "{error}");
        assert!(b.answer(0, first.id, "owner").is_ok());
        // Reading is board-wide for an authenticated human or agent.
        assert!(
            get_blocker(
                &mut b.conn,
                &b.agents[2],
                proto::GetBlockerRequest {
                    id: first.id,
                    ..Default::default()
                }
            )
            .is_ok()
        );
        assert!(
            list_blockers(
                &mut b.conn,
                &Caller::default(),
                proto::ListBlockersRequest::default()
            )
            .is_err()
        );
    }

    #[test]
    fn the_creator_its_owner_and_its_owners_agents_cancel() {
        let mut b = Board::memory();
        let cancel = |b: &mut Board, by: &Caller, id: i64, key: &str| {
            cancel_blocker(
                &mut b.conn,
                by,
                proto::CancelBlockerRequest {
                    id,
                    request_key: key.into(),
                },
            )
        };
        let first = b.put(0, "Which branch?", None, "first");
        for (by, key) in [
            (b.humans[1].clone(), "ada"),
            (b.humans[2].clone(), "external"),
            (b.agents[2].clone(), "lu"),
        ] {
            let error = cancel(&mut b, &by, first.id, key).unwrap_err().to_string();
            assert!(error.contains("its owner"), "{error}");
        }
        let by = b.agents[1].clone();
        assert!(cancel(&mut b, &by, first.id, "zed").is_ok());
        let second = b.put(0, "Which tag?", None, "second");
        let by = b.humans[0].clone();
        assert!(cancel(&mut b, &by, second.id, "will").is_ok());
        let third = b.put(0, "Which day?", None, "third");
        let by = b.agents[0].clone();
        assert!(cancel(&mut b, &by, third.id, "mateo").is_ok());
        // Once cancelled, nobody cancels or answers it.
        let blocker = get_blocker(
            &mut b.conn,
            &b.agents[1],
            proto::GetBlockerRequest {
                id: first.id,
                ..Default::default()
            },
        )
        .unwrap()
        .blocker
        .unwrap();
        assert!(!blocker.can_cancel && !blocker.can_answer);
    }

    #[test]
    fn blocker_receipts_refuse_payload_or_operation_reuse_and_replay_without_new_mail() {
        let mut b = Board::memory();
        let request = proto::PutBlockerRequest {
            goal: None,
            choices: None,
            body: "Original".into(),
            request_key: "key".into(),
            ..Default::default()
        };
        let one = put_blocker(&mut b.conn, &b.agents[0], request.clone()).unwrap();
        let replay = put_blocker(&mut b.conn, &b.agents[0], request.clone()).unwrap();
        assert!(replay.replayed);
        assert_eq!(one.blocker, replay.blocker);
        let mut different = request;
        different.body = "Different".into();
        assert!(put_blocker(&mut b.conn, &b.agents[0], different).is_err());
        let id = one.blocker.unwrap().id;
        assert!(
            cancel_blocker(
                &mut b.conn,
                &b.agents[0],
                proto::CancelBlockerRequest {
                    id,
                    request_key: "key".into()
                }
            )
            .is_err()
        );
        let before = b.messages();
        let cancel = proto::CancelBlockerRequest {
            id,
            request_key: "cancel".into(),
        };
        let one = cancel_blocker(&mut b.conn, &b.humans[0], cancel.clone()).unwrap();
        let replay = cancel_blocker(&mut b.conn, &b.humans[0], cancel).unwrap();
        assert!(replay.replayed);
        assert_eq!(one.blocker, replay.blocker);
        // One cancel notice to the creator; the replay names it and sends no more.
        assert!(one.message_id.is_some());
        assert_eq!(replay.message_id, one.message_id);
        assert_eq!(b.messages(), before + 1);
        assert!(b.answer(0, id, "new-answer").is_err());
        let new = b.put(0, "Need help again", Some(id), "another");
        let before = b.messages();
        let cancel = proto::CancelBlockerRequest {
            id: new.id,
            request_key: "another-cancel".into(),
        };
        let first = cancel_blocker(&mut b.conn, &b.agents[1], cancel.clone()).unwrap();
        assert!(
            first.message_id.is_some(),
            "the creator hears of the cancel"
        );
        let replay = cancel_blocker(&mut b.conn, &b.agents[1], cancel).unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.message_id, first.message_id);
        assert_eq!(b.messages(), before + 1);
    }

    /// The cancel notices the creator received, newest last.
    fn cancel_notices(b: &Board, creator: i64) -> Vec<String> {
        let mut stmt = b
            .conn
            .prepare(
                "SELECT m.body FROM messages m JOIN deliveries d ON d.message_id = m.id
                 JOIN entity_addresses a ON a.id = d.recipient_address_id
                 JOIN entity_local_bindings b ON b.entity_id = a.entity_id
                 WHERE b.agent_id = ?1
                   AND m.body LIKE 'Blocker #% was cancelled by %' ORDER BY m.id",
            )
            .unwrap();
        stmt.query_map([creator], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn cancelling_tells_the_creator_unless_it_cancelled_its_own() {
        let mut b = Board::memory();
        let cancel = |b: &mut Board, by: &Caller, id: i64, key: &str| {
            cancel_blocker(
                &mut b.conn,
                by,
                proto::CancelBlockerRequest {
                    id,
                    request_key: key.into(),
                },
            )
            .unwrap()
        };
        // Its own: no notice.
        let own = b.put(0, "Which branch?", None, "own");
        let by = b.agents[0].clone();
        assert!(cancel(&mut b, &by, own.id, "c-own").message_id.is_none());
        assert!(cancel_notices(&b, b.ids[0]).is_empty());
        // A sibling agent, then the owner.
        let a = b.put(0, "Which branch?", None, "a");
        let by = b.agents[1].clone();
        cancel(&mut b, &by, a.id, "c-a");
        let c = b.put(0, "Ship it?", None, "c");
        let by = b.humans[0].clone();
        let notice = cancel(&mut b, &by, c.id, "c-c").message_id.unwrap();
        assert_eq!(
            cancel_notices(&b, b.ids[0]),
            vec![
                // The blocker is cited by ID, not quoted.
                format!("Blocker #{} was cancelled by Zed.", a.id),
                format!("Blocker #{} was cancelled by will.", c.id),
            ]
        );
        let sender: String = b
            .conn
            .query_row(
                "SELECT CASE WHEN b.agent_id IS NULL THEN 'human' ELSE 'agent' END
                 FROM messages m JOIN entity_addresses a ON a.id = m.sender_address_id
                 JOIN entity_local_bindings b ON b.entity_id = a.entity_id WHERE m.id = ?1",
                [notice],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sender, "human");
        // The outcome names the notice, so the creator's runner can wait for
        // it; with no notice it names none.
        let mail = |b: &mut Board, id: i64| {
            get_blocker(
                &mut b.conn,
                &b.agents[0],
                proto::GetBlockerRequest {
                    id,
                    ..Default::default()
                },
            )
            .unwrap()
            .current
            .unwrap()
            .outcome
            .unwrap()
            .message_id
        };
        assert_eq!(mail(&mut b, c.id), Some(notice));
        assert_eq!(mail(&mut b, own.id), None);
        // The event stores the notice; its own cancel stores none.
        let stored = |b: &Board, id: i64| -> Option<i64> {
            b.conn
                .query_row(
                    "SELECT message_id FROM blocker_events WHERE blocker_id = ?1 AND kind = 'cancelled'",
                    [id],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(stored(&b, c.id), Some(notice));
        assert_eq!(stored(&b, own.id), None);
        // In focus mode only the owner reaches the creator.
        service::set_focus(
            &mut b.conn,
            &b.humans[0],
            proto::SetFocusRequest {
                agent: "Mateo".into(),
                focused: true,
            },
        )
        .unwrap();
        let d = b.put(0, "Tag it?", None, "d");
        let by = b.agents[1].clone();
        assert!(cancel(&mut b, &by, d.id, "c-d").message_id.is_none());
        assert_eq!(mail(&mut b, d.id), None);
        let e = b.put(0, "Merge it?", None, "e");
        let by = b.humans[0].clone();
        assert!(cancel(&mut b, &by, e.id, "c-e").message_id.is_some());
        let notices = cancel_notices(&b, b.ids[0]);
        assert_eq!(notices.len(), 3);
        assert!(notices[2].starts_with(&format!("Blocker #{} was cancelled by will.", e.id)));
    }

    #[test]
    fn blocker_answer_and_mail_roll_back_together_when_outcome_cannot_commit() {
        let mut b = Board::memory();
        let first = b.put(0, "Needs approval", None, "put");
        let before = b.messages();
        b.conn
            .execute_batch(
                "CREATE TRIGGER refuse_answer BEFORE INSERT ON blocker_events
            WHEN NEW.kind = 'answered' BEGIN SELECT RAISE(ABORT, 'fixture refusal'); END;",
            )
            .unwrap();
        assert!(b.answer(0, first.id, "answer").is_err());
        assert_eq!(b.messages(), before);
        assert_eq!(b.list().active_count, 1);
        assert!(
            store::operation(
                &b.conn,
                &db::find_human(&b.conn, "will").unwrap().unwrap(),
                "answer",
            )
            .unwrap()
            .is_none()
        );
        b.conn.execute_batch("DROP TRIGGER refuse_answer").unwrap();
        b.answer(0, first.id, "answer").unwrap();
        assert_eq!(b.messages(), before + 1);
    }

    #[test]
    fn blocker_scope_counts_and_paging_preserve_original_queue_position() {
        let mut b = Board::memory();
        let one = b.put(0, "one", None, "one");
        let two = b.put(1, "two", None, "two");
        let new_one = b.put(0, "new one", Some(one.id), "new-one");
        let page = list_blockers(
            &mut b.conn,
            &b.humans[2],
            proto::ListBlockersRequest {
                limit: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.active_count, 2);
        assert!(page.has_more);
        assert_eq!(page.blockers[0].id, new_one.id);
        assert_eq!(page.next_after_root_id, Some(one.id));
        let next = list_blockers(
            &mut b.conn,
            &b.agents[2],
            proto::ListBlockersRequest {
                after_root_id: page.next_after_root_id,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            next.blockers.iter().map(|x| x.id).collect::<Vec<_>>(),
            [two.id]
        );
        assert_eq!(next.active_count, 2);
        let empty = list_blockers(
            &mut b.conn,
            &b.humans[0],
            proto::ListBlockersRequest {
                agent: Some("Lu".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(empty.active_count, 0);
        assert!(empty.blockers.is_empty());
        b.answer(0, two.id, "answer").unwrap();
        let closed = list_blockers(
            &mut b.conn,
            &b.agents[2],
            proto::ListBlockersRequest {
                include_closed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(closed.active_count, 1);
        assert_eq!(closed.blockers.len(), 2);
        let history = get_blocker(
            &mut b.conn,
            &b.agents[2],
            proto::GetBlockerRequest {
                id: new_one.id,
                limit: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(history.history[0].id, new_one.id);
        assert!(history.has_more);
        let history = get_blocker(
            &mut b.conn,
            &b.agents[2],
            proto::GetBlockerRequest {
                id: new_one.id,
                limit: Some(1),
                before_id: history.next_before_id,
            },
        )
        .unwrap();
        assert_eq!(history.history[0].id, one.id);
        assert!(!history.has_more);
    }

    #[test]
    fn retired_creators_keep_history_without_name_reuse_delivery() {
        let mut b = Board::memory();
        let first = b.put(0, "Need help", None, "put");
        service::bye(&mut b.conn, &b.agents[0]).unwrap();
        crate::db::date_retired_entity_ids(&b.conn);
        service::register(
            &mut b.conn,
            &b.humans[0],
            proto::RegisterRequest {
                name: Some("Mateo".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let before = b.messages();
        assert!(b.answer(0, first.id, "answer").is_err());
        assert_eq!(b.messages(), before);
        let get = get_blocker(
            &mut b.conn,
            &b.humans[2],
            proto::GetBlockerRequest {
                id: first.id,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(get.blocker.unwrap().creator.unwrap().id, b.ids[0]);
        cancel_blocker(
            &mut b.conn,
            &b.humans[0],
            proto::CancelBlockerRequest {
                id: first.id,
                request_key: "cancel".into(),
            },
        )
        .unwrap();
        assert_eq!(b.messages(), before);
        let history = list_blockers(
            &mut b.conn,
            &b.humans[0],
            proto::ListBlockersRequest {
                mine: true,
                include_closed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(history.blockers.len(), 1);
        assert_eq!(history.active_count, 0);
        // The reused name's agent scope does not inherit the retired agent's blocker.
        let reused = list_blockers(
            &mut b.conn,
            &b.humans[0],
            proto::ListBlockersRequest {
                agent: Some("Mateo".into()),
                include_closed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(reused.blockers.is_empty());
    }

    #[test]
    fn blocker_bounds_invalid_scopes_and_stale_replacement_leave_history_unchanged() {
        let mut b = Board::memory();
        let first = b.put(0, &"界".repeat(682), None, "first");
        for body in [" \n\t".into(), "界".repeat(683)] {
            assert!(
                put_blocker(
                    &mut b.conn,
                    &b.agents[0],
                    proto::PutBlockerRequest {
                        goal: None,
                        choices: None,
                        body,
                        request_key: "invalid".into(),
                        ..Default::default()
                    }
                )
                .is_err()
            );
        }
        assert!(
            list_blockers(
                &mut b.conn,
                &b.humans[0],
                proto::ListBlockersRequest {
                    limit: Some(0),
                    ..Default::default()
                }
            )
            .is_err()
        );
        let next = b.put(0, "replacement", Some(first.id), "replace");
        let before = b
            .conn
            .query_row("SELECT COUNT(*) FROM blocker_events", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap();
        assert!(
            put_blocker(
                &mut b.conn,
                &b.agents[0],
                proto::PutBlockerRequest {
                    goal: None,
                    choices: None,
                    body: "fork a chain".into(),
                    replaces: Some(first.id),
                    request_key: "fork".into(),
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert_eq!(
            b.conn
                .query_row("SELECT COUNT(*) FROM blocker_events", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            before
        );
        assert_eq!(b.list().blockers[0].id, next.id);
    }

    #[test]
    fn committed_blocker_retries_survive_the_creator_retiring() {
        let mut b = Board::memory();
        let first = b.put(0, "Need approval", None, "put");
        let answered = b.answer(0, first.id, "answer").unwrap();
        let second = b.put(0, "Cancel before retiring", None, "second");
        let request = proto::CancelBlockerRequest {
            id: second.id,
            request_key: "cancel".into(),
        };
        let cancelled = cancel_blocker(&mut b.conn, &b.humans[0], request.clone()).unwrap();
        service::bye(&mut b.conn, &b.agents[0]).unwrap();
        let before = b.messages();
        let replay = b.answer(0, first.id, "answer").unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.message_id, answered.message_id);
        assert!(b.answer(0, first.id, "different-answer").is_err());
        let replay = cancel_blocker(&mut b.conn, &b.humans[0], request).unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.message_id, cancelled.message_id);
        assert_eq!(
            replay.blocker.unwrap().status(),
            proto::BlockerStatus::BlockerCancelled
        );
        assert_eq!(b.messages(), before);
    }

    #[test]
    fn concurrent_blocker_answers_commit_exactly_one_response_and_one_mail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bbs.db");
        let mut b = Board::new(db::open(&path).unwrap());
        let first = b.put(0, "Need a single decision", None, "put");
        let before = b.messages();
        let connections = [db::open(&path).unwrap(), db::open(&path).unwrap()];
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles = connections
            .into_iter()
            .enumerate()
            .map(|(i, mut conn)| {
                let barrier = barrier.clone();
                // The owner answers twice at once, as from two tabs.
                let caller = b.humans[0].clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    answer_blocker(
                        &mut conn,
                        &caller,
                        proto::AnswerBlockerRequest {
                            selected: Vec::new(),
                            id: first.id,
                            answer: format!("Answer from {i}"),
                            request_key: format!("answer-{i}"),
                        },
                    )
                    .is_ok()
                })
            })
            .collect::<Vec<_>>();
        let successes = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|ok| *ok)
            .count();
        assert_eq!(successes, 1);
        assert_eq!(b.messages(), before + 1);
        assert_eq!(b.list().active_count, 0);
    }

    fn choices(mode: proto::BlockerChoiceMode, ids: &[&str]) -> proto::BlockerChoices {
        proto::BlockerChoices {
            mode: mode.into(),
            options: ids
                .iter()
                .map(|id| proto::BlockerOption {
                    id: id.to_string(),
                    label: format!("Use {id}"),
                    detail: None,
                })
                .collect(),
            min: None,
            max: None,
        }
    }

    fn put_choices(
        b: &mut Board,
        choices: proto::BlockerChoices,
        replaces: Option<i64>,
        key: &str,
    ) -> Result<proto::BlockerMutationResponse> {
        put_blocker(
            &mut b.conn,
            &b.agents[0],
            proto::PutBlockerRequest {
                goal: None,
                body: "Which plan?".into(),
                replaces,
                request_key: key.into(),
                choices: Some(choices),
                ..Default::default()
            },
        )
    }

    fn select(
        b: &mut Board,
        id: i64,
        selected: &[&str],
        note: &str,
        key: &str,
    ) -> Result<proto::BlockerMutationResponse> {
        answer_blocker(
            &mut b.conn,
            &b.humans[0],
            proto::AnswerBlockerRequest {
                id,
                answer: note.into(),
                request_key: key.into(),
                selected: selected.iter().map(|s| s.to_string()).collect(),
            },
        )
    }

    fn mail(b: &Board, message_id: i64) -> String {
        b.conn
            .query_row(
                "SELECT body FROM messages WHERE id = ?1",
                [message_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn blocker_choices_are_validated_stored_canonically_and_replayed() {
        use proto::BlockerChoiceMode::*;
        let mut b = Board::memory();
        let before = b.messages();
        let blockers = |b: &Board| -> i64 {
            b.conn
                .query_row("SELECT COUNT(*) FROM blockers", [], |r| r.get(0))
                .unwrap()
        };
        // The server applies the same validator as the harness.
        assert!(
            put_choices(&mut b, choices(BlockerSelectOne, &["a"]), None, "few")
                .unwrap_err()
                .to_string()
                .contains("2 to 8 options")
        );
        assert!(put_choices(&mut b, choices(Unspecified, &["a", "b"]), None, "mode").is_err());
        assert_eq!(blockers(&b), 0);

        let many = put_choices(
            &mut b,
            choices(BlockerSelectMany, &["a", "b", "c"]),
            None,
            "k",
        )
        .unwrap()
        .blocker
        .unwrap();
        let stored = many.choices.clone().unwrap();
        assert_eq!((stored.min, stored.max), (Some(1), Some(3)));
        assert_eq!(stored.options[2].label, "Use c");
        // A retry with the defaults spelled out is the same canonical request.
        let explicit = proto::BlockerChoices {
            min: Some(1),
            max: Some(3),
            ..choices(BlockerSelectMany, &["a", "b", "c"])
        };
        assert!(put_choices(&mut b, explicit, None, "k").unwrap().replayed);
        let changed = choices(BlockerSelectOne, &["a", "b", "c"]);
        assert!(put_choices(&mut b, changed, None, "k").is_err());
        assert_eq!(blockers(&b), 1);
        // Free-text blockers take no selection; choice blockers take free text.
        let plain = b.put(0, "Plain question", None, "plain");
        assert!(
            select(&mut b, plain.id, &["a"], "", "plain-pick")
                .unwrap_err()
                .to_string()
                .contains("no choices")
        );
        assert!(select(&mut b, many.id, &[], " ", "blank").is_err());
        assert!(select(&mut b, many.id, &["a", "zz"], "", "unknown").is_err());
        assert_eq!(b.messages(), before);

        let answered = select(&mut b, many.id, &["c", "a"], "Start with c.", "pick").unwrap();
        let outcome = answered.blocker.unwrap().outcome.unwrap();
        assert_eq!(outcome.selected, ["a", "c"]);
        let text = "Selected: Use a [a]\nSelected: Use c [c]\n\nStart with c.";
        assert_eq!(outcome.answer.as_deref(), Some(text));
        let body = mail(&b, answered.message_id.unwrap());
        assert!(
            body.starts_with(&format!("Blocker #{} was answered by ", many.id))
                && body.ends_with(&format!(".\n\nAnswer:\n{text}"))
                && !body.contains("Which plan?"),
            "{body}"
        );
        // The same selection in another order replays without new mail.
        let again = select(&mut b, many.id, &["a", "c"], "Start with c.", "pick").unwrap();
        assert!(again.replayed);
        assert_eq!(again.message_id, answered.message_id);
        assert!(select(&mut b, many.id, &["a"], "Start with c.", "pick").is_err());
        assert_eq!(b.messages(), before + 1);

        // A replacement may change the options; the old version's are stale.
        let one = put_choices(&mut b, choices(BlockerSelectOne, &["x", "y"]), None, "one")
            .unwrap()
            .blocker
            .unwrap();
        let next = put_choices(
            &mut b,
            choices(BlockerSelectOne, &["y", "z"]),
            Some(one.id),
            "next",
        )
        .unwrap()
        .blocker
        .unwrap();
        assert!(select(&mut b, one.id, &["x"], "", "stale").is_err());
        assert!(select(&mut b, next.id, &["x"], "", "gone").is_err());
        assert!(select(&mut b, next.id, &["y", "z"], "", "two").is_err());
        let picked = select(&mut b, next.id, &["z"], "", "z").unwrap();
        let outcome = picked.blocker.unwrap().outcome.unwrap();
        assert_eq!(
            (outcome.selected, outcome.answer.as_deref()),
            (vec!["z".to_string()], Some("Selected: Use z [z]"))
        );

        // A text-only answer to a choice blocker stores no selection.
        let last = put_choices(&mut b, choices(BlockerSelectOne, &["p", "q"]), None, "last")
            .unwrap()
            .blocker
            .unwrap();
        let free = b.answer(0, last.id, "free").unwrap().blocker.unwrap();
        let outcome = free.outcome.unwrap();
        assert!(outcome.selected.is_empty());
        assert_eq!(
            outcome.answer.as_deref(),
            Some("Approved; continue with option B.")
        );
    }

    fn propose(
        b: &mut Board,
        goal: proto::BlockerGoal,
        choices: Option<proto::BlockerChoices>,
        key: &str,
    ) -> Result<proto::BlockerMutationResponse> {
        put_blocker(
            &mut b.conn,
            &b.agents[0],
            proto::PutBlockerRequest {
                body: "I can own the release.".into(),
                replaces: None,
                request_key: key.into(),
                choices,
                goal: Some(goal),
                ..Default::default()
            },
        )
    }

    /// Every control message: its ID, control and body, oldest first.
    fn controls(b: &Board) -> Vec<(i64, String, String)> {
        let mut stmt = b
            .conn
            .prepare("SELECT id, control, body FROM messages WHERE control IS NOT NULL ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn only_the_owner_answers_a_goal_proposal_and_approval_sets_the_goal() {
        let mut b = Board::memory();
        let goal = |objective: &str, duration_s| proto::BlockerGoal {
            objective: objective.into(),
            duration_s,
        };
        let pick = choices(proto::BlockerChoiceMode::BlockerSelectOne, &["a", "b"]);
        let err = propose(&mut b, goal("Ship", None), Some(pick), "own").unwrap_err();
        assert!(err.to_string().contains("choices are fixed"), "{err:#}");
        assert!(propose(&mut b, goal(" ", None), None, "blank").is_err());
        assert!(propose(&mut b, goal("Ship", Some(0)), None, "zero").is_err());
        let err = put_blocker(
            &mut b.conn,
            &b.humans[0],
            proto::PutBlockerRequest {
                body: "Mine".into(),
                request_key: "human".into(),
                goal: Some(goal("Ship", None)),
                ..Default::default()
            },
        );
        assert!(err.is_err());

        let proposed = goal("Ship 0.26", Some(8 * 3600));
        let proposal = propose(&mut b, proposed.clone(), None, "ship")
            .unwrap()
            .blocker
            .unwrap();
        assert_eq!(proposal.goal.as_ref(), Some(&proposed));
        assert_eq!(proposal.choices, Some(blocker_choices::goal_choices()));
        // Every client shows the objective, in the stored body.
        assert!(
            proposal.body.starts_with(
                "I can own the release.\n\n**Proposed goal**, with a time limit of 8h"
            )
        );
        assert!(
            proposal.body.ends_with("\n\n```\nShip 0.26\n```"),
            "{}",
            proposal.body
        );
        // A retry with the same payload replays.
        assert!(
            propose(&mut b, proposed.clone(), None, "ship")
                .unwrap()
                .replayed
        );
        // Only will owns Mateo, so only he answers and sets its goal.
        let read = |b: &mut Board, human: usize| {
            get_blocker(
                &mut b.conn,
                &b.humans[human],
                proto::GetBlockerRequest {
                    id: proposal.id,
                    ..Default::default()
                },
            )
            .unwrap()
            .blocker
            .unwrap()
        };
        let as_ada = read(&mut b, 1);
        assert!(!as_ada.can_cancel && !as_ada.can_answer);
        assert!(read(&mut b, 0).can_answer);
        let err = b.answer(1, proposal.id, "ada").unwrap_err();
        assert!(err.to_string().contains("only Mateo's owner"), "{err:#}");
        assert!(controls(&b).is_empty());

        // Approval sets the goal and its deadline from the owner, before the
        // answer mail, in the same transaction.
        let answered = select(&mut b, proposal.id, &["approve"], "", "approve").unwrap();
        let set = controls(&b);
        assert_eq!(set.len(), 2);
        assert_eq!(
            (set[0].1.as_str(), set[0].2.as_str()),
            ("goal-set", "Ship 0.26")
        );
        assert_eq!(set[1].1, "goal-deadline");
        let deadline = chrono::DateTime::parse_from_rfc3339(&set[1].2).unwrap();
        let ahead = deadline.with_timezone(&chrono::Utc) - chrono::Utc::now();
        assert!((ahead - chrono::Duration::hours(8)).num_seconds().abs() < 60);
        let mail_id = answered.message_id.unwrap();
        assert!(set.iter().all(|c| c.0 < mail_id));
        assert!(mail(&b, mail_id).contains("Selected: Approve and set this goal [approve]"));
        // A retry replays without setting it again.
        let again = select(&mut b, proposal.id, &["approve"], "", "approve").unwrap();
        assert!(again.replayed);
        assert_eq!(controls(&b).len(), 2);

        // Declining, or answering with only a note, sets nothing.
        let undated = propose(&mut b, goal("Tidy up", None), None, "tidy")
            .unwrap()
            .blocker
            .unwrap();
        select(&mut b, undated.id, &["decline"], "Not now.", "decline").unwrap();
        let noted = propose(&mut b, goal("Tidy up", None), None, "tidy-2")
            .unwrap()
            .blocker
            .unwrap();
        b.answer(0, noted.id, "note").unwrap();
        assert_eq!(controls(&b).len(), 2);
        // Without a duration, approval sets no deadline.
        let last = propose(&mut b, goal("Tidy up", None), None, "tidy-3")
            .unwrap()
            .blocker
            .unwrap();
        select(&mut b, last.id, &["approve"], "Go ahead.", "approve-3").unwrap();
        let set = controls(&b);
        assert_eq!(set.len(), 3);
        assert_eq!(
            (set[2].1.as_str(), set[2].2.as_str()),
            ("goal-set", "Tidy up")
        );
        // A full-size objective, well past the choices limit once escaped,
        // is stored and set whole.
        let full = "\"".repeat(blocker_choices::MAX_GOAL_OBJECTIVE);
        let big = propose(&mut b, goal(&full, None), None, "full")
            .unwrap()
            .blocker
            .unwrap();
        assert_eq!(big.goal.unwrap().objective, full);
        select(&mut b, big.id, &["approve"], "", "approve-full").unwrap();
        assert_eq!(controls(&b)[3].2, full);
    }
}
