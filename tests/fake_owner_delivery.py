"""Synthetic owner-chat protocol; no accounts, external tools or model calls."""
import json
import os
import subprocess
import sys
import time
from pathlib import Path

kind = sys.argv[1]
session = 'parent'
turn = 0
mcp = None


def out(value):
    print(json.dumps(value), flush=True)


def reply(mid, value):
    out(dict(jsonrpc='2.0', id=mid, result=value))


def note(method, value):
    out(dict(jsonrpc='2.0', method=method, params=value))


def text(body, message='message', parent=session):
    if kind == 'acp':
        note('session/update', dict(sessionId=parent, update=dict(
            sessionUpdate='agent_message_chunk', messageId=message,
            content=dict(type='text', text=body))))
    else:
        note('item/agentMessage/delta', dict(threadId=parent, turnId=str(turn), itemId=message, delta=body))


def tool():
    if kind == 'acp':
        note('session/update', dict(sessionId=session, update=dict(
            sessionUpdate='tool_call', toolCallId='boundary', title='boundary', status='completed')))
    else:
        note('item/completed', dict(threadId=session, turnId=str(turn), item=dict(
            type='commandExecution', id='boundary', command='boundary', status='completed')))


def call(to, body):
    args = dict(to=to, body=body)
    if kind == 'acp':
        mcp.stdin.write(json.dumps(dict(jsonrpc='2.0', id=1, method='tools/call',
                                       params=dict(name='bbs_send', arguments=args)))+'\n')
        mcp.stdin.flush()
        r = json.loads(mcp.stdout.readline())['result']
        return not r.get('isError', False), r['content'][0]['text']
    out(dict(jsonrpc='2.0', id='guard', method='item/tool/call', params=dict(
        threadId=session, turnId=str(turn), tool='bbs_send', arguments=args)))
    r = json.loads(sys.stdin.readline())['result']
    return r['success'], r['contentItems'][0]['text']


def finish(mid, interrupted=False):
    if kind == 'acp':
        reply(mid, dict(stopReason='cancelled' if interrupted else 'end_turn'))
    else:
        note('turn/completed', dict(threadId=session, turn=dict(
            id=str(turn), status='interrupted' if interrupted else 'completed')))


for line in sys.stdin:
    msg = json.loads(line)
    method, mid, p = msg.get('method'), msg.get('id'), msg.get('params', {})
    if method == 'initialize':
        reply(mid, dict(protocolVersion=1, agentCapabilities=dict(loadSession=True, sessionCapabilities=dict(resume={}))) if kind == 'acp' else {})
    elif method in ['session/new', 'session/resume', 'session/load', 'thread/start', 'thread/resume']:
        if kind == 'acp':
            spec = p['mcpServers'][0]
            env = dict(os.environ)
            env.update({e['name']: e['value'] for e in spec['env']})
            mcp = subprocess.Popen([spec['command']] + spec['args'], env=env, text=True,
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        text('OLD REPLAY')
        reply(mid, dict(sessionId=session) if kind == 'acp' else dict(thread=dict(id=session)))
    elif method in ['session/prompt', 'turn/start']:
        turn += 1
        prompt = p['prompt'][0]['text'] if kind == 'acp' else p['input'][0]['text']
        with open('owner-prompts.jsonl', 'a') as f:
            f.write(json.dumps(prompt)+'\n')
        if kind == 'codex':
            note('turn/started', dict(threadId=session, turn=dict(id=str(turn))))
            if 'INTERRUPT' not in prompt and 'OUTAGE' not in prompt:
                text(' hello\n')
            reply(mid, dict(turn=dict(id=str(turn))))
            if turn > 1:
                note('item/agentMessage/delta', dict(threadId=session, turnId=str(turn-1), itemId='stale', delta='STALE'))
                note('turn/completed', dict(threadId=session, turn=dict(id=str(turn-1), status='completed')))
        text('FOREIGN', parent='child')
        if 'OUTAGE' in prompt:
            while not Path('release-output').exists():
                time.sleep(0.02)
            text('durable outage reply\n')
            finish(mid)
            continue
        if 'INTERRUPT' in prompt:
            text(' partial\n')
            tool()
            cancel = json.loads(sys.stdin.readline())
            assert cancel['method'] in ['session/cancel', 'turn/interrupt'], cancel
            if kind == 'codex':
                reply(cancel['id'], {})
            text('tail\n')
            finish(mid, True)
            continue
        ok, reason = call('WiLl', 'must not send owner tool mail')
        assert not ok and 'Nothing was sent' in reason, (ok, reason)
        ok, reason = call('alice', 'other-recipient-still-works')
        assert ok, reason
        if kind == 'acp':
            text(' hello\n')
        text('**world**\n')
        if kind == 'codex':
            note('item/completed', dict(threadId=session, turnId=str(turn), item=dict(
                type='agentMessage', id='message', text=' hello\n**world**\n')))
        text(' \n\t', message='space')
        tool()
        text('```text\n' + 'é'*33000 + '\n```\n', message='long')
        # This last delta is immediately followed by the prompt/turn result.
        text('FINAL\n', message='final')
        finish(mid)
    elif mid is not None:
        out(dict(jsonrpc='2.0', id=mid, error=dict(code=-32601, message=method)))
