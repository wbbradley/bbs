"""No accounts, tools or model calls: effort protocol fixture for local tests."""
import json
from pathlib import Path
import sys
import time

kind = sys.argv[1]
scenario = sys.argv[2] if len(sys.argv) > 2 else ''
current = 'low'
turn = 0
held = None

def record(kind, **fields):
    with open('effort-wire.jsonl', 'a') as f:
        f.write(json.dumps(dict(kind=kind, **fields)) + '\n')

def out(value):
    print(json.dumps(value), flush=True)

def reply(mid, result):
    out(dict(jsonrpc='2.0', id=mid, result=result))

def error(mid):
    out(dict(jsonrpc='2.0', id=mid, error=dict(code=-32602, message='effort deliberately rejected')))

def note(method, params):
    out(dict(jsonrpc='2.0', method=method, params=params))

def options():
    return [dict(id='thinking', category='thought_level', type='select', currentValue=current,
                 options=[dict(value=v, name=v.title()) for v in ['default', 'max', 'low', 'medium', 'high', 'xhigh']])]

def finish(mid, text, tid):
    record('delivered', text=text, effort=current)
    if kind == 'acp':
        note('session/update', dict(sessionId='session', update=dict(sessionUpdate='agent_message_chunk', content=dict(type='text', text='delivered'))))
        reply(mid, dict(stopReason='end_turn'))
    else:
        note('turn/completed', dict(threadId='thread', turn=dict(id=tid, status='completed')))

for line in sys.stdin:
    msg = json.loads(line)
    method, mid, p = msg.get('method'), msg.get('id'), msg.get('params', {})
    if method == 'initialize':
        if kind == 'acp':
            reply(mid, dict(protocolVersion=1, agentInfo=dict(name='@agentclientprotocol/claude-agent-acp'), agentCapabilities=dict(loadSession=True, sessionCapabilities={} if scenario == 'load' else dict(resume={}))))
        else:
            reply(mid, {})
    elif method in ['session/new', 'session/resume', 'session/load']:
        record('open', method=method)
        if method != 'session/new':
            current = 'medium'
            note('session/update', dict(sessionId='session', update=dict(sessionUpdate='config_option_update', configOptions=options())))
            note('session/update', dict(sessionId='child', update=dict(sessionUpdate='config_option_update', configOptions=[])))
            # Replay text must not leak, but the effort metadata must survive.
            note('session/update', dict(sessionId='session', update=dict(sessionUpdate='agent_message_chunk', content=dict(type='text', text='OLD REPLAY'))))
            reply(mid, {})
        else:
            reply(mid, dict(sessionId='session', configOptions=options()))
    elif method in ['thread/start', 'thread/resume']:
        record('open', method=method)
        if method == 'thread/resume':
            current = 'medium'
        reply(mid, dict(thread=dict(id='thread'), model='fixture', reasoningEffort=current))
    elif method == 'model/list':
        reply(mid, dict(data=[dict(id='fixture', model='fixture', defaultReasoningEffort='high', supportedReasoningEfforts=[dict(reasoningEffort=v, description=v) for v in ['max', 'low', 'medium', 'high', 'xhigh']])], nextCursor=None))
    elif method == 'session/set_config_option':
        record('setting', value=p['value'])
        if scenario == 'timeout':
            time.sleep(30)
        if p['value'] == 'max':
            error(mid)
        else:
            if p['value'] == 'low':
                time.sleep(0.4)
            current = 'high' if p['value'] == 'xhigh' else p['value']
            reply(mid, dict(configOptions=options()))
    elif method in ['session/prompt', 'turn/start']:
        turn += 1
        tid = str(turn)
        text = p['prompt'][0]['text'] if kind == 'acp' else p['input'][0]['text']
        record('attempt', text=text, effort=p.get('effort'))
        if scenario == 'drain-error':
            record('drain_error')
            error(mid)
            continue
        if kind == 'codex':
            if scenario == 'timeout' and 'effort' in p:
                time.sleep(30)
            if p.get('effort') == 'xhigh':
                sys.exit(0)  # Ambiguous acceptance: no ACK, never retry automatically.
            if p.get('effort') == 'max':
                error(mid)
                continue
            current = p.get('effort', current)
            reply(mid, dict(turn=dict(id=tid, status='inProgress')))
        if 'METADATA' in text:
            current = 'high'
            if kind == 'acp':
                note('session/update', dict(sessionId='session', update=dict(sessionUpdate='config_option_update', configOptions=options())))
                note('session/update', dict(sessionId='child', update=dict(sessionUpdate='config_option_update', configOptions=[])))
            else:
                note('thread/settings/updated', dict(threadId='thread', threadSettings=dict(model='fixture', effort=current)))
                note('thread/settings/updated', dict(threadId='child', threadSettings=dict(model='other', effort='max')))
        if scenario == 'drain' and 'DRAIN_ACTIVE' in text:
            record('drain_held')
            while not Path('release-drain-turn').exists():
                time.sleep(0.03)
            finish(mid, text, tid)
        elif 'HOLD' in text:
            held = (mid, text, tid)
            record('held', effort=current)
        else:
            finish(mid, text, tid)
    elif method in ['session/cancel', 'turn/interrupt']:
        if kind == 'codex':
            reply(mid, {})
        if held:
            finish(*held)
            held = None
    elif mid is not None:
        out(dict(jsonrpc='2.0', id=mid, error=dict(code=-32601, message=method)))
