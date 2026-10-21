"""Isolated Codex fork fixture; no model calls or real tools."""
import json
import os
import sys
import threading
import time

scenario = sys.argv[1]
lock = threading.Lock()
parent_live = False
child_live = False
child_turn = 0
waiting = set()
record_pid = os.getpid()
# The BBS MCP server an "adopted" parent (opened through ACP) is resumed with.
MCP = dict(mcp_servers=dict(bbs=dict(command='bbs-fixture',args=[],env=dict(BBS_HEADLESS_TOKEN='fixture'))))

def record(kind, **fields):
    with open('btw-wire.jsonl','a') as f: f.write(json.dumps(dict(kind=kind,**fields))+'\n')
def out(value):
    with lock: print(json.dumps(value),flush=True)
def reply(mid,result):out(dict(jsonrpc='2.0',id=mid,result=result))
def error(mid,code=-32601,message='unsupported fixture prerequisite'):out(dict(jsonrpc='2.0',id=mid,error=dict(code=code,message=message)))
def note(method,p):out(dict(jsonrpc='2.0',method=method,params=p))
def text(thread,turn,body,item='m'):
    note('item/agentMessage/delta',dict(threadId=thread,turnId=turn,itemId=item,delta=body))
def done(thread,turn,status='completed'):
    global child_live,parent_live
    note('turn/completed',dict(threadId=thread,turn=dict(id=turn,status=status)))
    if thread=='child':child_live=False
    else:parent_live=False

def later(marker,mid,value):
    def wait():
        while not os.path.exists(marker): time.sleep(.01)
        reply(mid,value)
    threading.Thread(target=wait,daemon=True).start()

record('provider_pid',pid=os.getpid())
for line in sys.stdin:
    msg=json.loads(line);method=msg.get('method');mid=msg.get('id');p=msg.get('params',{})
    if method is None:
        record('callback',id=mid,response=msg)
        waiting.discard(mid)
        if not waiting and child_live and scenario in ['normal','early','tools','adopted','model']:
            text('child','s','side final\n')
            done('child','s')
        continue
    record('rpc',method=method,params=p)
    if method=='initialize':reply(mid,{})
    elif method=='model/list':reply(mid,dict(data=[],nextCursor=None))
    elif method in ['thread/start','thread/resume']:
        if method=='thread/start':
            assert 'adopted' not in scenario and any(t['name']=='bbs_agents' for t in p['dynamicTools'])
            if 'model' in scenario:assert p['model']=='fixture-model' and p['modelProvider']=='fixture-provider'
        # A thread opened through ACP gets its MCP server again; one bbs
        # started keeps its plain-function tools and gets none.
        elif 'adopted' in scenario:assert p['config']==MCP and 'dynamicTools' not in p
        else:assert 'config' not in p and 'dynamicTools' not in p
        reply(mid,dict(thread=dict(id='parent'),model='fixture',reasoningEffort='low'))
    elif method=='thread/fork':
        if scenario.startswith('continue'):child_turn=0
        # As Codex 0.160.1 refuses it.
        if p.get('ephemeral') and p.get('deferGoalContinuation'):
            error(mid,-32600,'`deferGoalContinuation` cannot be combined with `ephemeral`');continue
        expected=dict(threadId='parent',ephemeral=True,excludeTurns=True)
        # A fork inherits plain-function tools, but an MCP server only if given.
        if 'adopted' in scenario:expected['config']=MCP
        # Nor the session's model and provider, which Codex would otherwise
        # take from its own defaults.
        if 'model' in scenario:expected.update(model='fixture-model',modelProvider='fixture-provider')
        assert p==expected
        if scenario=='no-fork':error(mid);continue
        fork=dict(thread=dict(id='child',forkedFromId='parent',ephemeral=True))
        if scenario=='bad-fork':fork['thread']['ephemeral']=False
        if scenario=='late-fork':record('fork_pending');later('release-fork',mid,fork)
        else:reply(mid,fork)
    elif method.startswith('thread/goal/'):
        # An ephemeral child cannot own a goal.
        error(mid,-32600,'ephemeral thread does not support goals: '+p.get('threadId',''))
    elif method=='turn/start':
        if p['threadId']=='parent':
            parent_live=True;reply(mid,dict(turn=dict(id='p')));text('parent','p','parent before\n');note('thread/tokenUsage/updated',dict(threadId='parent',turnId='p',tokenUsage=dict(last=dict(totalTokens=40),modelContextWindow=100)));record('parent_active')
            continue
        assert p['threadId']=='child'
        if scenario.startswith('continue'):
            child_turn += 1
            turn = 's'+str(child_turn)
            body = p['input'][0]['text']
            if child_turn == 1:
                assert body.startswith('You are answering a side question alongside the main agent.')
            else:
                assert not body.startswith('You are answering a side question alongside the main agent.')
                text('child','s'+str(child_turn-1),'STALE SIDE TEXT')
                done('child','s'+str(child_turn-1))
            child_live=True
            ack=dict(turn=dict(id=turn))
            if scenario=='continue-late-start' and child_turn>1:
                record('start_pending');later('release-start',mid,ack);continue
            if scenario!='continue-early':reply(mid,ack)
            text('parent','p','parent alongside\n')
            text('child',turn,'side answer '+str(child_turn)+'\n')
            note('thread/tokenUsage/updated',dict(threadId='child',turnId=turn,tokenUsage=dict(last=dict(totalTokens=99999),modelContextWindow=100)))
            if 'tool' in scenario:
                note('item/started',dict(threadId='child',turnId=turn,item=dict(id='provider-tool',type='mcpToolCall' if 'adopted' in scenario else 'commandExecution')))
                if 'pending' not in scenario and ('cancel' not in scenario or 'HOLD SIDE' not in body):
                    note('item/completed',dict(threadId='child',turnId=turn,item=dict(id='provider-tool',type='mcpToolCall')))
            if 'HOLD SIDE' in body:record('side_active',turn=turn);continue
            done('child',turn,'failed' if scenario=='continue-failed' else 'interrupted' if scenario=='continue-unsolicited' else 'completed')
            if scenario=='continue-early':reply(mid,ack)
            continue
        assert p['input'][0]['text'].startswith('You are answering a side question alongside the main agent.')
        child_live=True
        ack=dict(turn=dict(id='s'))
        if scenario in ['late-start','late-start-tools']:
            note('turn/started',dict(threadId='child',turn=dict(id='s')))
            record('start_pending')
            if scenario=='late-start-tools':
                out(dict(jsonrpc='2.0',id='tool',method='item/tool/call',params=dict(threadId='child',turnId='s',callId='t',tool='bbs_agents',arguments={})))
                record('tool_pending')
            later('release-start',mid,ack);continue
        if scenario!='early':reply(mid,ack)
        text('parent','p','parent alongside\n')
        text('child','old-side','STALE SIDE TEXT')
        text('child','s','side prefix\n')
        note('thread/tokenUsage/updated',dict(threadId='child',turnId='s',tokenUsage=dict(last=dict(totalTokens=99999),modelContextWindow=100)))
        if scenario=='overflow':text('child','s','é'*40000);continue
        if scenario=='hold':record('side_active');continue
        waiting={'native','approval','foreign','legacy-command','legacy-patch','ambiguous-legacy'}
        out(dict(jsonrpc='2.0',id='native',method='item/tool/requestUserInput',params=dict(threadId='child',turnId='s',itemId='q',isBlocking=True,questions=[])))
        out(dict(jsonrpc='2.0',id='approval',method='item/commandExecution/requestApproval',params=dict(threadId='child',turnId='s',command='synthetic')))
        out(dict(jsonrpc='2.0',id='foreign',method='item/tool/call',params=dict(threadId='unrelated',turnId='s',callId='f',tool='bbs_status',arguments=dict(text='must not execute'))))
        out(dict(jsonrpc='2.0',id='legacy-command',method='execCommandApproval',params=dict(conversationId='child',callId='legacy-c',command=['synthetic'])))
        out(dict(jsonrpc='2.0',id='legacy-patch',method='applyPatchApproval',params=dict(conversationId='child',callId='legacy-p',fileChanges={})))
        out(dict(jsonrpc='2.0',id='ambiguous-legacy',method='execCommandApproval',params=dict(callId='ambiguous',command=['synthetic'])))
        if scenario=='tools':
            waiting.add('parent-tool')
            out(dict(jsonrpc='2.0',id='parent-tool',method='item/tool/call',params=dict(threadId='parent',turnId='p',callId='parent-t',tool='bbs_agents',arguments={})))
        if scenario in ['tools','tools-cancel','tools-complete']:
            waiting.add('tool')
            out(dict(jsonrpc='2.0',id='tool',method='item/tool/call',params=dict(threadId='child',turnId='s',callId='t',tool='bbs_agents',arguments={})))
            record('tool_pending')
            if scenario=='tools-complete':done('child','s')
        if scenario=='early':reply(mid,ack)
    elif method=='turn/interrupt':
        assert p['threadId'] in ['parent','child']
        if p['threadId']=='child' and scenario=='continue-bad-interrupt':error(mid,-32603,'synthetic interrupt failure');continue
        reply(mid,{})
        if p['threadId']=='child' and scenario=='continue-no-terminal':continue
        if 'tool-cancel' in scenario:
            note('item/completed',dict(threadId='child',turnId=p['turnId'],item=dict(id='provider-tool',type='mcpToolCall')))
        done(p['threadId'],p['turnId'],'interrupted')
    elif method=='thread/unsubscribe':
        assert p['threadId']=='child';reply(mid,{})
    elif method=='turn/steer':
        assert p['threadId']=='parent';reply(mid,dict(turnId='p'));text('parent','p','parent steered\n')
    elif mid is not None:error(mid)
