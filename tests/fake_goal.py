"""A fake ACP agent for `bbs headless` goal tests.

It records every prompt's text in fake-goal-prompts.jsonl. A prompt carrying a
goal block ("[bbs goal") runs the next action from FAKE_GOAL_SCRIPT, a
comma-separated list; the position is kept in fake-goal-step so it survives a
restart. Once the script runs out, it makes no tool calls. Actions:

  tool           make one tool call that is not a goal update
  slow           take a fifth of a second, then make one tool call
  idle           make no tool call
  complete-bare  mark the goal complete without evidence (refused)
  complete       mark the goal complete with evidence
  wait1s         wait for one second
  wait2s         wait for two seconds
  block          put a blocker in @crew and block the goal on it
  waitcancel     wait for session/cancel, then end the turn cancelled

Every bbs_goal_update result is appended to fake-goal-results.jsonl.
"""

import json
import os
import subprocess
import sys
import time
from pathlib import Path


def out(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else None


class Mcp:
    def __init__(self, spec):
        env = dict(os.environ)
        env.update({e["name"]: e["value"] for e in spec["env"]})
        self.proc = subprocess.Popen(
            [spec["command"]] + spec["args"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            env=env,
        )
        self.next_id = 0
        self.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}})

    def call(self, method, params):
        self.next_id += 1
        msg = {"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()
        return json.loads(self.proc.stdout.readline())["result"]


def update(session, body):
    out({"jsonrpc": "2.0", "method": "session/update",
         "params": {"sessionId": session, "update": body}})


def next_action():
    script = [a for a in os.environ.get("FAKE_GOAL_SCRIPT", "").split(",") if a]
    step = Path("fake-goal-step")
    n = int(step.read_text()) if step.exists() else 0
    step.write_text(str(n + 1))
    return script[n] if n < len(script) else "idle"


def main():
    mcp = None
    session = "fake-session"
    calls = 0
    while (msg := read()) is not None:
        method, mid, params = msg.get("method"), msg.get("id"), msg.get("params", {})
        if method == "initialize":
            out({"jsonrpc": "2.0", "id": mid, "result": {
                "protocolVersion": 1, "agentCapabilities": {"loadSession": True}, "authMethods": [],
            }})
        elif method in ("session/new", "session/load"):
            mcp = Mcp(params["mcpServers"][0])
            session = params.get("sessionId", session)
            out({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": session} if method == "session/new" else {}})
        elif method == "session/prompt":
            text = params["prompt"][0]["text"]
            with open("fake-goal-prompts.jsonl", "a") as f:
                f.write(json.dumps(text) + "\n")
            action = next_action() if "[bbs goal" in text else "idle"

            def tool(name, arguments):
                nonlocal calls
                calls += 1
                update(session, {"sessionUpdate": "tool_call", "toolCallId": f"c{calls}",
                                 "title": name, "status": "completed"})
                if not name.startswith("bbs_"):
                    return None
                result = mcp.call("tools/call", {"name": name, "arguments": arguments})
                if name == "bbs_goal_update":
                    with open("fake-goal-results.jsonl", "a") as f:
                        f.write(json.dumps(result) + "\n")
                return result

            if action == "tool":
                tool("ls", {})
            elif action == "slow":
                time.sleep(0.2)
                tool("ls", {})
            elif action == "complete-bare":
                tool("bbs_goal_update", {"status": "complete"})
            elif action == "complete":
                tool("bbs_goal_update", {"status": "complete", "evidence": "tests pass"})
            elif action == "wait1s":
                tool("bbs_goal_update", {"status": "waiting", "for": "1s", "reason": "deploy"})
            elif action == "wait2s":
                tool("bbs_goal_update", {"status": "waiting", "for": "2s", "reason": "deploy"})
            elif action == "block":
                put = tool("bbs_blocker_put", {"group": "crew", "body": "Which branch?",
                                               "request_key": f"block-{calls}"})
                blocker = json.loads(put["content"][0]["text"])["blocker"]["id"]
                tool("bbs_goal_update", {"status": "blocked", "blocker": blocker,
                                         "reason": "which branch?"})
            elif action == "waitcancel":
                update(session, {"sessionUpdate": "tool_call", "toolCallId": "w",
                                 "title": "waiting-cancel", "status": "in_progress"})
                cancel = read()
                assert cancel["method"] == "session/cancel", cancel
                out({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "cancelled"}})
                continue
            out({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
        elif mid is not None:
            out({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": method}})


main()
