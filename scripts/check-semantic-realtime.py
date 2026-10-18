#!/usr/bin/env python3
"""Real native events -> invalidation -> budgeted compiler work -> current relations."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import time

PROJECT=Path(__file__).resolve().parent.parent
spec=importlib.util.spec_from_file_location('semantic_protocol',PROJECT/'scripts/check-semantic.py')
protocol=importlib.util.module_from_spec(spec);spec.loader.exec_module(protocol)


def main():
    output=PROJECT/'build/checks';output.mkdir(parents=True,exist_ok=True);report={}
    with tempfile.TemporaryDirectory(prefix='semantic-live-',dir=output) as folder:
        root=Path(folder).resolve();(root/'preferred').mkdir();(root/'fallback').mkdir()
        header='#ifdef __cplusplus\nextern "C" {\n#endif\nint target(int); int alternate(int);\n#ifdef __cplusplus\n}\n#endif\n#define ROUTE target\n'
        a='#include "api.h"\nint target(int x){return x+1;}\nint alternate(int x){return x-1;}\nint a_caller(){return ROUTE(1);}\n'
        b='#include "api.h"\n#include <choice.h>\nint cpp_caller(){return CHOICE(2);}\n'
        (root/'api.h').write_text(header);(root/'fallback/choice.h').write_text('#define CHOICE target\n')
        (root/'a.c').write_text(a);(root/'b.cpp').write_text(b);(root/'unindexed.c').write_text('int never_automatically_indexed(){return 0;}\n')
        commands=[{'directory':str(root),'file':file,'arguments':['clang++' if file.endswith('cpp') else 'clang','-I.','-Ipreferred','-Ifallback','-c',file]} for file in ['a.c','b.cpp','unindexed.c']]
        (root/'compile_commands.json').write_text(json.dumps(commands))
        c=protocol.Client(root,root/'cache')
        try:
            c.configure();c.index(['a.c','b.cpp']);target=c.symbol('target')['id'];alternate=c.symbol('alternate')['id']
            status=c.rpc('semantic.watch',{'enabled':True,'auto_refresh':True,'metadata_cache':True})
            assert status['monitor'] and not status['monitor']['degraded'],status
            report['watch_backend']=status['monitor']['backend']
            def poll_until(predicate,label,timeout=12):
                start=time.monotonic()
                while time.monotonic()-start<timeout:
                    p=c.rpc('semantic.progress')
                    if p['job']['state']!='running' and predicate(p):
                        report[label]={'elapsed_ms':round((time.monotonic()-start)*1000,2),'job':p['job'],'watch':p['watch']};return p
                    time.sleep(.075)
                raise AssertionError((label,c.rpc('semantic.status')))
            def incoming(entity):return len(c.relations(entity,'incoming'))
            # Warm fingerprints must not bypass freshly changed file stamps.
            incoming(target);incoming(target)
            assert c.rpc('semantic.progress')['watch']['fingerprints']['hits']>0
            b=b.replace('CHOICE(2)','alternate(2)');(root/'b.cpp').write_text(b)
            poll_until(lambda p:p['watch']['auto_jobs']>=1 and incoming(target)==1 and incoming(alternate)==1,'source_update')
            assert c.rpc('semantic.status')['index']['units'].__len__()==2
            print('PASS native saved source update and no automatic indexing of unrelated units',flush=True)
            (root/'api.h').write_text(header.replace('ROUTE target','ROUTE alternate'))
            p=poll_until(lambda p:incoming(target)==0 and incoming(alternate)==2,'header_update');assert p['job']['total']==2
            print('PASS reverse header dependencies rebuild the two indexed owners',flush=True)
            b=b.replace('alternate(2)','CHOICE(2)');(root/'b.cpp').write_text(b)
            poll_until(lambda p:incoming(target)==1,'restore_include')
            (root/'preferred/choice.h').write_text('#define CHOICE alternate\n')
            poll_until(lambda p:incoming(target)==0 and incoming(alternate)==2,'new_shadow_header')
            (root/'preferred/choice.h').unlink()
            poll_until(lambda p:incoming(target)==1 and incoming(alternate)==1,'deleted_shadow_header')
            print('PASS new/deleted shadow headers invalidate unchanged positive dependencies',flush=True)
            b=b.replace('CHOICE(2)','alternate(2)');(root/'atomic.tmp').write_text(b);os.replace(root/'atomic.tmp',root/'b.cpp')
            poll_until(lambda p:incoming(target)==0 and incoming(alternate)==2,'atomic_save')
            before=c.rpc('semantic.progress')['watch']['auto_jobs']
            for i in range(12):(root/'b.cpp').write_text(b+'// burst %d\n'%i)
            poll_until(lambda p:p['watch']['auto_jobs']>before and incoming(alternate)==2,'burst')
            print('PASS atomic replace and burst saves reach the final content',flush=True)
            opened=c.rpc('document.open',{'path':'b.cpp'});offset=b.index('alternate(2)')
            edit=c.rpc('document.edit',{'path':'b.cpp','base_revision':opened['analysis']['revision'],'start_byte':offset,'old_end_byte':offset+9,'new_text':'target'})
            (root/'b.cpp').write_text(b+'// disk changed while buffer dirty\n')
            before=c.rpc('semantic.progress')['watch']['auto_jobs']
            end=time.monotonic()+2
            while time.monotonic()<end:c.rpc('semantic.progress');time.sleep(.1)
            p=c.rpc('semantic.progress');assert p['watch']['pending_units']>=1,p
            assert next(u for u in c.rpc('semantic.status')['index']['units'] if u['file'].endswith('/b.cpp'))['state']!='current'
            assert incoming(alternate)==1
            preview=c.rpc('semantic.preview',{'path':'b.cpp','revision':edit['analysis']['revision']});assert preview['snapshot']['calls'][0]['target']==target and not preview['persisted']
            c.rpc('document.close',{'path':'b.cpp'})
            poll_until(lambda p:incoming(alternate)==2,'dirty_buffer_release')
            print('PASS dirty overlays mask disk facts and block automatic publication until released',flush=True)
            c.rpc('semantic.cancel');(root/'b.cpp').write_text(b+'// paused change\n')
            before=c.rpc('semantic.progress')['watch']['auto_jobs'];time.sleep(1.5)
            p=c.rpc('semantic.progress');assert p['watch']['paused'] and p['watch']['auto_jobs']==before
            c.rpc('semantic.watch',{'enabled':True,'auto_refresh':True,'metadata_cache':True})
            poll_until(lambda p:incoming(alternate)==2 and p['watch']['pending_units']==0 and p['watch']['auto_jobs']>before,'resume')
            print('PASS explicit cancellation pauses automatic work and resume preserves pending units',flush=True)
            c.rpc('cache.pressure',{'level':'critical','busy':True,'now_ms':1000})
            (root/'b.cpp').write_text(b+'// pressure change\n');time.sleep(1.5)
            p=c.rpc('semantic.progress');assert p['watch']['pressure_paused'] and p['job']['state']!='running'
            c.rpc('cache.pressure',{'level':'normal','busy':False,'now_ms':2000})
            poll_until(lambda p:incoming(alternate)==2 and p['watch']['pending_units']==0,'pressure_resume')
            c.rpc('semantic.refresh')
            p=poll_until(lambda p:p['job']['trigger']=='manual' and p['job']['analyzed']==2,'manual_recorded_refresh')
            assert p['job']['total']==2 and len(c.rpc('semantic.status')['index']['units'])==2
            b=b.replace('alternate(2)','target(2)');(root/'b.cpp').write_text(b)
            time.sleep(.6)  # Let the native listener queue a save without pumping it.
            c.rpc('semantic.watch',{'enabled':True,'auto_refresh':True,'metadata_cache':False})
            poll_until(lambda p:incoming(target)==1 and incoming(alternate)==1,'settings_preserve_queued_save')
            c.rpc('semantic.watch',{'enabled':True,'auto_refresh':True,'metadata_cache':True})
            (root/'unindexed.c').write_text('int broken( {\n')
            c.index(['unindexed.c'],success=False)
            (root/'unindexed.c').write_text('int recovered_after_first_failure(){return 1;}\n')
            poll_until(lambda p:bool(c.rpc('semantic.symbols',{'name':'recovered_after_first_failure'})['rows']),'first_failure_recovery')
            print('PASS a first-ever failed translation unit is retried when its main file is fixed',flush=True)
            opened=c.rpc('document.open',{'path':'b.cpp'})
            c.rpc('document.edit',{'path':'b.cpp','base_revision':opened['analysis']['revision'],'start_byte':0,'old_end_byte':0,'new_text':'// dirty during config change\n'})
            commands[0]['arguments'].append('-DCHANGED=1');(root/'compile_commands.json').write_text(json.dumps(commands))
            assert c.rpc('semantic.cancel')['cancelled']
            poll_until(lambda p:p['watch']['configuration_required'],'configuration_changed')
            assert c.rpc('semantic.symbols',{'name':'target'},expect_error=True)
            print('PASS critical pressure pauses work; changed compile flags require reconfiguration',flush=True)
        finally:c.close()
    (output/'semantic-realtime.json').write_text(json.dumps(report,ensure_ascii=False,indent=2))
    print('All native realtime checks passed.')

if __name__=='__main__':main()
