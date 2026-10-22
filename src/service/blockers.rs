use anyhow::{Context, Result, bail};
use rusqlite::Connection;

use super::{Caller, Ctx, group_name, principal, require_group, with_ctx};
use crate::db::{self, Kind, Principal, blockers as store};
use crate::proto;

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

fn manages(ctx: &Ctx<'_>, actor: &Principal, group: i64) -> Result<bool> {
    match actor.kind {
        Kind::Agent => db::is_member(ctx.conn, group, actor.id),
        Kind::Human => store::human_owns_member(ctx.conn, group, actor.id),
    }
}

fn authorize(ctx: &Ctx<'_>, actor: &Principal, group: i64) -> Result<()> {
    if !manages(ctx, actor, group)? {
        bail!("blocker writes require current group membership or ownership of a current member");
    }
    Ok(())
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

fn as_proto(ctx: &Ctx<'_>, actor: &Principal, row: store::Row) -> Result<proto::Blocker> {
    let open = row.outcome.is_none() && row.replaced_by.is_none() && !row.archived;
    let can_cancel = open && manages(ctx, actor, row.group_id)?;
    let can_answer =
        can_cancel && actor.kind == Kind::Human && db::agent_is_live(ctx.conn, row.creator.id)?;
    let status = match row.outcome.as_ref().map(|e| e.kind.as_str()) {
        Some("answered") => proto::BlockerStatus::BlockerAnswered,
        Some("cancelled") => proto::BlockerStatus::BlockerCancelled,
        Some("superseded") => proto::BlockerStatus::BlockerSuperseded,
        _ => proto::BlockerStatus::BlockerOpen,
    };
    let mut creator = principal(row.creator);
    creator.owner = row.creator_owner;
    Ok(proto::Blocker {
        id: row.id,
        group_id: row.group_id,
        group: row.group,
        creator: Some(creator),
        body: row.body,
        created_at: row.created_at,
        root_id: row.root_id,
        replaces: row.replaces,
        replaced_by: row.replaced_by,
        status: status.into(),
        outcome: row.outcome.map(|o| proto::BlockerOutcome {
            status: status.into(),
            actor: Some(proto::Principal {
                owner: o.actor_owner,
                ..principal(o.actor)
            }),
            created_at: o.created_at,
            answer: o.answer,
            message_id: o.message_id,
        }),
        can_answer,
        can_cancel,
    })
}

pub fn list_blockers(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::ListBlockersRequest,
) -> Result<proto::ListBlockersResponse> {
    let limit = page(req.limit, 50)?;
    if [req.group.is_some(), req.agent.is_some(), req.mine]
        .into_iter()
        .filter(|s| *s)
        .count()
        > 1
    {
        bail!("blocker group, agent and mine scopes are mutually exclusive");
    }
    if let Some(id) = req.after_root_id {
        positive(id, "root cursor")?;
    }
    with_ctx(conn, caller, |ctx| {
        let actor = ctx.actor()?;
        let human = if req.mine {
            Some(ctx.human()?.id)
        } else {
            None
        };
        let group = req.group.as_deref().map(group_name);
        if let Some(name) = group
            && db::group_ids_named(ctx.conn, name)?.is_empty()
        {
            bail!("no such group @{name}");
        }
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
            group,
            agent,
            human,
            req.include_closed,
            req.after_root_id.unwrap_or(0),
            limit + 1,
        )?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_after_root_id = has_more.then(|| rows.last().expect("nonempty page").root_id);
        Ok(proto::ListBlockersResponse {
            blockers: rows
                .into_iter()
                .map(|r| as_proto(ctx, &actor, r))
                .collect::<Result<_>>()?,
            active_count: store::active_count(ctx.conn, group, agent, human)?,
            has_more,
            next_after_root_id,
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
    text(&req.body, "body")?;
    key(&req.request_key)?;
    if let Some(id) = req.replaces {
        positive(id, "predecessor ID")?;
    }
    with_ctx(conn, caller, |ctx| {
        let actor = ctx.agent()?;
        let groups = db::group_ids_named(ctx.conn, group_name(&req.group))?;
        if let Some(response) = replay(ctx, &actor, &req.request_key, |op| {
            matches!(op,
            store::Operation::Put { group: old_group, body, replaces, .. }
            if groups.contains(old_group) && *body == req.body && *replaces == req.replaces)
        })? {
            return Ok(response);
        }
        let group = require_group(ctx.conn, group_name(&req.group))?;
        authorize(ctx, &actor, group)?;
        let prior = req.replaces.map(|id| require_row(ctx, id)).transpose()?;
        if let Some(prior) = &prior {
            if prior.group_id != group {
                bail!("a blocker replacement must stay in the same group");
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
            group,
            actor.id,
            &req.body,
            prior.as_ref(),
            &ctx.now_s,
            &req.request_key,
        )?;
        store::event(ctx.conn, id, "created", &actor, &ctx.now_s, None, None)?;
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
    text(&req.answer, "answer")?;
    key(&req.request_key)?;
    with_ctx(conn, caller, |ctx| {
        let actor = ctx.human()?;
        if let Some(response) = replay(ctx, &actor, &req.request_key, |op| {
            matches!(op,
            store::Operation::Answer { id, answer, .. } if *id == req.id && *answer == req.answer)
        })? {
            return Ok(response);
        }
        let row = require_row(ctx, req.id)?;
        authorize(ctx, &actor, row.group_id)?;
        require_open(&row)?;
        if !db::agent_is_live(ctx.conn, row.creator.id)? {
            bail!("creating agent is retired; cancel or replace this blocker instead");
        }
        let body = format!(
            "Blocker #{} in @{}:\n{}\n\nAnswer:\n{}",
            row.id, row.group, row.body, req.answer
        );
        super::validate_body(&body)?;
        let message_id = db::insert_message(ctx.conn, &actor, None, &body, &ctx.now_s)?;
        db::insert_delivery(ctx.conn, message_id, Kind::Agent, row.creator.id)?;
        db::record_human_conversation(ctx.conn, row.creator.id, actor.id, &ctx.now_s)?;
        store::event(
            ctx.conn,
            row.id,
            "answered",
            &actor,
            &ctx.now_s,
            Some((&req.answer, message_id)),
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
            store::Operation::Cancel { id } if *id == req.id)
        })? {
            return Ok(response);
        }
        let row = require_row(ctx, req.id)?;
        authorize(ctx, &actor, row.group_id)?;
        require_open(&row)?;
        store::event(
            ctx.conn,
            row.id,
            "cancelled",
            &actor,
            &ctx.now_s,
            None,
            Some(&req.request_key),
        )?;
        finish(ctx, &actor, row.id, None)
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
            service::group_register(
                &mut conn,
                &humans[0],
                proto::GroupRegisterRequest {
                    group: "crew".into(),
                    description: None,
                },
            )
            .unwrap();
            let mut ids = Vec::new();
            let agents = ["Mateo", "Zed", "Lu"]
                .into_iter()
                .enumerate()
                .map(|(i, name)| {
                    let registration = service::register(
                        &mut conn,
                        &humans[i],
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
                    if i < 2 {
                        service::group_join(
                            &mut conn,
                            &caller,
                            proto::GroupRequest {
                                group: "crew".into(),
                            },
                        )
                        .unwrap();
                    }
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
                    group: "crew".into(),
                    body: body.into(),
                    replaces,
                    request_key: key.into(),
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
        check(&mut b, [1, 1, 0]);
        let a = b.put(1, "updated a", Some(a.id), "a2");
        check(&mut b, [1, 1, 0]); // one open version, not two
        let c = b.put(0, "c", None, "c");
        check(&mut b, [2, 2, 0]); // count all, not just the returned page
        b.answer(0, a.id, "answer").unwrap();
        check(&mut b, [1, 1, 0]);
        cancel_blocker(
            &mut b.conn,
            &b.agents[0],
            proto::CancelBlockerRequest {
                id: c.id,
                request_key: "cancel".into(),
            },
        )
        .unwrap();
        check(&mut b, [0, 0, 0]);
        service::group_register(
            &mut b.conn,
            &b.humans[0],
            proto::GroupRegisterRequest {
                group: "other".into(),
                description: None,
            },
        )
        .unwrap();
        for i in [0, 2] {
            service::group_join(
                &mut b.conn,
                &b.agents[i],
                proto::GroupRequest {
                    group: "other".into(),
                },
            )
            .unwrap();
        }
        put_blocker(
            &mut b.conn,
            &b.agents[2],
            proto::PutBlockerRequest {
                group: "other".into(),
                body: "other issue".into(),
                request_key: "other".into(),
                replaces: None,
            },
        )
        .unwrap();
        b.put(0, "new crew issue", None, "crew");
        check(&mut b, [2, 1, 1]); // includes blockers created by someone else
        service::group_leave(
            &mut b.conn,
            &b.agents[0],
            proto::GroupRequest {
                group: "crew".into(),
            },
        )
        .unwrap();
        check(&mut b, [1, 1, 1]);
        service::group_delete(
            &mut b.conn,
            &b.humans[1],
            proto::GroupRequest {
                group: "crew".into(),
            },
        )
        .unwrap();
        check(&mut b, [1, 0, 1]);
        service::group_register(
            &mut b.conn,
            &b.humans[0],
            proto::GroupRegisterRequest {
                group: "crew".into(),
                description: None,
            },
        )
        .unwrap();
        service::group_join(
            &mut b.conn,
            &b.agents[0],
            proto::GroupRequest {
                group: "crew".into(),
            },
        )
        .unwrap();
        check(&mut b, [1, 0, 1]); // archived identity never leaks into the new group
        b.put(0, "new identity", None, "new-crew");
        check(&mut b, [2, 0, 1]);
    }

    #[test]
    fn retired_agent_counts_do_not_inherit_a_reused_names_blockers() {
        let mut b = Board::memory();
        b.put(0, "original agent", None, "old");
        service::bye(&mut b.conn, &b.agents[0]).unwrap();
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
        service::group_register(
            &mut b.conn,
            &b.humans[0],
            proto::GroupRegisterRequest {
                group: "new-group".into(),
                description: None,
            },
        )
        .unwrap();
        service::group_join(
            &mut b.conn,
            &mateo,
            proto::GroupRequest {
                group: "new-group".into(),
            },
        )
        .unwrap();
        put_blocker(
            &mut b.conn,
            &mateo,
            proto::PutBlockerRequest {
                group: "new-group".into(),
                body: "new agent".into(),
                request_key: "new".into(),
                replaces: None,
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
        // Zed, ada's agent, supersedes Mateo's blocker and cancels its own.
        let first = b.put(0, "Which option?", None, "first");
        let second = b.put(1, "Which option, now with costs?", Some(first.id), "second");
        assert_eq!(
            owner_of(&mut b, first.id),
            ("Zed".into(), Some("ada".into()))
        );
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
            ("Zed".into(), Some("ada".into()))
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
            1,
            "Need a choice, with updated estimates",
            Some(old.id),
            "replace",
        );
        assert_eq!(b.messages(), before);
        assert_eq!(next.root_id, old.id);
        assert_eq!(next.replaces, Some(old.id));
        assert_eq!(next.creator.as_ref().unwrap().id, b.ids[1]);
        assert_eq!(b.list().active_count, 1);
        assert!(b.answer(0, old.id, "stale").is_err());
        assert_eq!(b.messages(), before);
        let answered = b.answer(0, next.id, "answer").unwrap();
        let again = b.answer(0, next.id, "answer").unwrap();
        assert!(again.replayed);
        assert_eq!(answered.message_id, again.message_id);
        assert_eq!(b.messages(), before + 1);
        assert_eq!(b.list().active_count, 0);
        let recipient: i64 = b.conn.query_row(
            "SELECT recipient_id FROM deliveries WHERE message_id = ?1 AND recipient_kind = 'agent'",
            [answered.message_id.unwrap()], |r| r.get(0),
        ).unwrap();
        assert_eq!(recipient, b.ids[1]);
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
    fn blockers_check_current_membership_and_ownership_without_group_writer_grants() {
        let mut b = Board::memory();
        let first = b.put(0, "Need help", None, "first");
        assert!(b.answer(2, first.id, "foreign").is_err());
        assert!(
            answer_blocker(
                &mut b.conn,
                &b.agents[0],
                proto::AnswerBlockerRequest {
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
                &b.agents[2],
                proto::PutBlockerRequest {
                    group: "crew".into(),
                    body: "not a member".into(),
                    request_key: "foreign".into(),
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert!(
            put_blocker(
                &mut b.conn,
                &b.humans[0],
                proto::PutBlockerRequest {
                    group: "crew".into(),
                    body: "human create".into(),
                    request_key: "human".into(),
                    ..Default::default()
                }
            )
            .is_err()
        );
        // An eligible human can help a different owner's agent.
        let next = b.put(
            1,
            "Updated by another group member",
            Some(first.id),
            "replace",
        );
        service::group_leave(
            &mut b.conn,
            &b.agents[0],
            proto::GroupRequest {
                group: "crew".into(),
            },
        )
        .unwrap();
        assert!(b.answer(0, next.id, "lost-membership").is_err());
        assert!(b.answer(1, next.id, "own-member").is_ok());
        // Reading is board-wide for an authenticated human or nonmember agent.
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
    fn blocker_receipts_refuse_payload_or_operation_reuse_and_replay_without_new_mail() {
        let mut b = Board::memory();
        let request = proto::PutBlockerRequest {
            group: "crew".into(),
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
        let one = cancel_blocker(&mut b.conn, &b.humans[1], cancel.clone()).unwrap();
        let replay = cancel_blocker(&mut b.conn, &b.humans[1], cancel).unwrap();
        assert!(replay.replayed);
        assert_eq!(one.blocker, replay.blocker);
        assert_eq!(b.messages(), before);
        assert!(b.answer(1, id, "new-answer").is_err());
        let new = b.put(1, "Need help again", Some(id), "another");
        let before = b.messages();
        cancel_blocker(
            &mut b.conn,
            &b.agents[0],
            proto::CancelBlockerRequest {
                id: new.id,
                request_key: "another-cancel".into(),
            },
        )
        .unwrap();
        assert_eq!(b.messages(), before);
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
        assert!(b.answer(1, first.id, "answer").is_err());
        assert_eq!(b.messages(), before);
        assert_eq!(b.list().active_count, 1);
        assert!(
            store::operation(
                &b.conn,
                &db::find_human(&b.conn, "ada").unwrap().unwrap(),
                "answer",
            )
            .unwrap()
            .is_none()
        );
        b.conn.execute_batch("DROP TRIGGER refuse_answer").unwrap();
        b.answer(1, first.id, "answer").unwrap();
        assert_eq!(b.messages(), before + 1);
    }

    #[test]
    fn blocker_scope_counts_and_paging_preserve_original_queue_position() {
        let mut b = Board::memory();
        let one = b.put(0, "one", None, "one");
        let two = b.put(1, "two", None, "two");
        let new_one = b.put(1, "new one", Some(one.id), "new-one");
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
                group: Some("@crew".into()),
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
    fn retired_creators_and_archived_groups_keep_history_without_name_reuse_delivery() {
        let mut b = Board::memory();
        let first = b.put(0, "Need help", None, "put");
        service::bye(&mut b.conn, &b.agents[0]).unwrap();
        service::register(
            &mut b.conn,
            &b.humans[2],
            proto::RegisterRequest {
                name: Some("Mateo".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let before = b.messages();
        assert!(b.answer(1, first.id, "answer").is_err());
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
            &b.humans[1],
            proto::CancelBlockerRequest {
                id: first.id,
                request_key: "cancel".into(),
            },
        )
        .unwrap();
        service::group_delete(
            &mut b.conn,
            &b.humans[0],
            proto::GroupRequest {
                group: "crew".into(),
            },
        )
        .unwrap();
        assert!(
            get_blocker(
                &mut b.conn,
                &b.humans[2],
                proto::GetBlockerRequest {
                    id: first.id,
                    ..Default::default()
                }
            )
            .is_ok()
        );
        let history = list_blockers(
            &mut b.conn,
            &b.humans[2],
            proto::ListBlockersRequest {
                group: Some("crew".into()),
                include_closed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(history.blockers.len(), 1);
        assert_eq!(history.active_count, 0);
    }

    #[test]
    fn blocker_bounds_invalid_scopes_and_stale_replacement_leave_history_unchanged() {
        let mut b = Board::memory();
        let first = b.put(0, &"界".repeat(2730), None, "first");
        for body in [" \n\t".into(), "界".repeat(2731)] {
            assert!(
                put_blocker(
                    &mut b.conn,
                    &b.agents[0],
                    proto::PutBlockerRequest {
                        group: "crew".into(),
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
        assert!(
            list_blockers(
                &mut b.conn,
                &b.humans[0],
                proto::ListBlockersRequest {
                    group: Some("crew".into()),
                    agent: Some("Mateo".into()),
                    ..Default::default()
                }
            )
            .is_err()
        );
        let next = b.put(1, "replacement", Some(first.id), "replace");
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
                    group: "crew".into(),
                    body: "fork a chain".into(),
                    replaces: Some(first.id),
                    request_key: "fork".into()
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
    fn committed_blocker_retries_survive_membership_changes_and_group_archive() {
        let mut b = Board::memory();
        let first = b.put(0, "Need approval", None, "put");
        let answered = b.answer(0, first.id, "answer").unwrap();
        service::group_leave(
            &mut b.conn,
            &b.agents[0],
            proto::GroupRequest {
                group: "crew".into(),
            },
        )
        .unwrap();
        let before = b.messages();
        let replay = b.answer(0, first.id, "answer").unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.message_id, answered.message_id);
        assert_eq!(b.messages(), before);
        assert!(b.answer(0, first.id, "different-answer").is_err());

        let mut b = Board::memory();
        let first = b.put(0, "Archive after commit", None, "put");
        service::group_delete(
            &mut b.conn,
            &b.humans[0],
            proto::GroupRequest {
                group: "crew".into(),
            },
        )
        .unwrap();
        let replay = put_blocker(
            &mut b.conn,
            &b.agents[0],
            proto::PutBlockerRequest {
                group: "crew".into(),
                body: "Archive after commit".into(),
                request_key: "put".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.blocker.unwrap().id, first.id);
        assert!(
            put_blocker(
                &mut b.conn,
                &b.agents[0],
                proto::PutBlockerRequest {
                    group: "crew".into(),
                    body: "New write after archive".into(),
                    request_key: "new".into(),
                    ..Default::default()
                }
            )
            .is_err()
        );

        let mut b = Board::memory();
        let first = b.put(0, "Cancel before leaving", None, "put");
        let request = proto::CancelBlockerRequest {
            id: first.id,
            request_key: "cancel".into(),
        };
        cancel_blocker(&mut b.conn, &b.agents[1], request.clone()).unwrap();
        service::group_leave(
            &mut b.conn,
            &b.agents[1],
            proto::GroupRequest {
                group: "crew".into(),
            },
        )
        .unwrap();
        let before = b.messages();
        let replay = cancel_blocker(&mut b.conn, &b.agents[1], request).unwrap();
        assert!(replay.replayed);
        assert_eq!(
            replay.blocker.unwrap().status(),
            proto::BlockerStatus::BlockerCancelled
        );
        assert_eq!(b.messages(), before);
        assert!(
            cancel_blocker(
                &mut b.conn,
                &b.agents[1],
                proto::CancelBlockerRequest {
                    id: first.id,
                    request_key: "different-cancel".into()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn recreated_group_names_use_current_membership_and_keep_original_retry_identity() {
        let mut b = Board::memory();
        let old = b.put(0, "Original group", None, "old-key");
        service::group_delete(
            &mut b.conn,
            &b.humans[0],
            proto::GroupRequest {
                group: "crew".into(),
            },
        )
        .unwrap();
        service::group_register(
            &mut b.conn,
            &b.humans[0],
            proto::GroupRegisterRequest {
                group: "crew".into(),
                description: None,
            },
        )
        .unwrap();
        service::group_join(
            &mut b.conn,
            &b.agents[0],
            proto::GroupRequest {
                group: "crew".into(),
            },
        )
        .unwrap();
        let current = b.put(0, "New group", None, "new-key");
        assert_ne!(old.group_id, current.group_id);
        let replay = put_blocker(
            &mut b.conn,
            &b.agents[0],
            proto::PutBlockerRequest {
                group: "@crew".into(),
                body: "Original group".into(),
                request_key: "old-key".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.blocker.unwrap().id, old.id);
        let active = list_blockers(
            &mut b.conn,
            &b.humans[0],
            proto::ListBlockersRequest {
                group: Some("crew".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(active.active_count, 1);
        assert_eq!(
            active.blockers.iter().map(|x| x.id).collect::<Vec<_>>(),
            [current.id]
        );
        let all = list_blockers(
            &mut b.conn,
            &b.humans[0],
            proto::ListBlockersRequest {
                group: Some("crew".into()),
                include_closed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(all.active_count, 1);
        assert_eq!(
            all.blockers.iter().map(|x| x.id).collect::<Vec<_>>(),
            [old.id, current.id]
        );
        let agent = list_blockers(
            &mut b.conn,
            &b.humans[0],
            proto::ListBlockersRequest {
                agent: Some("Mateo".into()),
                include_closed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(agent.active_count, 1);
        assert_eq!(
            agent.blockers.iter().map(|x| x.id).collect::<Vec<_>>(),
            [current.id]
        );
        // A predecessor in the archived identity cannot be moved to the new group.
        assert!(
            put_blocker(
                &mut b.conn,
                &b.agents[0],
                proto::PutBlockerRequest {
                    group: "crew".into(),
                    body: "Move history".into(),
                    replaces: Some(old.id),
                    request_key: "cross-group".into()
                }
            )
            .is_err()
        );
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
                let caller = b.humans[i].clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    answer_blocker(
                        &mut conn,
                        &caller,
                        proto::AnswerBlockerRequest {
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
}
