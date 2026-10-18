#!/usr/bin/env python3
"""Native read-only reader follows an external saved edit in both appearances."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

PROJECT=Path(__file__).resolve().parent.parent
spec=importlib.util.spec_from_file_location('semantic_protocol',PROJECT/'scripts/check-semantic.py')
protocol=importlib.util.module_from_spec(spec);spec.loader.exec_module(protocol)


def main():
    output=PROJECT/'build/checks';output.mkdir(parents=True,exist_ok=True);reports={}
    with tempfile.TemporaryDirectory(prefix='native-live-',dir=output) as folder:
        root=Path(folder).resolve();cache=root/'cache'
        (root/'a.c').write_text('int first(int x){return x+1;}\nint second(int x){return x-1;}\n')
        before='extern "C" int first(int);\nextern "C" int second(int);\nint caller(){return first(1);}\n'
        after=before.replace('return first(1)','return second(1)')
        commands=[{'directory':str(root),'file':file,'arguments':['clang++' if file.endswith('cpp') else 'clang','-c',file]} for file in ['a.c','b.cpp']]
        (root/'compile_commands.json').write_text(json.dumps(commands))
        for appearance in ['light','dark']:
            (root/'b.cpp').write_text(before)
            client=protocol.Client(root,cache)
            try:client.configure();client.index()
            finally:client.close()
            ready=root.parent/(root.name+'-'+appearance+'.ready')
            png=Path('/tmp')/('symweft-realtime-'+appearance+'.png')
            stdout=output/('native-live-'+appearance+'.json');stderr=output/('native-live-'+appearance+'.stderr')
            with stdout.open('w') as out,stderr.open('w') as err:
                process=subprocess.Popen([str(PROJECT/'build/macos/Symweft.app/Contents/MacOS/Symweft'),'--workspace',str(root),'--file','b.cpp','--compile-commands',str(root/'compile_commands.json'),'--inspect-symbol','caller','--appearance',appearance,'--capture-to',str(png),'--capture-delay-ms','5500','--qa-ready-to',str(ready)],stdout=out,stderr=err,env=dict(os.environ,SYMWEFT_CACHE_DIR=str(cache)))
                try:
                    deadline=time.monotonic()+15
                    while not ready.exists() and time.monotonic()<deadline:
                        assert process.poll() is None,stderr.read_text();time.sleep(.05)
                    assert ready.exists(),'native window not ready'
                    (root/'saved.tmp').write_text(after);os.replace(root/'saved.tmp',root/'b.cpp')
                    process.wait(timeout=20)
                    assert process.returncode==0,stderr.read_text()
                finally:
                    if process.poll() is None:process.kill();process.wait()
                    ready.unlink(missing_ok=True)
            report=json.loads(stdout.read_text())
            assert report['semantic_outgoing_names']==['second'],report
            assert report['document_size_bytes']==len(after.encode()),report
            assert report['semantic_watch_revision']>1 and report['current_function']=='caller',report
            reports[appearance]=report
            print('PASS',appearance,'native reader reloaded saved text and compiler-bound target first -> second',flush=True)
    (output/'native-realtime.json').write_text(json.dumps(reports,ensure_ascii=False,indent=2))

if __name__=='__main__':main()
