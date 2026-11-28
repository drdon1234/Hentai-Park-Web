"""单个 Actions 作业的截止时间和脱敏回执。只终止本次包装命令的进程组。"""
import contextlib
import datetime
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time
import uuid

WORK = Path('test/host-clock')
OUT = Path('test/ui-evidence')
STAGES = {'dependencies':440,'suite':1570,'unpack':110,'simulator-ready':360,'install':180,
          'launch':180,'fixture':100,'test-dependencies':130,'xctest':740,'cold-launch':95,'readback':85}
MARKERS = {'start','pass','fail','timeout','interrupted'}
SIGNALS = ('BUILD FAILED','TEST FAILED','TEST SUCCEEDED','SIGABRT','EXC_BAD_ACCESS','jetsam',
           'watchdog','timed out','testmanagerd','failed to launch','unable to boot','No space left','Permission denied')

def utc(stamp):
    return datetime.datetime.fromtimestamp(stamp,datetime.timezone.utc).isoformat(timespec='milliseconds')

def write_json(path,value):
    path.parent.mkdir(parents=True,exist_ok=True)
    tmp=path.with_name(path.name+'.'+uuid.uuid4().hex+'.tmp')
    with tmp.open('w',encoding='utf8') as f:
        json.dump(value,f,ensure_ascii=False);f.flush();os.fsync(f.fileno())
    os.replace(tmp,path)

def meta():
    runtime=os.environ['SIMULATOR_RUNTIME']
    if runtime=='host' and sys.platform=='darwin':
        runtime=command(['sw_vers','-productVersion'],3).strip()
    return dict(candidate=os.environ['CANDIDATE_SHA'],host=os.environ['HOST_KIND'],system=runtime,
                packageRun=os.environ['PACKAGE_RUN'],scope=os.environ['EXERCISE_SCOPE'],
                run=os.environ['GITHUB_RUN_ID'],attempt=os.environ['GITHUB_RUN_ATTEMPT'])

def emit(stage,instance,result,started,ended=None):
    assert stage in STAGES and result in MARKERS
    now=time.time() if ended is None else ended
    event=dict(stage=stage,id=instance,result=result,started=utc(started),startEpoch=started,
               ended=None if result=='start' else utc(now),seconds=round(max(0,now-started),3))
    OUT.mkdir(parents=True,exist_ok=True)
    with (OUT/'stages.jsonl').open('ab') as f:
        f.write((json.dumps(event)+'\n').encode());f.flush();os.fsync(f.fileno())

@contextlib.contextmanager
def stage(name):
    started=time.time();ident=uuid.uuid4().hex
    emit(name,ident,'start',started)
    try:
        yield
    except BaseException as exc:
        emit(name,ident,'timeout' if isinstance(exc,(TimeoutError,subprocess.TimeoutExpired)) else 'fail',started)
        raise
    else:
        emit(name,ident,'pass',started)

def simulator(udid):
    assert re.fullmatch('[0-9a-fA-F-]{36}',udid)
    write_json(WORK/'simulator.json',udid)

def rows(path):
    if not path.is_file():return []
    lines=path.read_bytes().splitlines(keepends=True)
    return [json.loads(line) for line in lines if line.endswith(b'\n')]

def active():
    result={}
    for event in rows(OUT/'stages.jsonl'):
        if event['result']=='start':result[event['id']]=event
        else:result.pop(event['id'],None)
    return list(result.values())

def read_actions(doc,token):
    result=[];starts={}
    for row in rows(doc/'action-clock.jsonl'):
        code=row.get('code','')
        if (row.get('run')!=token or not re.fullmatch(r'(95|96)\.[0-9]{2}',code)
                or row.get('state') not in ('start','pass','fail') or type(row.get('at')) not in (float,int)):
            continue
        stamp=row['at']
        if row['state']=='start':starts[code]=stamp
        start=starts.get(code,stamp)
        result.append(dict(action=int(code[:2]),code=code,state=row['state'],at=stamp,time=utc(stamp),
                           stage='cold-launch' if code=='95.08' else 'readback' if code in ('95.09','96.04') else 'xctest',
                           started=utc(start),ended=None if row['state']=='start' else utc(stamp),seconds=round(max(0,stamp-start),3)))
    return result

def documents():
    path=WORK/'simulator.json'
    if not path.is_file():return [],None
    device=json.loads(path.read_bytes())
    base=Path.home()/'Library/Developer/CoreSimulator/Devices'/device/'data/Containers/Data/Application'
    return [p.parent for p in base.glob('*/Documents/action-clock.jsonl')],device

def poll_actions():
    docs,device=documents()
    token=os.environ['GITHUB_RUN_ID']+'-'+os.environ['GITHUB_RUN_ATTEMPT']
    for doc in docs:
        events=read_actions(doc,token)
        if events:
            report=OUT/'xctest-actions.json'
            if not report.exists() or json.loads(report.read_bytes())!=events:write_json(report,events)
            return events,doc,device
    return [],None,device

def command(args,seconds):
    try:
        r=subprocess.run(args,capture_output=True,text=True,errors='replace',timeout=seconds)
        return r.stdout if r.returncode==0 else ''
    except (OSError,subprocess.TimeoutExpired):return ''

def diagnostics(reason):
    events,doc,device=poll_actions()
    if not events and (OUT/'xctest-actions.json').is_file():
        events=json.loads((OUT/'xctest-actions.json').read_bytes())
    phases=rows(OUT/'stages.jsonl')
    stopped=time.time()
    measured=[]
    pending={}
    for event in events:
        code=event['code']
        if event['state']=='start':
            pending[code]=event
        elif code in pending:
            pending.pop(code)
            measured.append(dict(code=code,stage=event['stage'],result=event['state'],started=event['started'],
                                 ended=event['ended'],seconds=event['seconds']))
    for code,event in pending.items():
        result='timeout' if reason in ('action-timeout','suite-deadline') or reason.endswith('-timeout') else 'interrupted'
        measured.append(dict(code=code,stage=event['stage'],result=result,started=event['started'],
                             ended=utc(stopped),seconds=round(max(0,stopped-event['at']),3)))
    if measured:write_json(OUT/'action-timing.json',dict(meta(),reason=reason,steps=measured))
    snapshot=dict(meta(),reason=reason,lastStarted=next((x for x in reversed(events) if x['state']=='start'),None),
                  lastCompleted=next((x for x in reversed(events) if x['state']=='pass'),None),
                  lastStageStarted=next((x['stage'] for x in reversed(phases) if x['result']=='start'),None),
                  lastStageCompleted=next((x['stage'] for x in reversed(phases) if x['result']=='pass'),None),
                  failedStage=next((x['stage'] for x in reversed(phases) if x['result'] in ('fail','timeout')),None),
                  openStages=[s['stage'] for s in active()],simulatorState='unknown' if os.environ['HOST_KIND']!='macOS' else 'not-applicable',signals=[])
    write_json(OUT/'diagnosis.json',snapshot)
    if doc:
        for name in ('detail-cold.png','detail-end.png'):
            pic=doc/name
            if pic.is_file() and pic.stat().st_size<20*1024*1024 and pic.read_bytes()[:8]==b'\x89PNG\r\n\x1a\n':
                (OUT/name).write_bytes(pic.read_bytes())
    if os.environ['HOST_KIND']!='macOS':
        listing=command(['xcrun','simctl','list','devices','-j'],6)
        if listing:
            systems=json.loads(listing)['devices']
            system='com.apple.CoreSimulator.SimRuntime.iOS-'+os.environ['SIMULATOR_RUNTIME'].replace('.','-')
            snapshot['availableStates']={s:sum(d.get('state')==s for d in systems.get(system,[]) if d.get('isAvailable'))
                                         for s in ('Booted','Shutdown','Booting','Shutting Down')}
            state=next((d.get('state') for devices in systems.values() for d in devices if d['udid']==device),None)
            if state in ('Booted','Shutdown','Booting','Shutting Down'):snapshot['simulatorState']=state
        if device and snapshot['simulatorState']=='Booted':
            command(['xcrun','simctl','io',device,'screenshot',str(OUT/'failure-simulator.png')],8)
            logs=command(['xcrun','simctl','spawn',device,'log','show','--last','90s','--style','compact','--predicate',
                          'process == "HentaiPark" OR process == "testmanagerd"'],8)
            snapshot['signals']=[s for s in SIGNALS if s.lower() in logs.lower()]
    xc=Path('test/NativeReader/out.xcresult')
    if xc.is_dir():
        summary=command(['xcrun','xcresulttool','get','test-results','summary','--path',str(xc)],8)
        if summary:
            obj=json.loads(summary)
            snapshot['xctest']={k:v for k,v in obj.items() if k in ('passedTests','failedTests','skippedTests','totalTestCount') and type(v) is int}
    write_json(OUT/'diagnosis.json',snapshot)

def finish_preparation(result):
    path=OUT/'preparation.json'
    if path.exists():
        current=json.loads(path.read_bytes())
        if current['result']=='start':
            current.update(end=time.time(),result=result)
            current['seconds']=round(current['end']-current['start'],3)
            write_json(path,current)

def inspect_clock(end):
    now=time.time()
    if now>=end:return 'suite-deadline'
    for entry in active():
        if now-entry['startEpoch']>=STAGES[entry['stage']]:return entry['stage']+'-timeout'
    events,_,_=poll_actions()
    if events and events[-1]['state']=='start':
        limit=STAGES['cold-launch'] if events[-1]['code']=='95.08' else 65
        if now-events[-1]['at']>=limit:return 'action-timeout'
    return None

def watch(name,args):
    deadline=float((WORK/'deadline').read_text())
    current=time.time();ident=uuid.uuid4().hex
    emit(name,ident,'start',current)
    child=None;reason='failed';code=1
    try:
        if current<deadline:
            child=subprocess.Popen(args,start_new_session=True)
            while True:
                rc=child.poll()
                if rc is not None:
                    reason='pass' if rc==0 else 'failed'
                    code=rc if rc>=0 else 1
                    break
                exceeded=inspect_clock(deadline)
                if exceeded:reason=exceeded;code=124;break
                time.sleep(.35)
        else:reason='suite-deadline';code=124
    finally:
        if code:
            try:
                diagnostics(reason)
            except (OSError,ValueError,KeyError):
                write_json(OUT/'collection-error.json',dict(meta(),reason='partial-diagnostics'))
            finally:
                if child and child.poll() is None:
                    try:os.killpg(child.pid,signal.SIGTERM)
                    except ProcessLookupError:pass
                    try:child.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        try:os.killpg(child.pid,signal.SIGKILL)
                        except ProcessLookupError:pass
                        child.wait(timeout=3)
                for row in active():
                    if row['id']!=ident:
                        emit(row['stage'],row['id'],'timeout' if reason==row['stage']+'-timeout' else 'interrupted',row['startEpoch'])
        emit(name,ident,'pass' if code==0 else 'timeout' if code==124 else 'fail',current)
        if name=='dependencies':finish_preparation('pass' if code==0 else reason)
        poll_actions()
    return code

if __name__=='__main__':
    if sys.argv[1]=='init':
        WORK.mkdir(parents=True,exist_ok=True)
        deadline=float(os.environ['HOST_AUDIT_DEADLINE'])
        (WORK/'deadline').write_text(str(deadline))
        write_json(OUT/'host-run.json',dict(meta(),deadline=utc(deadline)))
    else:
        assert sys.argv[1]=='watch' and sys.argv[2] in STAGES and len(sys.argv)>3
        sys.exit(watch(sys.argv[2],sys.argv[3:]))
