"""A fake `codex app-server` for `bbs headless` tests.

Each turn replies to the last sender with the bbs_send dynamic tool. A prompt
containing WAITSTEER waits for turn/steer and replies to the steered mail
instead; one containing WAITCANCEL waits for turn/interrupt and ends the turn
as interrupted.
"""

import json
import re
import sys


def out(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else None


def notify(method, params):
    out({"jsonrpc": "2.0", "method": method, "params": params})


def send(thread, turn, to, body):
    """Call bbs_send through the client and return its reply."""
    out({"jsonrpc": "2.0", "id": "call-" + turn, "method": "item/tool/call",
         "params": {"callId": "c", "threadId": thread, "turnId": turn, "tool": "bbs_send",
                    "namespace": None, "arguments": {"to": to, "body": body}}})
    reply = read()
    assert reply["id"] == "call-" + turn and reply["result"]["success"], reply
    return reply


def sender(text):
    return re.findall(r"\] from (\S+)", text)[-1]


def main():
    thread = "fake-thread"
    turns = 0
    while (msg := read()) is not None:
        method, mid, params = msg.get("method"), msg.get("id"), msg.get("params", {})
        if method == "initialize":
            assert params["capabilities"]["experimentalApi"], params
            out({"jsonrpc": "2.0", "id": mid, "result": {}})
        elif method == "thread/start":
            tools = {t["name"]: t for t in params["dynamicTools"]}
            assert tools["bbs_send"]["type"] == "function", tools
            assert params["approvalPolicy"] == "never", params
            assert params["sandbox"] == "danger-full-access", params
            assert params["modelProvider"] == "fakeprovider", params
            out({"jsonrpc": "2.0", "id": mid, "result": {"thread": {"id": thread}}})
        elif method == "turn/start":
            turns += 1
            turn = "turn-%d" % turns
            text = params["input"][0]["text"]
            out({"jsonrpc": "2.0", "id": mid,
                 "result": {"turn": {"id": turn, "status": "inProgress", "items": []}}})
            notify("turn/started", {"threadId": thread, "turn": {"id": turn}})
            notify("item/agentMessage/delta",
                   {"threadId": thread, "turnId": turn, "itemId": "m", "delta": "thinking"})
            status = "completed"
            if "WAITSTEER" in text:
                steer = read()
                assert steer["method"] == "turn/steer", steer
                assert steer["params"]["expectedTurnId"] == turn, steer
                out({"jsonrpc": "2.0", "id": steer["id"], "result": {"turnId": turn}})
                steered = steer["params"]["input"][0]["text"]
                send(thread, turn, sender(steered), "steered: " + steered.splitlines()[-1])
            elif "WAITCANCEL" in text:
                interrupt = read()
                assert interrupt["method"] == "turn/interrupt", interrupt
                assert interrupt["params"]["turnId"] == turn, interrupt
                out({"jsonrpc": "2.0", "id": interrupt["id"], "result": {}})
                status = "interrupted"
            else:
                send(thread, turn, sender(text), "echo")
            notify("turn/completed", {"threadId": thread,
                                      "turn": {"id": turn, "status": status, "items": []}})
        elif mid is not None:
            out({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": method}})


main()
