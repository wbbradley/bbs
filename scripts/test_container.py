#!/usr/bin/env python3
"""Exercise inbox sorting and logging in an isolated, restricted BBS container."""

import argparse
from contextlib import closing
from datetime import datetime, timedelta, timezone
import os
from pathlib import Path
import json
import re
import sqlite3
import subprocess
import tempfile
import time
from urllib.parse import quote
from urllib.request import Request, urlopen


def run(*args, env=None):
    result = subprocess.run(args, env=env, capture_output=True, text=True, timeout=60)
    if result.returncode:
        raise RuntimeError(f"{args[0]} failed: {result.stderr.strip()}")
    return result.stdout


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def check_web(address, token):
    # Use the configured public origin while connecting only to Docker's
    # ephemeral loopback mapping. No proxy or production endpoint is involved.
    origin = "http://bbs-container.test"
    headers = {"Host": "bbs-container.test", "Origin": origin, "x-bbs-web": "1"}
    base = f"http://{address}"

    def request(path, body=None, cookie=None):
        values = dict(headers)
        if cookie:
            values["Cookie"] = cookie
        if body is not None:
            values["Content-Type"] = "application/json"
        return urlopen(Request(
            base + path, data=None if body is None else json.dumps(body).encode(),
            headers=values,
        ), timeout=5)

    with request("/healthz") as response:
        require(response.status == 200, "HTTP listener is not ready")
    with request("/") as response:
        page = response.read().decode()
        require("text/html" in response.headers.get("Content-Type", ""), "missing HTML UI")
    assets = re.findall(r'(?:src|href)="(/assets/[^\"]+)"', page)
    require(any(path.endswith(".js") for path in assets), "index has no bundled JavaScript")
    for path in assets:
        with request(path) as response:
            require(response.status == 200 and bool(response.read()), "missing bundled asset")
    with request("/api/info") as response:
        require(bool(json.load(response).get("release")), "HTTP server has no release identity")
    with request("/api/login", {"name": "webtester", "token": token}) as response:
        cookie = response.headers["Set-Cookie"]
        require("HttpOnly" in cookie and "SameSite=Strict" in cookie, "unsafe session cookie")
        cookie = cookie.split(";", 1)[0]
    with request("/api/rpc/whoami", {}, cookie) as response:
        require(json.load(response)["name"] == "webtester", "wrong HTTP identity")
    subscriptions = quote(json.dumps([{"key": "summary", "kind": "summary", "q": {}}]))
    with request("/api/events?tab=container-test&s=" + subscriptions, cookie=cookie) as response:
        require("text/event-stream" in response.headers.get("Content-Type", ""), "missing SSE")
        require("no-store" in response.headers.get("Cache-Control", ""), "cached SSE")
        while True:
            line = response.readline()
            require(bool(line), "SSE closed without a snapshot")
            if line.startswith(b"data:"):
                event = json.loads(line[5:])
                require(event["key"] == "summary" and "snapshot" in event, "wrong SSE snapshot")
                break
    with request("/api/logout", {}, cookie) as response:
        require(response.status == 204, "HTTP logout failed")


def check(image, bbs):
    image_user = run("docker", "image", "inspect", "--format", "{{.Config.User}}", image).strip()
    require(image_user == "999:999", f"expected image USER 999:999, got {image_user!r}")
    with tempfile.TemporaryDirectory(prefix="bbs-container-test-") as temporary:
        env = {key: value for key, value in os.environ.items() if not key.startswith("BBS_")}
        env["HOME"] = temporary
        run(bbs, "init", "--human", "tester", env=env)
        state = Path(temporary, ".local/state/bbs")
        database = state / "bbs.db"
        token = run(bbs, "admin", "--state-dir", str(state), "human", "add", "webtester", env=env).strip()
        now = datetime.now(timezone.utc)
        with closing(sqlite3.connect(database)) as conn, conn:
            recipient = conn.execute("SELECT id FROM humans WHERE name = 'tester'").fetchone()[0]
            sender = conn.execute(
                "INSERT INTO humans (name, created_at, active_at) VALUES ('sender', ?1, ?1)",
                (now.isoformat(),),
            ).lastrowid
            for number in range(200):
                body = f"container-regression-{number:03d}\n" + "x" * 60000
                sent = (now + timedelta(microseconds=number)).isoformat()
                message = conn.execute(
                    "INSERT INTO messages (from_kind, from_id, body, created_at) VALUES ('human', ?, ?, ?)",
                    (sender, body, sent),
                ).lastrowid
                conn.execute(
                    "INSERT INTO deliveries (message_id, recipient_kind, recipient_id) VALUES (?, 'human', ?)",
                    (message, recipient),
                )
        # The fixture belongs to the host user; the image runs as uid 999.
        state.chmod(0o777)
        database.chmod(0o666)
        container = run(
            "docker", "run", "--detach", "--read-only",
            "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
            "--publish", "127.0.0.1::7777",
            "--publish", "127.0.0.1::7778",
            "--mount", f"type=bind,src={state},dst=/var/lib/bbs",
            image,
            "--listen", "0.0.0.0:7777", "--web", "0.0.0.0:7778",
            "--web-origin", "http://bbs-container.test",
        ).strip()
        try:
            env["BBS_SERVER"] = run("docker", "port", container, "7777/tcp").strip()
            deadline = time.monotonic() + 15
            while True:
                try:
                    require(run(bbs, "--as", "tester", "whoami", env=env) == "tester\n", "wrong server identity")
                    break
                except RuntimeError:
                    if time.monotonic() >= deadline:
                        raise
                    time.sleep(0.1)
            web = run("docker", "port", container, "7778/tcp").strip()
            check_web(web, token)
            # Each response stays below gRPC's message limit while sorting enough
            # data to force SQLite's temporary B-tree to spill to disk.
            for batch in range(4):
                wait = ["--wait"] if batch == 0 else []
                output = run(bbs, "--as", "tester", "read", "--limit", "50", *wait, env=env)
                require(output.count("container-regression-") == 50, "expected 50 unread messages")
                for number in range(batch * 50, (batch + 1) * 50):
                    require(f"container-regression-{number:03d}\n" in output, f"missing message {number}")
            require(run(bbs, "--as", "tester", "read", env=env) == "", "inbox was not drained")
            history = run(bbs, "--as", "tester", "read", "--archived", "--limit", "50", env=env)
            require(history.count("container-regression-") == 50, "expected 50 archived messages")
            logs = subprocess.run(
                ["docker", "logs", container], capture_output=True, text=True, check=True, timeout=10,
            )
            require("serving" in logs.stderr, "server tracing must reach stderr")
            require(not (state / "bbs.log").exists(), "server must not write a log file")
        except Exception:
            subprocess.run(["docker", "logs", container], check=False, timeout=10)
            raise
        finally:
            subprocess.run(["docker", "rm", "--force", container], check=True, capture_output=True, timeout=15)
    print("Restricted container: gRPC reads/logging and HTTP UI/assets/auth/SSE passed.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image", help="locally built BBS image")
    parser.add_argument("--bbs", default="target/debug/bbs", help="host BBS client binary")
    args = parser.parse_args()
    check(args.image, str(Path(args.bbs).resolve()))
