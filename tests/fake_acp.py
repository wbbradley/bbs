"""A fake ACP agent for `bbs headless` tests.

On each prompt it starts (once) the MCP server the client passed in
session/new, asks for one permission, and replies to the last sender with
bbs_send. A prompt containing WAITCANCEL waits for session/cancel instead and
ends the turn as cancelled.
"""

import json
import os
import re
import subprocess
import sys


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


def main():
    mcp = None
    session = "fake-session"
    while (msg := read()) is not None:
        method, mid, params = msg.get("method"), msg.get("id"), msg.get("params", {})
        if method == "initialize":
            out({"jsonrpc": "2.0", "id": mid, "result": {
                "protocolVersion": 1,
                "agentCapabilities": {"loadSession": True},
                "authMethods": [],
            }})
        elif method in ("session/new", "session/load"):
            mcp = Mcp(params["mcpServers"][0])
            session = params.get("sessionId", session)
            result = {"sessionId": session} if method == "session/new" else {}
            out({"jsonrpc": "2.0", "id": mid, "result": result})
        elif method == "session/prompt":
            text = params["prompt"][0]["text"]
            sender = re.findall(r"\] from (\S+)", text)[-1]
            update(session, {"sessionUpdate": "agent_message_chunk",
                             "content": {"type": "text", "text": "thinking about " + sender}})
            if "WAITCANCEL" in text:
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
