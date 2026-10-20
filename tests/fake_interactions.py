"""Native input/context protocol fixture. No tools, accounts or model calls."""
import json
import sys

kind, scenario = sys.argv[1:3]
turn = 0
held = None

def record(kind, **fields):
    with open('interaction-wire.jsonl', 'a') as f:
        f.write(json.dumps(dict(kind=kind, **fields)) + '\n')
def out(obj): print(json.dumps(obj), flush=True)
def reply(mid, result): out(dict(jsonrpc='2.0', id=mid, result=result))
def note(method, params): out(dict(jsonrpc='2.0', method=method, params=params))
def ask(mid, parent, tid):
    if kind == 'acp':
        schema = dict(type='object', properties=dict(choice=dict(type='string', title='Strategy', description='Pick one', oneOf=[dict(const='safe', title='Safe', description='Keep data')]), extra=dict(type='string', title='Other')))
        if scenario == 'unsupported': schema['properties']['extra'] = dict(type='array', items=dict(type='string'))
        out(dict(jsonrpc='2.0', id=mid, method='elicitation/create', params=dict(sessionId=parent, mode='form', message='Which deployment strategy?', requestedSchema=schema)))
    else:
        out(dict(jsonrpc='2.0', id=mid, method='item/tool/requestUserInput', params=dict(threadId=parent, turnId=tid, itemId='decision', isBlocking=False, questions=[dict(id='choice', header='Strategy', question='Which deployment strategy?', isOther=True, isSecret=False, options=[dict(label='Safe', description='Keep data')])])) )
def usage(parent, tid, used):
    if kind == 'acp':
        note('session/update', dict(sessionId=parent, update=dict(sessionUpdate='usage_update', used=used, size=100)))
    else:
        note('thread/tokenUsage/updated', dict(threadId=parent, turnId=tid, tokenUsage=dict(last=dict(totalTokens=used), total=dict(totalTokens=99999), modelContextWindow=100)))
def finish():
    global held
    if not held: return
    mid, tid = held
    if kind == 'acp': reply(mid, dict(stopReason='end_turn'))
    else: note('turn/completed', dict(threadId='parent', turn=dict(id=tid, status='completed')))
    usage('parent', tid, 80)
    usage('child', tid, 99)
    if kind == 'codex': usage('parent', 'old-turn', 97)
    record('finished', turn=tid)
    held = None

for line in sys.stdin:
    msg=json.loads(line)
    method, mid, p=msg.get('method'),msg.get('id'),msg.get('params',{})
    if method is None:
        record('answer', id=mid, response=msg)
        if mid == 'question': finish()
    elif method == 'initialize':
        if kind == 'acp':
            if scenario == 'disabled': assert 'elicitation' not in p['clientCapabilities']
            else: assert p['clientCapabilities']['elicitation'] == dict(form={})
            reply(mid,dict(protocolVersion=1,agentCapabilities={},agentInfo=dict(name='fixture')))
        else: reply(mid,{})
    elif method in ['thread/start','thread/resume']:
        reply(mid,dict(thread=dict(id='parent'),model='fixture',reasoningEffort='low'))
        record('ready')
    elif method == 'model/list':
        reply(mid,dict(data=[],nextCursor=None))
    elif method in ['session/new','session/resume']:
        reply(mid,dict(sessionId='parent'))
        record('ready')
    elif method in ['turn/start','session/prompt']:
        turn+=1; tid=str(turn); held=(mid,tid)
        text=p['prompt'][0]['text'] if kind=='acp' else p['input'][0]['text']
        record('prompt', text=text)
        if kind=='codex' and scenario!='resolved': reply(mid,dict(turn=dict(id=tid,status='inProgress')))
        usage('parent',tid,40)
        if scenario == 'text-before-question':
            before = '  Before the question.\n\n```text\n  exact bytes\n```\n'
            if kind == 'acp':
                note('session/update',dict(sessionId='parent',update=dict(sessionUpdate='agent_message_chunk',content=dict(type='text',text=before))))
            else:
                note('item/agentMessage/delta',dict(threadId='parent',turnId=tid,itemId='preface',delta=before))
        ask('foreign','child',tid)
        ask('question','parent',tid)
        if kind=='codex' and scenario=='resolved':
            note('serverRequest/resolved',dict(threadId='parent',requestId='question'))
            reply(mid,dict(turn=dict(id=tid,status='inProgress')))
            finish()
        elif scenario=='double': ask('duplicate','parent',tid)
    elif method in ['turn/interrupt','session/cancel']:
        if kind=='codex': reply(mid,{})
        finish()
    elif method=='turn/steer':
        record('steer',text=p['input'][0]['text']);reply(mid,dict(turnId=str(turn)))
    elif mid is not None:
        out(dict(jsonrpc='2.0',id=mid,error=dict(code=-32601,message=method)))
