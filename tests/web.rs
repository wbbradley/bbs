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
        let home = std::env::temp_dir().join(format!("bbs-web-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_bbs"))
            .args(["serve", "--listen", "127.0.0.1:0", "--web", "127.0.0.1:0"])
            .arg("--require-human-token")
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

    /// One request; the whole response as (status, headers, body).
    fn request(&self, method: &str, path: &str, headers: &[&str], body: &str) -> Response {
        let mut stream = self.connect();
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
            self.web,
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
