//! Drive `bbs serve --web` over plain HTTP: sign-in, the JSON API and streams.
#![cfg(feature = "web")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Server {
    child: Child,
    home: PathBuf,
    web: String,
}

impl Server {
    fn start(test: &str) -> Server {
        Server::start_with(test, &[])
    }

    fn start_with(test: &str, args: &[&str]) -> Server {
        let home = std::env::temp_dir().join(format!("bbs-web-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_bbs"))
            .args(["serve", "--listen", "127.0.0.1:0", "--web", "127.0.0.1:0"])
            .arg("--require-human-token")
            .args(args)
            .env("HOME", &home)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        // The gRPC line, then the web line.
        out.read_line(&mut line).unwrap();
        line.clear();
        out.read_line(&mut line).unwrap();
        let web = line
            .trim()
            .rsplit("http://")
            .next()
            .expect("web line")
            .to_string();
        Server { child, home, web }
    }

    /// A new human's token, issued as an administrator would.
    fn human(&self, name: &str) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_bbs"))
            .args(["admin", "human", "add", name])
            .env("HOME", &self.home)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn connect(&self) -> TcpStream {
        let stream = TcpStream::connect(&self.web).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
    }

    /// One request, as the page sends it (a POST names its origin); the
    /// whole response.
    fn request(&self, method: &str, path: &str, headers: &[&str], body: &str) -> Response {
        let origin = format!("origin: http://{}", self.web);
        let mut all = headers.to_vec();
        if method == "POST" {
            all.push(&origin);
        }
        self.raw(method, path, &self.web, &all, body)
    }

    /// One request exactly as given, to `host`.
    fn raw(&self, method: &str, path: &str, host: &str, headers: &[&str], body: &str) -> Response {
        let mut stream = self.connect();
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        for header in headers {
            head.push_str(header);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body.as_bytes()).unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        Response::parse(&text)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Response {
    fn parse(text: &str) -> Response {
        let (head, body) = text.split_once("\r\n\r\n").expect("response head");
        let mut lines = head.lines();
        let status = lines
            .next()
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        Response {
            status,
            headers,
            body: body.to_string(),
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }
}

const CSRF: &str = "x-bbs-web: 1";

fn sign_in(server: &Server, name: &str, token: &str) -> String {
    let body = serde_json::json!({ "name": name, "token": token }).to_string();
    let r = server.request("POST", "/api/login", &[CSRF], &body);
    assert_eq!(r.status, 200, "{}", r.body);
    let cookie = r.header("set-cookie").expect("a session cookie");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    format!("cookie: {}", cookie.split(';').next().unwrap())
}

#[test]
fn humans_sign_in_with_their_token_and_use_the_api() {
    let server = Server::start("sign-in");
    let token = server.human("ada");

    let public = server.request("GET", "/api/info", &[], "");
    assert_eq!(public.status, 200);
    assert_eq!(public.json()["require_human_token"], true);

    let wrong = serde_json::json!({ "name": "ada", "token": "nope" }).to_string();
    let r = server.request("POST", "/api/login", &[CSRF], &wrong);
    assert_eq!(r.status, 401, "{}", r.body);
    assert!(r.header("set-cookie").is_none());

    // A name that is not the token's own is refused, not corrected.
    let other = serde_json::json!({ "name": "bob", "token": token }).to_string();
    assert_eq!(
        server.request("POST", "/api/login", &[CSRF], &other).status,
        401
    );

    let cookie = sign_in(&server, "ADA", &token);
    let me = server.request("POST", "/api/rpc/whoami", &[CSRF, &cookie], "");
    assert_eq!(me.status, 200, "{}", me.body);
    assert_eq!(me.json()["name"], "ada");
    assert_eq!(me.json()["kind"], 2);

    let created = server.request(
        "POST",
        "/api/rpc/group_register",
        &[CSRF, &cookie],
        r#"{"group":"core","description":"Core work"}"#,
    );
    assert_eq!(created.status, 200, "{}", created.body);
    let groups = server.request("POST", "/api/rpc/group_list", &[CSRF, &cookie], "{}");
    assert_eq!(groups.json()["groups"][0]["name"], "core");
    assert_eq!(groups.json()["groups"][0]["description"], "Core work");

    // Service refusals keep their message.
    let refused = server.request(
        "POST",
        "/api/rpc/send",
        &[CSRF, &cookie],
        r#"{"to":"nobody","body":"hi"}"#,
    );
    assert_eq!(refused.status, 400, "{}", refused.body);
    assert!(refused.json()["error"].as_str().unwrap().contains("nobody"));

    let unknown = server.request("POST", "/api/rpc/register", &[CSRF, &cookie], "{}");
    assert_eq!(
        unknown.status, 404,
        "agent RPCs are not offered: {}",
        unknown.body
    );

    let out = server.request("POST", "/api/logout", &[CSRF, &cookie], "");
    assert_eq!(out.status, 204);
    assert!(out.header("set-cookie").unwrap().contains("Max-Age=0"));
}

#[test]
fn calls_need_the_csrf_header_and_a_valid_session() {
    let server = Server::start("guards");
    let token = server.human("ada");
    let cookie = sign_in(&server, "ada", &token);

    let r = server.request("POST", "/api/rpc/whoami", &[&cookie], "");
    assert_eq!(r.status, 403, "{}", r.body);
    let r = server.request("POST", "/api/login", &[], "{}");
    assert_eq!(r.status, 403, "{}", r.body);

    let r = server.request("POST", "/api/rpc/whoami", &[CSRF], "");
    assert_eq!(r.status, 401, "{}", r.body);

    // Rotation takes effect at once: the old cookie no longer works.
    let rotated = Command::new(env!("CARGO_BIN_EXE_bbs"))
        .args(["admin", "human", "rotate", "ada"])
        .env("HOME", &server.home)
        .output()
        .unwrap();
    assert!(rotated.status.success());
    let r = server.request("POST", "/api/rpc/whoami", &[CSRF, &cookie], "");
    assert_eq!(r.status, 401, "{}", r.body);
}

#[test]
fn watch_streams_arrive_as_server_sent_events() {
    let server = Server::start("watch");
    let token = server.human("ada");
    let cookie = sign_in(&server, "ada", &token);

    let mut stream = server.connect();
    let q = "%7B%22backlog%22%3A10%7D";
    write!(
        stream,
        "GET /api/watch/feed?q={q} HTTP/1.1\r\nHost: {}\r\n{cookie}\r\n\r\n",
        server.web
    )
    .unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("HTTP/1.1 200"), "{line}");
    let first = loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if let Some(data) = line.strip_prefix("data: ") {
            break data.trim().to_string();
        }
    };
    let snapshot: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(snapshot["messages"], serde_json::json!([]));

    // A change sends the next snapshot.
    let sent = server.request(
        "POST",
        "/api/rpc/group_register",
        &[CSRF, &cookie],
        r#"{"group":"core"}"#,
    );
    assert_eq!(sent.status, 200);
    let r = server.request(
        "POST",
        "/api/rpc/send",
        &[CSRF, &cookie],
        r#"{"to":"@core","body":"hello from the web"}"#,
    );
    assert_eq!(r.status, 200, "{}", r.body);
    // Registering the group was a change too; its snapshot may come first.
    loop {
        line.clear();
        assert!(reader.read_line(&mut line).unwrap() > 0, "stream ended");
        if line.starts_with("data: ") && line.contains("hello from the web") {
            break;
        }
    }

    // Without a session the stream is refused before it opens.
    let r = server.request("GET", &format!("/api/watch/feed?q={q}"), &[], "");
    assert_eq!(r.status, 401);
}

#[test]
fn the_page_is_served_for_client_routes() {
    let server = Server::start("page");
    let r = server.request("GET", "/agents/Mateo", &[], "");
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.header("content-type").unwrap().starts_with("text/html"));
    assert!(r.header("content-security-policy").is_some());
    let r = server.request("GET", "/api/nothing", &[], "");
    assert_eq!(r.status, 404);
}

/// An open event stream: its `event`/`data` pairs, one at a time.
struct Sse {
    reader: BufReader<TcpStream>,
}

impl Sse {
    fn open(server: &Server, path: &str, cookie: &str) -> Sse {
        let mut stream = server.connect();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {}\r\n{cookie}\r\n\r\n",
            server.web
        )
        .unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.starts_with("HTTP/1.1 200"), "{line}");
        let mut cache = None;
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some((k, v)) = line.split_once(':')
                && k.eq_ignore_ascii_case("cache-control")
            {
                cache = Some(v.trim().to_string());
            }
        }
        assert_eq!(cache.as_deref(), Some("no-store"));
        Sse { reader }
    }

    /// The next event as (name, data), or None when the stream ends.
    fn next(&mut self) -> Option<(String, serde_json::Value)> {
        let mut name = "message".to_string();
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line).ok()?;
            if n == 0 {
                return None;
            }
            // Chunked framing lines carry no field.
            if let Some(event) = line.strip_prefix("event: ") {
                name = event.trim().to_string();
            } else if let Some(data) = line.strip_prefix("data: ") {
                return Some((name, serde_json::from_str(data.trim()).unwrap()));
            }
        }
    }
}

fn events_path(subscriptions: serde_json::Value) -> String {
    let s: String = subscriptions
        .to_string()
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("/api/events?s={s}")
}

#[test]
fn logging_out_ends_the_session_for_every_copy_of_the_cookie() {
    let server = Server::start("logout");
    let token = server.human("ada");
    let cookie = sign_in(&server, "ada", &token);
    // The cookie is a session id, not the credential.
    assert!(!cookie.contains(&token));
    let encoded = data_encoding::BASE64URL_NOPAD.encode(token.as_bytes());
    assert!(!cookie.contains(&encoded));
    assert!(cookie.starts_with("cookie: bbs_session="), "{cookie}");
    let r = server.request("POST", "/api/logout", &[CSRF, &cookie], "");
    assert_eq!(r.status, 204);
    // A copy kept from before signing out is worthless.
    let r = server.request("POST", "/api/rpc/whoami", &[CSRF, &cookie], "");
    assert_eq!(r.status, 401, "{}", r.body);
    // Signing in again starts a different session.
    let again = sign_in(&server, "ada", &token);
    assert_ne!(again, cookie);
    let r = server.request("POST", "/api/rpc/whoami", &[CSRF, &again], "");
    assert_eq!(r.status, 200, "{}", r.body);
}

#[test]
fn a_rotated_token_ends_the_session_and_its_open_streams() {
    let server = Server::start("rotate-stream");
    let token = server.human("ada");
    let cookie = sign_in(&server, "ada", &token);
    let path = events_path(serde_json::json!([
        {"key": "feed", "kind": "feed", "q": {"backlog": 10}}
    ]));
    let mut sse = Sse::open(&server, &path, &cookie);
    let (name, data) = sse.next().expect("the first snapshot");
    assert_eq!(
        (name.as_str(), &data["key"]),
        ("message", &serde_json::json!("feed"))
    );
    let rotated = Command::new(env!("CARGO_BIN_EXE_bbs"))
        .args(["admin", "human", "rotate", "ada"])
        .env("HOME", &server.home)
        .output()
        .unwrap();
    assert!(rotated.status.success());
    // The stream says the session ended, then ends.
    let ended = std::iter::from_fn(|| sse.next()).find(|(name, _)| name == "fail");
    let (_, data) = ended.expect("a fail event");
    assert_eq!(data["key"], serde_json::Value::Null, "{data}");
    assert_eq!(data["status"], 401, "{data}");
    assert!(sse.next().is_none());
    let r = server.request("POST", "/api/rpc/whoami", &[CSRF, &cookie], "");
    assert_eq!(r.status, 401, "{}", r.body);
    assert!(r.header("set-cookie").unwrap().contains("Max-Age=0"));
}

#[test]
fn other_hosts_and_origins_are_refused() {
    let server = Server::start("boundary");
    let token = server.human("ada");
    let cookie = sign_in(&server, "ada", &token);
    let login = serde_json::json!({ "name": "ada", "token": token }).to_string();
    // A rebound DNS name reaches the port but not the API or the page.
    let r = server.raw(
        "POST",
        "/api/login",
        "evil.example",
        &[CSRF, "origin: http://evil.example"],
        &login,
    );
    assert_eq!(r.status, 421, "{}", r.body);
    let r = server.raw("GET", "/", "evil.example", &[], "");
    assert_eq!(r.status, 421, "{}", r.body);
    // Another origin, or none, may not call.
    let r = server.raw(
        "POST",
        "/api/login",
        &server.web,
        &[CSRF, "origin: http://localhost:1"],
        &login,
    );
    assert_eq!(r.status, 403, "{}", r.body);
    let r = server.raw("POST", "/api/rpc/whoami", &server.web, &[CSRF, &cookie], "");
    assert_eq!(r.status, 403, "{}", r.body);
    let r = server.raw(
        "GET",
        "/api/info",
        &server.web,
        &["sec-fetch-site: cross-site"],
        "",
    );
    assert_eq!(r.status, 403, "{}", r.body);
    let r = server.raw(
        "GET",
        "/api/info",
        &server.web,
        &["sec-fetch-site: same-origin"],
        "",
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.header("cache-control"), Some("no-store"));
    // localhost is the same loopback server.
    let port = server.web.rsplit(':').next().unwrap();
    let local = format!("localhost:{port}");
    let origin = format!("origin: http://{local}");
    let r = server.raw(
        "POST",
        "/api/rpc/whoami",
        &local,
        &[CSRF, &cookie, &origin],
        "",
    );
    assert_eq!(r.status, 200, "{}", r.body);
    // Readiness needs no session, but the same host as everything else.
    let r = server.raw("GET", "/healthz", &server.web, &[], "");
    assert_eq!(r.status, 200, "{}", r.body);
    let r = server.raw("GET", "/healthz", "10.0.0.7:7778", &[], "");
    assert_eq!(r.status, 421, "{}", r.body);
    // The page forbids a base URL and foreign form posts.
    let r = server.request("GET", "/", &[], "");
    let csp = r.header("content-security-policy").unwrap();
    assert!(
        csp.contains("base-uri 'none'") && csp.contains("form-action 'self'"),
        "{csp}"
    );
}

#[test]
fn one_event_stream_carries_several_watches() {
    let server = Server::start("events");
    let token = server.human("ada");
    let cookie = sign_in(&server, "ada", &token);
    let path = events_path(serde_json::json!([
        {"key": "feed", "kind": "feed", "q": {"backlog": 10}},
        {"key": "summary", "kind": "summary", "q": {}},
        {"key": "nope", "kind": "bogus"}
    ]));
    let mut sse = Sse::open(&server, &path, &cookie);
    let mut seen = std::collections::BTreeMap::new();
    while seen.len() < 3 {
        let (name, data) = sse.next().expect("an event");
        seen.insert(data["key"].as_str().unwrap().to_string(), (name, data));
    }
    assert_eq!(seen["feed"].0, "message");
    assert_eq!(
        seen["feed"].1["snapshot"]["messages"],
        serde_json::json!([])
    );
    assert_eq!(seen["summary"].0, "message");
    // A bad watch fails alone; the others go on.
    assert_eq!(seen["nope"].0, "fail");
    assert_eq!(seen["nope"].1["status"], 404);
    server.request(
        "POST",
        "/api/rpc/group_register",
        &[CSRF, &cookie],
        r#"{"group":"core"}"#,
    );
    let r = server.request(
        "POST",
        "/api/rpc/send",
        &[CSRF, &cookie],
        r#"{"to":"@core","body":"over one stream"}"#,
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let update = std::iter::from_fn(|| sse.next())
        .find(|(_, d)| d["key"] == "feed" && d.to_string().contains("over one stream"));
    assert!(update.is_some());
    // Bounds.
    let many: Vec<_> = (0..9)
        .map(|i| serde_json::json!({"key": format!("k{i}"), "kind": "summary"}))
        .collect();
    let r = server.request("GET", &events_path(serde_json::json!(many)), &[&cookie], "");
    assert_eq!(r.status, 400, "{}", r.body);
    let twice =
        serde_json::json!([{"key": "a", "kind": "summary"}, {"key": "a", "kind": "summary"}]);
    let r = server.request("GET", &events_path(twice), &[&cookie], "");
    assert_eq!(r.status, 400, "{}", r.body);
}

#[test]
fn an_https_origin_makes_the_cookie_secure_and_host_only() {
    let server = Server::start_with("secure", &["--web-origin", "https://bbs.example.com"]);
    let token = server.human("ada");
    let body = serde_json::json!({ "name": "ada", "token": token }).to_string();
    let r = server.raw(
        "POST",
        "/api/login",
        "bbs.example.com",
        &[CSRF, "origin: https://bbs.example.com"],
        &body,
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let cookie = r.header("set-cookie").unwrap();
    assert!(cookie.starts_with("__Host-bbs_session="), "{cookie}");
    assert!(
        cookie.contains("; Secure") && cookie.contains("Path=/"),
        "{cookie}"
    );
    // A forwarded-protocol header no longer decides anything.
    let plain = Server::start("plain-xfp");
    let token = plain.human("ada");
    let body = serde_json::json!({ "name": "ada", "token": token }).to_string();
    let r = plain.request(
        "POST",
        "/api/login",
        &[CSRF, "x-forwarded-proto: https"],
        &body,
    );
    assert!(!r.header("set-cookie").unwrap().contains("Secure"));
}

#[test]
fn a_network_address_needs_an_origin() {
    let home = std::env::temp_dir().join(format!("bbs-web-{}-origin", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_bbs"))
        .args(["serve", "--listen", "127.0.0.1:0", "--web", "0.0.0.0:0"])
        .env("HOME", &home)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&home);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--web-origin"), "{err}");
}

#[cfg(unix)]
#[test]
fn stopping_ends_open_streams_and_exits_promptly() {
    let mut server = Server::start("stop");
    let token = server.human("ada");
    let cookie = sign_in(&server, "ada", &token);
    let path = events_path(serde_json::json!([{"key": "s", "kind": "summary"}]));
    let mut sse = Sse::open(&server, &path, &cookie);
    assert!(sse.next().is_some());
    let started = std::time::Instant::now();
    let kill = Command::new("kill")
        .args(["-TERM", &server.child.id().to_string()])
        .status()
        .unwrap();
    assert!(kill.success());
    // The stream ends rather than hold the server open.
    while sse.next().is_some() {}
    let status = loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            break status;
        }
        assert!(started.elapsed() < Duration::from_secs(8), "still running");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{status:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}
