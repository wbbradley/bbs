"""Goal continuation fixture. Only local fake JSON-RPC and a loopback board."""
import json
import sys

scenario = sys.argv[1]
thread = "goal-parent"
root = "root-1"
goal = "goal-2"
starts = 0


def out(value):
    print(json.dumps(value), flush=True)


def record(kind, **fields):
    with open("goal-wire.jsonl", "a") as f:
        f.write(json.dumps(dict(kind=kind, **fields)) + "\n")


def reply(mid, result):
    out(dict(jsonrpc="2.0", id=mid, result=result))


def note(method, **params):
    out(dict(jsonrpc="2.0", method=method, params=params))


def start(tid, **fields):
    note("turn/started", threadId=thread, turn=dict(id=tid, status="inProgress", rootTurnId=root, **fields))


def finish(tid, status="completed"):
    note("turn/completed", threadId=thread, turn=dict(id=tid, status=status))


def usage():
    note("thread/tokenUsage/updated", threadId=thread, turnId=goal,
         tokenUsage=dict(last=dict(totalTokens=88), total=dict(totalTokens=999), modelContextWindow=100))


def call(cid, tid, parent=thread):
    out(dict(jsonrpc="2.0", id=cid, method="item/tool/call", params=dict(
        threadId=parent, turnId=tid, tool="bbs_send", arguments=dict(to="observer", body=cid))))


def read_response(cid, success):
    response = json.loads(sys.stdin.readline())
    assert response["id"] == cid, response
    assert (response.get("result", {}).get("success") is True) == success, response
    record("tool", id=cid, success=success)


for line in sys.stdin:
    msg = json.loads(line)
    method, mid, p = msg.get("method"), msg.get("id"), msg.get("params", {})
    if method == "initialize":
        reply(mid, {})
    elif method == "thread/start":
        reply(mid, dict(thread=dict(id=thread)))
    elif method == "thread/resume":
        assert p["threadId"] == thread, p
        reply(mid, dict(thread=dict(id=thread)))
    elif method == "thread/goal/get":
        record("goal-read")
        if scenario == "resume-race":
            note("thread/goal/updated", threadId=thread, goal=dict(status="paused"))
        parent = "child" if scenario == "resume-foreign" else thread
        reply(mid, dict(goal=dict(threadId=parent, status="active")))
    elif method == "model/list":
        reply(mid, dict(data=[], nextCursor=None))
        record("ready")
    elif method == "turn/start":
        starts += 1
        record("prompt", number=starts)
        if starts > 1:
            reply(mid, dict(turn=dict(id="root-new", status="inProgress")))
            note("item/completed", threadId=thread, turnId="root-new", item=dict(
                type="agentMessage", id="new-text", text="Fresh prompt delivered"))
            finish("root-new")
            record("fresh-finished")
            continue
        if not scenario.startswith("resume"):
            note("thread/goal/updated", threadId=thread, goal=dict(status="active"))
        if scenario != "before-ack":
            reply(mid, dict(turn=dict(id=root, rootTurnId=root, status="inProgress")))
        start(root)
        finish(root)
        if scenario in ["resume-foreign", "resume-race"]:
            record("ordinary-finished")
            continue
        if scenario == "gap-exit":
            break
        if scenario == "gap-cancel":
            record("between")
            continue
        start("goal-1")
        call("from-goal-1", "goal-1")
        if scenario == "before-ack":
            # Both completion and the next turn's callback precede the ACK.
            reply(mid, dict(turn=dict(id=root, rootTurnId=root, status="inProgress")))
        read_response("from-goal-1", True)
        finish("goal-1")
        start(goal)
        # Neither another thread nor an old turn can call tools or speak.
        for cid, parent, tid in [("foreign", "child", goal), ("stale", thread, root)]:
            call(cid, tid, parent)
            read_response(cid, False)
            note("item/completed", threadId=parent, turnId=tid, item=dict(
                type="agentMessage", id="bad-text", text="FORBIDDEN OUTPUT"))
        call("from-goal-2", goal)
        read_response("from-goal-2", True)
        note("item/completed", threadId=thread, turnId=goal, item=dict(
            type="agentMessage", id="goal-text", text="Continuation output"))
        record("waiting", turn=goal)
    elif method == "turn/steer":
        assert p["expectedTurnId"] == goal, p
        assert "MAIL_DURING_GOAL" in p["input"][0]["text"], p
        reply(mid, dict(turnId=goal))
        record("steered", turn=goal)
        if scenario == "before-ack":
            note("thread/goal/updated", threadId=thread, goal=dict(status="complete"))
            finish(goal)
            usage()
        else:
            finish(goal)
            usage()
            # Status can also arrive after the last turn/completed.
            note("thread/goal/updated", threadId=thread, goal=dict(status="paused"))
        record("finished")
    elif method == "turn/interrupt":
        assert p["turnId"] == goal, p
        reply(mid, {})
        note("thread/goal/updated", threadId=thread, goal=dict(status="paused"))
        finish(goal, "interrupted")
        record("interrupted")
    elif method == "thread/goal/set":
        assert p["status"] == "paused" and p["origin"] == "automatic", p
        if scenario in ["pause-denied", "pause-unconfirmed", "pause-timeout"]:
            record("pause-failed", scenario=scenario)
            if scenario == "pause-denied":
                out(dict(jsonrpc="2.0", id=mid, error=dict(code=-32600, message="pause refused")))
            elif scenario == "pause-unconfirmed":
                reply(mid, dict(goal=dict(threadId=thread, status="active")))
            continue
        note("thread/goal/updated", threadId=thread, goal=dict(status="paused"))
        reply(mid, dict(goal=dict(threadId=thread, status="paused")))
        record("paused")
    elif mid is not None:
        out(dict(jsonrpc="2.0", id=mid, error=dict(code=-32601, message="unsupported")))
