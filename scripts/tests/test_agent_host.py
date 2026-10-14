"""Focused adapter cooperative-interrupt regression; no authenticated model calls."""
import json
import os
from pathlib import Path
import signal
import runpy
import subprocess
import sys
import tempfile
import time
import unittest

ADAPTER = Path(__file__).resolve().parents[1] / "agent-host.py"
MAIN = runpy.run_path(str(ADAPTER))["project_local_root"]()

class HostInterrupt(unittest.TestCase):
    def test_forwards_interrupt_and_retains_cleanup_evidence(self):
        for requested in (signal.SIGINT, signal.SIGTERM, "output_limit"):
            with self.subTest(signal=requested), tempfile.TemporaryDirectory(dir=MAIN, prefix="host-regression-") as directory:
                root = Path(directory)
                repo = root / "repo"
                repo.mkdir()
                evidence = root / "evidence"
                evidence.mkdir()
                (evidence / "prompt.txt").write_text("fixture")
                fake = root / "codex-fixture"
                fake.write_text("#!" + sys.executable + "\n" + r'''import json,signal,subprocess,sys,time
from pathlib import Path
child=subprocess.Popen([sys.executable,"-c","import time; time.sleep(120)"],start_new_session=True)
def stop(number,frame):
 child.terminate()
 child.wait(timeout=5)
 Path("cleanup.json").write_text(json.dumps({"child":child.pid,"signal":number}))
 sys.exit(0)
signal.signal(signal.SIGINT,stop)
Path("started.json").write_text(json.dumps({"child":child.pid}))
while True:time.sleep(.05)
''')
                if requested == "output_limit":
                    code = fake.read_text().replace("while True:time.sleep(.05)", "sys.stdout.write('x' * (9 * 1024 * 1024)); sys.stdout.flush(); time.sleep(120)")
                    fake.write_text(code)
                fake.chmod(0o700)
                process = subprocess.Popen([sys.executable, str(ADAPTER), "_child", "--codex", str(fake), "--evidence", str(evidence), "--model", "gpt-6.1-sol"],cwd=repo,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                try:
                    deadline=time.monotonic()+10
                    while not (repo / "started.json").exists():
                        self.assertIsNone(process.poll())
                        self.assertLess(time.monotonic(),deadline)
                        time.sleep(.02)
                    if requested != "output_limit":
                        process.send_signal(requested)
                    stdout,stderr=process.communicate(timeout=10)
                    self.assertEqual(process.returncode,1 if requested == "output_limit" else 130,stderr.decode())
                    result=json.loads((evidence / "host-result.json").read_text())
                    self.assertEqual(result["cancellation_requested"],requested != "output_limit")
                    self.assertEqual(result["output_limit_exceeded"],requested == "output_limit")
                    self.assertEqual(result["returncode"],0)
                    cleanup=json.loads((repo / "cleanup.json").read_text())
                    self.assertEqual(cleanup["signal"],signal.SIGINT)
                    with self.assertRaises(ProcessLookupError):os.kill(cleanup["child"],0)
                finally:
                    if process.poll() is None:process.kill()
                    process.wait(timeout=5)
                    for name in ("stdout","stderr"):
                        handle=getattr(process,name)
                        if handle:handle.close()

class OuterInterrupt(unittest.TestCase):
    def test_startup_and_wait_interrupts_clean_owned_controller(self):
        for mode, stage in [(mode, stage) for mode in ("outer", "child")
                            for stage in ("before_launch", "after_popen", "during_wait")]:
            for requested in (signal.SIGINT, signal.SIGTERM):
                with self.subTest(mode=mode, stage=stage, signal=requested), tempfile.TemporaryDirectory(dir=MAIN, prefix="host-outer-regression-") as directory:
                    root = Path(directory)
                    repo = root / "repo"
                    repo.mkdir()
                    state = root / "state"
                    state.mkdir(mode=0o700)
                    auth = root / "fixture-auth.json"
                    auth.write_text("fixture credentials only")
                    prompt = root / "prompt.txt"
                    prompt.write_text("fixture")
                    fake = root / "izu-fixture"
                    fake.write_text("#!" + sys.executable + "\n" + r'''import json,os,signal,subprocess,sys,time
from pathlib import Path
if "--version" in sys.argv:
 print("fixture-host 1")
 sys.exit(0)
child=subprocess.Popen([sys.executable,"-c","import time; time.sleep(120)"],start_new_session=True)
def stop(number,frame):
 child.terminate()
 child.wait(timeout=5)
 Path("cleanup.json").write_text(json.dumps({"controller":os.getpid(),"child":child.pid,"signal":number}))
 sys.exit(0)
signal.signal(signal.SIGINT,stop)
Path("started.json").write_text(json.dumps({"controller":os.getpid(),"child":child.pid}))
while True:time.sleep(.02)
''')
                    fake.chmod(0o700)
                    evidence = root / "evidence"
                    if mode == "child":
                        evidence.mkdir()
                        (evidence / "prompt.txt").write_text("fixture")
                    driver = root / "driver.py"
                    driver.write_text(r'''import os,runpy,signal,subprocess,sys,time
from pathlib import Path
adapter,fake,repo,state,auth,prompt,evidence,stage,requested,mode=sys.argv[1:]
requested=int(requested)
real_popen=subprocess.Popen
real_signal=signal.signal
injecting=False
injected=False
def unexpected(number,frame):
 raise AssertionError("startup cancellation reached previous handler")
real_signal(requested,unexpected)
def install(number,handler):
 global injected
 result=real_signal(number,handler)
 if stage=="before_launch" and number==requested and not injected:
  injected=True
  signal.raise_signal(requested)
 return result
signal.signal=install
def popen(command,*args,**kwargs):
 global injecting,injected
 process=real_popen(command,*args,**kwargs)
 if str(command[0])!=fake or "--version" in command:return process
 original_send=process.send_signal
 try:
  deadline=time.monotonic()+5
  while not (Path(repo)/"started.json").exists():
   assert process.poll() is None
   assert time.monotonic()<deadline
   time.sleep(.01)
  original_send=process.send_signal
  def send(number):
   assert not injecting,"signal handler called Popen.send_signal"
   return original_send(number)
  process.send_signal=send
  original_wait=process.wait
  def wait(*args,**kwargs):
   global injecting,injected
   if stage=="during_wait" and not injected:
    injected=True
    injecting=True
    try:signal.raise_signal(requested)
    finally:injecting=False
   return original_wait(*args,**kwargs)
  process.wait=wait
  if stage=="after_popen":
   injecting=True
   try:signal.raise_signal(requested)
   finally:injecting=False
  return process
 except BaseException:
  original_send(signal.SIGINT)
  process.wait(timeout=5)
  raise
subprocess.Popen=popen
sys.argv=[adapter,"start","--izu",fake,"--repo",repo,"--change","fixture","--codex",fake,"--state-root",state,"--auth-file",auth,"--evidence",evidence,"--prompt-file",prompt]
if mode=="child":sys.argv=[adapter,"_child","--codex",fake,"--evidence",evidence]
runpy.run_path(adapter,run_name="__main__")
''')
                    process=subprocess.Popen([sys.executable,str(driver),str(ADAPTER),str(fake),str(repo),str(state),str(auth),str(prompt),str(evidence),stage,str(int(requested)),mode],cwd=repo,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                    stdout,stderr=process.communicate(timeout=12)
                    self.assertEqual(process.returncode,130,stderr.decode())
                    result=json.loads((evidence / ("launch-result.json" if mode=="outer" else "host-result.json")).read_text())
                    self.assertTrue(result["cancellation_requested"])
                    launched=result["controller_launched"] if mode=="outer" else result["returncode"] is not None
                    self.assertEqual(launched,stage!="before_launch")
                    self.assertEqual(list(state.glob("codex-session-*")),[])
                    self.assertEqual(auth.read_text(),"fixture credentials only")
                    if stage=="before_launch":
                        self.assertFalse((repo / "started.json").exists())
                        self.assertFalse((evidence / "controller.json").exists())
                    else:
                        cleanup=json.loads((repo / "cleanup.json").read_text())
                        self.assertEqual(cleanup["signal"],signal.SIGINT)
                        for pid in (cleanup["controller"],cleanup["child"]):
                            with self.assertRaises(ProcessLookupError):os.kill(pid,0)

if __name__ == "__main__":unittest.main()
