"""A fake `codex app-server` that records how bbs opens its thread.

Each request is appended to the file named by argv[1]. thread/resume fails
for a thread id starting with "gone"; thread/start opens "native-thread".
No turns are run.
"""

import json
import sys

record = open(sys.argv[1], "a", buffering=1)


def out(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    msg = json.loads(line)
    method, mid, params = msg.get("method"), msg.get("id"), msg.get("params", {})
    if method is None:
        continue
    record.write(json.dumps({"method": method, "params": params}) + "\n")
    if mid is None:
        continue
    if method == "initialize":
        out({"jsonrpc": "2.0", "id": mid, "result": {}})
    elif method == "thread/resume":
        if params["threadId"].startswith("gone"):
            out({"jsonrpc": "2.0", "id": mid,
                 "error": {"code": -32600, "message": "no rollout found"}})
        else:
            out({"jsonrpc": "2.0", "id": mid, "result": {"thread": {"id": params["threadId"]}}})
    elif method == "thread/start":
        out({"jsonrpc": "2.0", "id": mid, "result": {"thread": {"id": "native-thread"}}})
    elif method == "model/list":
        out({"jsonrpc": "2.0", "id": mid, "result": {"data": [], "nextCursor": None}})
    else:
        out({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "unsupported"}})
