"""A fake ACP agent for `bbs headless` tests.

On each prompt it starts (once) the MCP server the client passed in
session/new, asks for one permission, and replies to the last sender with
bbs_send. A prompt containing WAITCANCEL starts a "waiting-cancel" tool call,
waits for session/cancel instead, and ends the turn as cancelled; one
containing FAILTURN fails. At startup it starts a long `sleep`, standing in for
an agent's command, and writes its pid to fake-child.pid in its directory.
"""

import json
import os
import re
import subprocess
import sys
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
        if os.environ.get("FAKE_MCP_PROCESS_ENV"):
            token = os.environ.get("BBS_HEADLESS_TOKEN", "")
            assert token and token != "stale-token", "runtime token missing"
            assert not spec["env"], "MCP spec must not contain credentials"
            for pid in (os.getpid(), self.proc.pid):
                assert token.encode() not in Path(f"/proc/{pid}/cmdline").read_bytes(), "token in argv"
            with open("fake-mcp-specs.jsonl", "a") as f:
                f.write(json.dumps(spec) + "\n")
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


def main():
    child = subprocess.Popen(["sleep", "300"])
    with open("fake-child.pid", "w") as f:
        f.write(str(child.pid))
    mcp = None
    session = "fake-session"
    model_selected = False
    while (msg := read()) is not None:
        method, mid, params = msg.get("method"), msg.get("id"), msg.get("params", {})
        if method == "initialize":
            caps = {"loadSession": True}
            if os.environ.get("FAKE_ACP_RESUME"):
                caps["sessionCapabilities"] = {"resume": {}}
            out({"jsonrpc": "2.0", "id": mid, "result": {
                "protocolVersion": 1,
                "agentCapabilities": caps,
                "authMethods": [],
            }})
        elif method in ("session/new", "session/load", "session/resume"):
            mcp = Mcp(params["mcpServers"][0])
            if os.environ.get("FAKE_MCP_PROCESS_ENV"):
                with open("fake-session-methods", "a") as f:
                    f.write(method + "\n")
            session = params.get("sessionId", session)
            result = {"sessionId": session} if method == "session/new" else {}
            if os.environ.get("FAKE_MCP_PROCESS_ENV"):
                model_selected = False
                result["configOptions"] = [{"id": "model", "category": "model"}]
            out({"jsonrpc": "2.0", "id": mid, "result": result})
        elif method == "session/set_config_option":
            assert params == {"sessionId": session, "configId": "model", "value": "synthetic/model"}
            model_selected = True
            with open("fake-model-selections", "a") as f:
                f.write(params["value"] + "\n")
            out({"jsonrpc": "2.0", "id": mid, "result": {"configOptions": []}})
        elif method == "session/prompt":
            if os.environ.get("FAKE_MCP_PROCESS_ENV"):
                assert model_selected, "configured model must be reapplied before prompting"
            text = params["prompt"][0]["text"]
            sender = re.findall(r"\] from (\S+)", text)[-1]
            update(session, {"sessionUpdate": "agent_message_chunk",
                             "content": {"type": "text", "text": "thinking about " + sender}})
            if "FAILTURN" in text:
                out({"jsonrpc": "2.0", "id": mid, "error": {"code": -32603, "message": "model gone"}})
                continue
            if "WAITCANCEL" in text:
                # Tells the test the prompt has arrived, so a cancel cannot overtake it.
                update(session, {"sessionUpdate": "tool_call", "toolCallId": "w",
                                 "title": "waiting-cancel", "status": "in_progress"})
                cancel = read()
                assert cancel["method"] == "session/cancel", cancel
                out({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "cancelled"}})
                continue
            out({"jsonrpc": "2.0", "id": "perm", "method": "session/request_permission",
                 "params": {"sessionId": session, "toolCall": {"toolCallId": "t1", "title": "reply"},
                            "options": [{"optionId": "yes", "name": "Allow", "kind": "allow_once"}]}})
            choice = read()["result"]["outcome"]["optionId"]
            mcp.call("tools/call", {"name": "bbs_send",
                                    "arguments": {"to": sender, "body": "echo (" + choice + ")"}})
            out({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
        elif mid is not None:
            out({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": method}})


main()
