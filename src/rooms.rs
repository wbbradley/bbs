//! `bbs room`: rooms on another home (DESIGN.md "Rooms"), every action
//! through this installation's own server. Each verb becomes one `Room` RPC
//! with JSON arguments; `--json` prints its JSON result on one line.

use std::fmt::Write as _;

use anyhow::Result;
use clap::Subcommand;
use serde_json::{Value, json};

use crate::backend::Backend;

#[derive(Subcommand, Debug)]
pub enum RoomCommand {
    /// List the host's rooms, optionally only those whose name or
    /// description contains FILTER.
    List {
        filter: Option<String>,
        /// Include archived rooms.
        #[arg(long)]
        archived: bool,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// A room's description, who may post, managers and recent posters.
    Info { room: String },
    /// Create a room (you manage it). NAME is [a-z0-9][a-z0-9-]*.
    Create {
        name: String,
        #[arg(long, allow_hyphen_values = true)]
        desc: Option<String>,
        /// Who may post: any (default), none, self, ns:NAMESPACE,
        /// entity:PATH@NAMESPACE, comma-separated.
        #[arg(long, value_delimiter = ',')]
        post: Option<Vec<String>>,
        /// Extra managers, in the same forms.
        #[arg(long, value_delimiter = ',')]
        manage: Option<Vec<String>>,
    },
    /// Change a room's description (managers).
    Describe {
        room: String,
        #[arg(allow_hyphen_values = true)]
        text: String,
    },
    /// Set who may post (managers): any, none, or a list of subjects.
    Policy {
        room: String,
        #[arg(long, value_delimiter = ',', required = true)]
        post: Vec<String>,
    },
    /// Let a subject post, or make it a manager (managers).
    Grant {
        room: String,
        #[arg(long, conflicts_with = "manage", required_unless_present = "manage")]
        post: Option<String>,
        #[arg(long)]
        manage: Option<String>,
    },
    /// Stop a subject posting, or remove a manager (managers).
    Revoke {
        room: String,
        #[arg(long, conflicts_with = "manage", required_unless_present = "manage")]
        post: Option<String>,
        #[arg(long)]
        manage: Option<String>,
    },
    /// Archive a room: posts are refused, reading still works (managers).
    Archive { room: String },
    /// Undo an archive (managers).
    Unarchive { room: String },
    /// Follow a room: new posts reach your inbox (at your next read, or
    /// waking you with --wake, at most once a minute per room).
    Follow {
        room: String,
        #[arg(long)]
        wake: bool,
        /// Start from `oldest` or a cursor instead of the head.
        #[arg(long)]
        from: Option<String>,
    },
    /// Stop following a room.
    Unfollow { room: String },
    /// The rooms you follow.
    Follows,
    /// Post to a room. Reports `queued` unless --wait.
    Post {
        room: String,
        #[arg(allow_hyphen_values = true)]
        text: String,
        /// Answer this post (NAMESPACE:ID or a message number).
        #[arg(long)]
        reply: Option<String>,
        /// Tag an agent by full address (PATH@NAMESPACE); at most 8.
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Wait for the outcome (confirmed, rejected, ...).
        #[arg(long)]
        wait: bool,
        /// How long --wait waits, in seconds (default 60).
        #[arg(long, requires = "wait")]
        wait_secs: Option<u64>,
    },
    /// Reply to post M in the room, or privately to its author.
    Reply {
        room: String,
        message: String,
        #[arg(allow_hyphen_values = true)]
        text: String,
        #[arg(long)]
        private: bool,
        #[arg(long, conflicts_with = "private")]
        wait: bool,
    },
    /// What became of your post M: queued, sent, unknown, confirmed,
    /// rejected, expired or unconfirmed. Checks the room at once.
    Status { message: String },
    /// Recent posts, newest last.
    History {
        room: String,
        #[arg(long)]
        limit: Option<u32>,
        /// Older than this cursor.
        #[arg(long)]
        before: Option<String>,
    },
    /// Search a room by text, author or time.
    Search {
        room: String,
        text: Option<String>,
        /// PATH@NAMESPACE.
        #[arg(long)]
        author: Option<String>,
        /// A duration back from now (1h) or RFC3339.
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// One post, and with --thread its replies.
    Show {
        room: String,
        message: String,
        #[arg(long)]
        thread: bool,
    },
}

/// The verb and JSON arguments a command sends.
pub fn request(command: RoomCommand, host: Option<String>) -> (&'static str, Value) {
    use RoomCommand as C;
    let (verb, mut args) = match command {
        C::List {
            filter,
            archived,
            limit,
        } => (
            "list",
            json!({"filter": filter, "archived": archived, "limit": limit}),
        ),
        C::Info { room } => ("info", json!({"room": room})),
        C::Create {
            name,
            desc,
            post,
            manage,
        } => (
            "create",
            json!({"name": name, "desc": desc, "post": post, "manage": manage}),
        ),
        C::Describe { room, text } => ("describe", json!({"room": room, "text": text})),
        C::Policy { room, post } => ("policy", json!({"room": room, "post": post})),
        C::Grant { room, post, manage } => (
            "grant",
            json!({"room": room, "post": post.map(|p| vec![p]), "manage": manage.map(|m| vec![m])}),
        ),
        C::Revoke { room, post, manage } => (
            "revoke",
            json!({"room": room, "post": post.map(|p| vec![p]), "manage": manage.map(|m| vec![m])}),
        ),
        C::Archive { room } => ("archive", json!({"room": room})),
        C::Unarchive { room } => ("unarchive", json!({"room": room})),
        C::Follow { room, wake, from } => {
            ("follow", json!({"room": room, "wake": wake, "from": from}))
        }
        C::Unfollow { room } => ("unfollow", json!({"room": room})),
        C::Follows => ("follows", json!({})),
        C::Post {
            room,
            text,
            reply,
            tags,
            wait,
            wait_secs,
        } => (
            "post",
            json!({"room": room, "text": text, "reply": reply, "tags": tags, "wait": wait,
                   "wait_secs": wait_secs}),
        ),
        C::Reply {
            room,
            message,
            text,
            private,
            wait,
        } => (
            "reply",
            json!({"room": room, "message": message, "text": text, "private": private,
                   "wait": wait}),
        ),
        C::Status { message } => ("status", json!({"message": message})),
        C::History {
            room,
            limit,
            before,
        } => (
            "history",
            json!({"room": room, "limit": limit, "before": before}),
        ),
        C::Search {
            room,
            text,
            author,
            since,
            until,
            limit,
        } => (
            "search",
            json!({"room": room, "text": text, "author": author, "since": since,
                   "until": until, "limit": limit}),
        ),
        C::Show {
            room,
            message,
            thread,
        } => (
            "show",
            json!({"room": room, "message": message, "thread": thread}),
        ),
    };
    let obj = args.as_object_mut().unwrap();
    obj.retain(|_, v| !v.is_null() && *v != json!(false));
    if let Some(host) = host {
        obj.insert("host".into(), json!(host));
    }
    (verb, args)
}

/// Exit codes (the build contract's machine output): 1 refused or error,
/// 2 `--wait` ran out, 4 no answer (host or relay unavailable).
pub const EXIT_WAIT: i32 = 2;
pub const EXIT_NO_ANSWER: i32 = 4;

pub async fn run(
    backend: &mut Backend,
    command: RoomCommand,
    host: Option<String>,
    json_output: bool,
) -> Result<String> {
    let (verb, args) = request(command, host);
    let value = match backend.room(verb, &args).await {
        Ok(v) => v,
        Err(err) => {
            let (v, no_answer) = error_json(&err);
            if json_output {
                // The failure is also one JSON object on stdout.
                println!("{v}");
            }
            if no_answer {
                eprintln!("bbs: {err:#}");
                std::process::exit(EXIT_NO_ANSWER);
            }
            return Err(err);
        }
    };
    let out = if json_output {
        format!("{}\n", serde_json::to_string(&value)?)
    } else {
        text(verb, &value)
    };
    // `--wait` ran out before the post settled.
    if args["wait"] == true
        && matches!(value["state"].as_str(), Some("queued" | "sent" | "unknown"))
    {
        print!("{out}");
        std::process::exit(EXIT_WAIT);
    }
    Ok(out)
}

/// The codes that mean no answer came: the host or the relay was not
/// reachable (as opposed to a refusal).
const NO_ANSWER: &[&str] = &[
    "relay_unavailable",
    "home_offline",
    "room_timeout",
    "queue_full",
    "namespace_unknown",
    "storage_unavailable",
];

/// `{"error": "<code>", "detail": "<text>"}` for a failed command, with the
/// reason code its message starts with (the host's, the relay's or this
/// server's), else `error`; and whether it means no answer.
pub fn error_json(err: &anyhow::Error) -> (Value, bool) {
    let message = err.to_string();
    let (code, detail) = match message.split_once(": ") {
        Some((code, detail))
            if !code.is_empty()
                && code.len() <= 64
                && code
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') =>
        {
            (code.to_string(), detail.to_string())
        }
        _ => ("error".to_string(), message.clone()),
    };
    let no_answer = NO_ANSWER.contains(&code.as_str()) || detail.starts_with("no answer from");
    (json!({"error": code, "detail": detail}), no_answer)
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}

/// Human-readable output for verb `verb`'s result.
pub fn text(verb: &str, v: &Value) -> String {
    let mut out = String::new();
    match verb {
        "list" => {
            let rooms = v["rooms"].as_array().cloned().unwrap_or_default();
            if rooms.is_empty() {
                out.push_str("No rooms.\n");
            }
            for r in rooms {
                let archived = if r["archived"] == true {
                    " (archived)"
                } else {
                    ""
                };
                let _ = writeln!(out, "{}{archived}  {}", s(&r, "room"), s(&r, "description"));
            }
        }
        "info" | "create" | "describe" | "policy" | "grant" | "revoke" | "archive"
        | "unarchive" => {
            match (verb, &v["created"]) {
                ("create", Value::Bool(true)) => {
                    let _ = writeln!(out, "Created {}.", s(v, "room"));
                }
                ("create", _) => {
                    let _ = writeln!(out, "{} already exists; you manage it.", s(v, "room"));
                }
                _ => {}
            }
            if v["replayed"] == true {
                out.push_str("(the host returned the stored result of an earlier attempt)\n");
            }
            info_text(&mut out, v);
        }
        "follow" => {
            let state = match s(v, "state") {
                "pending" => " (pending: it starts at the next read of the room)",
                "backfill" => " (backfilling earlier posts)",
                _ => "",
            };
            let _ = writeln!(out, "Following {} ({}){state}.", s(v, "room"), s(v, "mode"));
        }
        "unfollow" => {
            let _ = if v["removed"] == true {
                writeln!(out, "Unfollowed {}.", s(v, "room"))
            } else {
                writeln!(out, "You did not follow {}.", s(v, "room"))
            };
        }
        "follows" => {
            let follows = v["follows"].as_array().cloned().unwrap_or_default();
            if follows.is_empty() {
                out.push_str("You follow no rooms.\n");
            }
            for f in follows {
                let _ = writeln!(
                    out,
                    "{} ({}, {})",
                    s(&f, "room"),
                    s(&f, "mode"),
                    s(&f, "state")
                );
            }
        }
        "post" | "reply" | "status" if v["private"] != true => {
            let _ = write!(
                out,
                "Post {} to {}: {}",
                s(v, "ref"),
                s(v, "room"),
                s(v, "state")
            );
            if let Some(r) = v["reason"].as_str() {
                let _ = write!(out, " ({r})");
            }
            out.push_str(".\n");
            for t in v["tags"].as_array().cloned().unwrap_or_default() {
                let _ = write!(out, "  tag {}: {}", s(&t, "to"), s(&t, "state"));
                if let Some(r) = t["reason"].as_str() {
                    let _ = write!(out, " ({r})");
                }
                out.push('\n');
            }
        }
        "reply" => {
            let _ = writeln!(
                out,
                "Sent to {} privately, answering {} (message {}). Only what this \
                 installation saw is known.",
                s(v, "to"),
                s(v, "in_reply_to"),
                v["message"]
            );
        }
        "history" | "search" | "show" => {
            let posts = v["posts"].as_array().cloned().unwrap_or_default();
            if posts.is_empty() {
                out.push_str("No posts.\n");
            }
            for r in &posts {
                record_text(&mut out, r, "");
            }
            if v["scan_truncated"] == true {
                out.push_str(
                    "(the host's scan bound stopped the search; pass --before to go on)\n",
                );
            }
            if let Some(c) = v["next_cursor"].as_str()
                && !posts.is_empty()
            {
                let _ = writeln!(out, "(older: --before {c})");
            }
        }
        _ => {
            let _ = writeln!(out, "{v}");
        }
    }
    out
}

fn info_text(out: &mut String, v: &Value) {
    let list = |key: &str| {
        v[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    let post = list("post");
    let _ = writeln!(out, "room: {}", s(v, "room"));
    let _ = writeln!(out, "description: {}", s(v, "description"));
    let _ = writeln!(
        out,
        "post: {}",
        if post.is_empty() {
            "managers only"
        } else {
            &post
        }
    );
    let _ = writeln!(out, "managers: {}", list("managers"));
    let _ = writeln!(out, "archived: {}", v["archived"]);
    let _ = writeln!(out, "revision: {}", v["revision"]);
    if let Some(c) = v["creator"].as_str() {
        let _ = writeln!(out, "creator: {c}");
    }
    let recent = list("recent_authors");
    if !recent.is_empty() {
        let _ = writeln!(out, "recent posters: {recent}");
    }
}

fn record_text(out: &mut String, r: &Value, indent: &str) {
    let who = if r["kind"] == "notice" {
        "(room notice)".to_string()
    } else {
        format!("{} (reported)", s(r, "author"))
    };
    let reply = r["in_reply_to"]
        .as_str()
        .map(|x| format!(" re {x}"))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "{indent}[{}] {who} {}{reply}: {}",
        s(r, "accepted_at"),
        s(r, "ref"),
        s(r, "body")
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    fn parse(args: &[&str]) -> (&'static str, Value) {
        let cli = crate::cli::Cli::try_parse_from(args).unwrap();
        let Some(crate::cli::Command::Room { command, host, .. }) = cli.command else {
            panic!("not a room command");
        };
        request(command, host)
    }

    #[test]
    fn commands_become_verbs_with_json_arguments() {
        assert_eq!(
            parse(&[
                "bbs",
                "room",
                "create",
                "war",
                "--desc",
                "x",
                "--post",
                "ns:bbs,ns:hub"
            ]),
            (
                "create",
                json!({"name": "war", "desc": "x", "post": ["ns:bbs", "ns:hub"]})
            )
        );
        assert_eq!(
            parse(&[
                "bbs", "room", "post", "war", "-hold-", "--tag", "a@hub", "--tag", "b@bbs",
                "--wait", "--host", "lab"
            ]),
            (
                "post",
                json!({"room": "war", "text": "-hold-", "tags": ["a@hub", "b@bbs"],
                       "wait": true, "host": "lab"})
            )
        );
        assert_eq!(
            parse(&["bbs", "room", "grant", "war", "--manage", "entity:x/y@bbs"]),
            (
                "grant",
                json!({"room": "war", "manage": ["entity:x/y@bbs"]})
            )
        );
        assert_eq!(
            parse(&[
                "bbs", "room", "follow", "war", "--wake", "--from", "oldest", "--json"
            ]),
            (
                "follow",
                json!({"room": "war", "wake": true, "from": "oldest"})
            )
        );
        assert!(crate::cli::Cli::try_parse_from(["bbs", "room", "grant", "war"]).is_err());
    }

    #[test]
    fn errors_name_their_code() {
        let e = anyhow::anyhow!("not_found: no room rooms/x");
        assert_eq!(
            error_json(&e),
            (
                json!({"error": "not_found", "detail": "no room rooms/x"}),
                false
            )
        );
        let e = anyhow::anyhow!("rooms are off: no integration");
        assert_eq!(error_json(&e).0["error"], "error");
        let e = anyhow::anyhow!("home_offline: no answer from hub");
        assert!(error_json(&e).1);
    }

    #[test]
    fn text_names_authors_as_reported() {
        let out = text(
            "history",
            &json!({"posts": [
                {"accepted_at": "t", "author": "oncall@hub", "ref": "hub:1", "body": "mine",
                 "kind": "post"}
            ], "next_cursor": "abc"}),
        );
        assert_eq!(
            out,
            "[t] oncall@hub (reported) hub:1: mine\n(older: --before abc)\n"
        );
    }
}
