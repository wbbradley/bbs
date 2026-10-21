use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{IsTerminal, Read as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::backend::{Backend, Local, Remote};
use crate::cli::{Cli, Command, GroupCommand, HumanCommand, TimerCommand};
use crate::config::Config;
use crate::proto;
use crate::service::{Caller, MAX_BODY_BYTES, fmt_time};

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("$HOME is not set")
}

pub(crate) fn state_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".local/state/bbs"))
}

/// Stored timestamps are fixed-width; show them to second precision.
pub(crate) fn display_time(stored: &str) -> String {
    format!("{}Z", &stored[..19.min(stored.len())])
}

pub async fn run(cli: Cli) -> Result<()> {
    let notice = wants_update_notice(&cli);
    let Cli {
        theme,
        token,
        as_human,
        command,
    } = cli;
    // Bare `bbs` opens connect; explicit subcommands behave exactly as before.
    let command = command.unwrap_or(Command::Connect);
    let theme_explicit = crate::ui::theme::init(theme.as_deref())?;
    let dir = match &command {
        Command::Serve {
            state_dir: Some(dir),
            ..
        }
        | Command::Admin {
            state_dir: Some(dir),
            ..
        }
        | Command::Backup {
            state_dir: Some(dir),
            ..
        } => dir.clone(),
        _ => state_dir()?,
    };
    match &command {
        Command::Init {
            server,
            human,
            human_token_stdin,
        } => {
            return init(&dir, server, human, *human_token_stdin, as_human.as_deref()).await;
        }
        Command::Hook { harness } => return crate::hook::run(*harness, &dir).await,
        Command::Headless { .. } => {
            let Command::Headless { command } = command else {
                unreachable!()
            };
            return crate::headless::run(command, &dir, as_human.as_deref()).await;
        }
        Command::Serve {
            listen,
            state_dir,
            require_human_token,
            minimum_client_version,
            recommended_client_version,
            max_timers_per_agent,
            min_timer_interval,
        } => {
            return crate::serve::run(
                *listen,
                state_dir.as_deref().unwrap_or(&dir),
                *require_human_token,
                crate::protocol::Policy::new(
                    minimum_client_version.clone(),
                    recommended_client_version.clone(),
                )?,
                crate::service::TimerLimits {
                    max_per_agent: *max_timers_per_agent,
                    min_interval: *min_timer_interval,
                },
            )
            .await;
        }
        Command::Admin { command, .. } => return crate::admin::run(&dir, command),
        Command::Backup { destination, .. } => {
            crate::backup::run(&dir, destination)?;
            println!("Backup saved to {}", destination.display());
            return Ok(());
        }
        _ => {}
    }
    let config = Config::load()?;
    let notice = notice && std::io::stderr().is_terminal();
    let caller = Caller {
        token: token.clone(),
        human: config.human(as_human.as_deref()),
        ..Default::default()
    };
    let mut backend = Backend::open(&config, &dir, caller).await?;
    match command {
        Command::View(args) => crate::view::run(&mut backend, args).await,
        Command::Connect => {
            let opener = crate::connect::Opener {
                config: config.clone(),
                dir: dir.clone(),
                caller: Caller {
                    token: token.clone(),
                    human: config.human(as_human.as_deref()),
                    ..Default::default()
                },
            };
            crate::connect::run(backend, opener, theme_explicit).await
        }
        Command::Read {
            limit,
            wait: true,
            json,
            ..
        } => {
            print!(
                "{}",
                format_messages(&backend.wait_for_mail(limit).await?, json)?
            );
            Ok(())
        }
        command => {
            print!("{}", dispatch(&mut backend, command).await?);
            if notice && let Some(line) = crate::protocol::update_notice() {
                eprintln!("{line}");
            }
            Ok(())
        }
    }
}

/// Only humans see the update notice. Agents pass `--token`, except when they
/// `register`, which runs before they have one.
fn wants_update_notice(cli: &Cli) -> bool {
    cli.token.is_none() && !matches!(cli.command, Some(Command::Register { .. }))
}

async fn dispatch(backend: &mut Backend, command: Command) -> Result<String> {
    Ok(match command {
        Command::Init { .. }
        | Command::Hook { .. }
        | Command::Headless { .. }
        | Command::Serve { .. }
        | Command::Admin { .. }
        | Command::Backup { .. }
        | Command::Version { .. }
        | Command::Setup(_)
        | Command::View(_)
        | Command::Connect => {
            unreachable!()
        }
        Command::Human {
            command: HumanCommand::Add { name },
        } => {
            backend
                .add_human(proto::AddHumanRequest { name: name.clone() })
                .await?;
            format!("Added human {name}.\n")
        }
        Command::Timer { command } => match command {
            TimerCommand::Add { agent, when, text } => {
                let req = proto::TimerAddRequest {
                    agent,
                    when,
                    body: text,
                };
                let t = backend.timer_add(req).await?;
                format!("Timer {} for {}: {}.\n", t.id, t.agent, timer_schedule(&t))
            }
            TimerCommand::List { agent } => {
                let r = backend
                    .timer_list(proto::TimerListRequest { agent })
                    .await?;
                r.timers.iter().map(timer_line).collect()
            }
            TimerCommand::Rm { id } => {
                let t = backend
                    .timer_remove(proto::TimerRemoveRequest { id })
                    .await?;
                format!("Removed timer {} for {}.\n", t.id, t.agent)
            }
        },
        Command::Group { command } => match command {
            GroupCommand::Register { group, description } => {
                let r = backend
                    .group_register(proto::GroupRegisterRequest { group, description })
                    .await?;
                format!("Registered group @{}.\n", r.group)
            }
            GroupCommand::Describe {
                group,
                description,
                clear,
            } => {
                // Clap requires exactly one of a description or --clear.
                let req = proto::GroupDescribeRequest { group, description };
                let r = backend.group_describe(req).await?;
                if clear {
                    format!("Cleared the description of @{}.\n", r.group)
                } else {
                    format!("Updated the description of @{}.\n", r.group)
                }
            }
            GroupCommand::Delete { group } => {
                let r = backend.group_delete(proto::GroupRequest { group }).await?;
                format!("Archived group @{}.\n", r.group)
            }
            GroupCommand::Join { group } => {
                let r = backend.group_join(proto::GroupRequest { group }).await?;
                if r.joined {
                    format!("Joined @{}.\n", r.group)
                } else {
                    format!("Already in @{}.\n", r.group)
                }
            }
            GroupCommand::Leave { group } => {
                let r = backend.group_leave(proto::GroupRequest { group }).await?;
                format!("Left @{}.\n", r.group)
            }
            GroupCommand::Add { group, agents } => {
                let r = backend
                    .group_add(proto::GroupAddRequest { group, agents })
                    .await?;
                let mut out = String::new();
                if !r.added.is_empty() {
                    writeln!(out, "Added {} to @{}.", r.added.join(", "), r.group)?;
                }
                if !r.already.is_empty() {
                    writeln!(out, "Already in @{}: {}.", r.group, r.already.join(", "))?;
                }
                out
            }
            GroupCommand::Remove { group, agent } => {
                let req = proto::GroupRemoveRequest { group, agent };
                let r = backend.group_remove(req).await?;
                format!("Removed {} from @{}.\n", r.agent, r.group)
            }
            GroupCommand::Grant { group, agent } => {
                set_group_writer(backend, group, agent, true).await?
            }
            GroupCommand::Revoke { group, agent } => {
                set_group_writer(backend, group, agent, false).await?
            }
            GroupCommand::List { json } => {
                format_groups(&backend.group_list().await?.groups, json)?
            }
        },
        Command::Register { name, note } => {
            let req = proto::RegisterRequest {
                name,
                note,
                pwd: std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                hostname: gethostname::gethostname().to_string_lossy().into_owned(),
            };
            let r = backend.register(req).await?;
            format!("Welcome to the BBS. {}\n", identity_line(&r.name, &r.token))
        }
        Command::Status { text } => {
            backend.set_status(proto::SetStatusRequest { text }).await?;
            "Status updated.\n".to_string()
        }
        Command::Bye => format!("Goodbye, {}.\n", backend.bye().await?.name),
        Command::Retire { agent } => {
            let r = backend.retire(proto::RetireRequest { agent }).await?;
            format!("Retired {}.\n", r.name)
        }
        Command::Send { to, body } => {
            let req = proto::SendRequest {
                to,
                body: read_body(body)?,
                control: None,
            };
            let r = backend.send(req).await?;
            match r.recipients {
                Some(n) => format!("Sent to {} ({n} recipients).\n", r.to),
                None => format!("Sent to {}.\n", r.to),
            }
        }
        Command::Read {
            limit,
            archived,
            since,
            json,
            ..
        } => {
            let req = proto::ReadRequest {
                limit,
                archived,
                since: since
                    .map(|s| parse_time("--since", &s, Utc::now()))
                    .transpose()?,
            };
            format_messages(&backend.read(req).await?.messages, json)?
        }
        Command::Whoami => format!("{}\n", backend.whoami().await?.name),
        Command::Agents { all, json } => {
            let r = backend
                .list_agents(proto::ListAgentsRequest { all })
                .await?;
            if json {
                agents_json(&r)?
            } else {
                format_agents(&r)
            }
        }
    })
}

/// Validate remote enrollment before saving credentials or changing config.
async fn init(
    dir: &Path,
    server: &Option<String>,
    human: &Option<String>,
    human_token_stdin: bool,
    as_human: Option<&str>,
) -> Result<()> {
    let mut config = Config::load()?;
    if let Some(server) = server {
        if config.server.as_ref() != Some(server) {
            config.human_token = None;
        }
        config.server = Some(server.clone());
    }
    if let Some(addr) = config.server.clone() {
        let mut backend = Backend::Remote(Remote::connect(&addr, Caller::default()).await?);
        let info = backend.server_info().await?;
        if human_token_stdin {
            let mut token = String::new();
            std::io::stdin().take(4097).read_to_string(&mut token)?;
            anyhow::ensure!(token.len() <= 4096, "human token input is too long");
            config.human_token = Some(token.trim().to_string());
        }
        if info.require_human_token && config.human_token.is_none() {
            anyhow::ensure!(
                std::io::stdin().is_terminal(),
                "human token required; pass --human-token-stdin or run in a terminal"
            );
            config.human_token = Some(
                rpassword::prompt_password("Human token: ")?
                    .trim()
                    .to_string(),
            );
        }
        let name = if let Some(token) = &config.human_token {
            backend.set_caller(Caller {
                human_token: Some(token.clone()),
                human: human.clone().or_else(|| as_human.map(str::to_string)),
                ..Default::default()
            });
            backend.whoami().await?.name
        } else {
            let name = human
                .clone()
                .or_else(|| config.human(as_human))
                .context("$USER is not set; pass --human <name>")?;
            backend
                .ensure_human(proto::EnsureHumanRequest { name: name.clone() })
                .await?;
            name
        };
        config.human = Some(name.clone());
        config.write()?;
        println!("BBS configured for server {addr}\nHuman: {name}");
    } else {
        let name = human
            .clone()
            .or_else(|| config.human(as_human))
            .context("$USER is not set; pass --human <name>")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut backend = Backend::Local(Local::open(dir, Caller::default())?);
        backend
            .ensure_human(proto::EnsureHumanRequest { name: name.clone() })
            .await?;
        if human.is_some() {
            config.human = Some(name.clone());
            config.write()?;
        }
        println!("BBS initialized at {}\nHuman: {name}", dir.display());
    }
    Ok(())
}

/// One JSON object per line. Every field is always present; absent values are
/// null. Names are bare (no `@`), timestamps are stored RFC3339 UTC strings.
fn json_lines<T: Serialize>(items: impl IntoIterator<Item = T>) -> Result<String> {
    let mut out = String::new();
    for item in items {
        out.push_str(&serde_json::to_string(&item)?);
        out.push('\n');
    }
    Ok(out)
}

#[derive(Serialize)]
struct GroupJson<'a> {
    name: &'a str,
    members: &'a [String],
    description: Option<&'a str>,
    /// Null from servers without group-writers.v1.
    writers: Option<&'a [String]>,
}

#[derive(Serialize)]
struct MessageJson<'a> {
    id: i64,
    created_at: &'a str,
    from: &'a str,
    from_kind: &'static str,
    /// The sending agent's human; null for humans and unknown owners.
    from_owner: Option<&'a str>,
    /// Direct recipient; null for broadcasts.
    to: Option<&'a str>,
    /// Broadcast group; null for direct messages.
    group: Option<&'a str>,
    body: &'a str,
    /// The timer that sent it; null for other messages.
    timer: Option<i64>,
}

#[derive(Serialize)]
struct AgentJson<'a> {
    name: &'a str,
    kind: &'static str,
    retired: bool,
    owner: Option<&'a str>,
    /// Null for humans, who receive every group broadcast.
    groups: Option<&'a [String]>,
    active_at: &'a str,
    hostname: Option<&'a str>,
    pwd: Option<&'a str>,
    status: Option<&'a str>,
}

fn kind_name(kind: proto::Kind) -> &'static str {
    match kind {
        proto::Kind::Agent => "agent",
        proto::Kind::Human => "human",
        proto::Kind::Unspecified => "unspecified",
    }
}

pub(crate) fn format_groups(groups: &[proto::Group], json: bool) -> Result<String> {
    if json {
        return json_lines(groups.iter().map(|g| GroupJson {
            name: &g.name,
            members: &g.members,
            description: g.description.as_deref(),
            writers: g.writers.as_ref().map(|w| &w.names[..]),
        }));
    }
    let mut out = String::new();
    for g in groups {
        let members = if g.members.is_empty() {
            "(no agents)".to_string()
        } else {
            g.members
                .iter()
                .map(|m| match &g.writers {
                    Some(w) if w.names.contains(m) => format!("{m} [w]"),
                    _ => m.clone(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        writeln!(out, "@{}: {members}", g.name)?;
        // Indented under its group so member parsing stays one `@group: ...` row.
        if let Some(description) = &g.description {
            writeln!(out, "  {description}")?;
        }
    }
    Ok(out)
}

async fn set_group_writer(
    backend: &mut Backend,
    group: String,
    agent: String,
    grant: bool,
) -> Result<String> {
    let req = proto::SetGroupWriterRequest {
        group,
        agent,
        grant,
    };
    let r = backend.set_group_writer(req).await?;
    Ok(match (grant, r.changed) {
        (true, true) => format!("Granted write on @{} to {}.\n", r.group, r.agent),
        (false, true) => format!("Revoked write on @{} from {}.\n", r.group, r.agent),
        (true, false) => format!("{} already has write on @{}.\n", r.agent, r.group),
        (false, false) => format!("{} already lacks write on @{}.\n", r.agent, r.group),
    })
}

pub(crate) fn identity_line(name: &str, token: &str) -> String {
    format!("Your name is \"{name}\", access BBS with `bbs --token {token} ...`")
}

/// The message body from the argument, or from stdin when omitted or "-".
/// Read one byte past the limit so the service can reject oversized bodies.
fn read_body(body: Option<String>) -> Result<String> {
    match body {
        Some(b) if b != "-" => Ok(b),
        _ => {
            let mut s = String::new();
            std::io::stdin()
                .take(MAX_BODY_BYTES as u64 + 1)
                .read_to_string(&mut s)
                .context("reading message body from stdin")?;
            Ok(s)
        }
    }
}

/// Resolve a time flag (RFC3339, or a duration like 2h meaning that long ago)
/// to RFC3339.
pub(crate) fn parse_time(flag: &str, s: &str, now: DateTime<Utc>) -> Result<String> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(fmt_time(t.with_timezone(&Utc)));
    }
    let d = humantime::parse_duration(s)
        .with_context(|| format!("invalid {flag} {s:?}: expected RFC3339 or a duration like 2h"))?;
    Ok(fmt_time(now - chrono::Duration::from_std(d)?))
}

/// When a timer runs: "every 1h, next at …" or "once at …".
pub(crate) fn timer_schedule(t: &proto::Timer) -> String {
    match &t.repeat {
        Some(repeat) => format!("{repeat}, next at {}", display_time(&t.next_run)),
        None => format!("once at {}", display_time(&t.next_run)),
    }
}

/// One timer per line: id, agent, schedule, creator, and text.
pub(crate) fn timer_line(t: &proto::Timer) -> String {
    format!(
        "{}  {}  {}  from {}: {}\n",
        t.id,
        t.agent,
        timer_schedule(t),
        t.creator,
        t.body.replace('\n', " ")
    )
}

pub(crate) fn format_messages(msgs: &[proto::Message], json: bool) -> Result<String> {
    if json {
        return json_lines(msgs.iter().map(|m| MessageJson {
            id: m.id,
            created_at: &m.created_at,
            from: &m.from_name,
            from_kind: kind_name(m.from_kind()),
            from_owner: m.from_owner.as_deref(),
            to: m.to_name.as_deref(),
            group: m.to_group.as_deref(),
            body: &m.body,
            timer: m.timer_id,
        }));
    }
    let mut out = String::new();
    for (i, m) in msgs.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let human = if m.from_kind() == proto::Kind::Human {
            " (human)"
        } else {
            ""
        };
        let to = match (&m.to_group, &m.to_name) {
            (Some(g), _) => format!("@{g}"),
            (None, to) => to.clone().unwrap_or_default(),
        };
        let timer = m
            .timer_id
            .map(|id| format!(" (timer {id})"))
            .unwrap_or_default();
        writeln!(
            out,
            "[{}] from {}{human} to {to}{timer}:",
            display_time(&m.created_at),
            m.sender_display_name()
        )?;
        out.push_str(m.body.trim_end_matches('\n'));
        out.push('\n');
    }
    Ok(out)
}

fn agents_json(r: &proto::ListAgentsResponse) -> Result<String> {
    let humans = r.humans.iter().map(|h| AgentJson {
        name: &h.name,
        kind: "human",
        retired: false,
        owner: None,
        groups: None,
        active_at: &h.active_at,
        hostname: None,
        pwd: None,
        status: None,
    });
    let agents = r.agents.iter().map(|a| AgentJson {
        name: &a.name,
        kind: "agent",
        retired: a.retired,
        owner: a.owner.as_deref(),
        groups: Some(&a.groups),
        active_at: &a.active_at,
        hostname: Some(&a.hostname),
        pwd: Some(&a.pwd),
        status: a.status.as_deref(),
    });
    json_lines(humans.chain(agents))
}

pub(crate) fn format_agents(r: &proto::ListAgentsResponse) -> String {
    let mut table: Vec<[String; 7]> = vec![[
        "NAME".into(),
        "KIND".into(),
        "OWNER".into(),
        "GROUPS".into(),
        "ACTIVE_AT".into(),
        "WHERE".into(),
        "STATUS".into(),
    ]];
    for h in &r.humans {
        table.push([
            h.name.clone(),
            "human".into(),
            "-".into(),
            "*".into(),
            display_time(&h.active_at),
            String::new(),
            String::new(),
        ]);
    }
    for a in &r.agents {
        let kind = if a.retired { "retired" } else { "agent" };
        let g = a.groups.join(",");
        table.push([
            a.name.clone(),
            kind.into(),
            a.owner.clone().unwrap_or_else(|| "-".into()),
            if g.is_empty() { "-".into() } else { g },
            display_time(&a.active_at),
            format!("{}:{}", a.hostname, a.pwd),
            a.status.clone().unwrap_or_default(),
        ]);
    }
    render_table(&table)
}

fn render_table(rows: &[[String; 7]]) -> String {
    let mut widths: HashMap<usize, usize> = HashMap::new();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            let w = widths.entry(i).or_default();
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i + 1 == row.len() {
                line.push_str(cell);
            } else {
                let _ = write!(line, "{cell:<w$}  ", w = widths[&i]);
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn update_notice_is_for_humans_only() {
        let wants = |args: &[&str]| wants_update_notice(&Cli::parse_from(args));
        assert!(wants(&["bbs", "agents"]));
        assert!(wants(&["bbs", "group", "register", "g"]));
        assert!(!wants(&["bbs", "--token", "t", "agents"]));
        assert!(!wants(&["bbs", "register"]));
    }

    #[test]
    fn bare_invocation_defaults_to_connect_and_flags_keep_semantics() {
        let cli = Cli::parse_from(["bbs"]);
        assert!(cli.command.is_none());
        let cli = Cli::parse_from(["bbs", "--as", "ada", "--theme", "mono"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.as_human.as_deref(), Some("ada"));
        assert_eq!(cli.theme.as_deref(), Some("mono"));
        // Explicit subcommands, help and version are unchanged.
        assert!(matches!(
            Cli::parse_from(["bbs", "agents"]).command,
            Some(Command::Agents { .. })
        ));
        let help = Cli::try_parse_from(["bbs", "--help"]).unwrap_err();
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        let version = Cli::try_parse_from(["bbs", "--version"]).unwrap_err();
        assert_eq!(version.kind(), clap::error::ErrorKind::DisplayVersion);
    }
}
