#!/usr/bin/env python3
"""Explicit, synthetic acceptance against a selected disposable GitHub target.
No deletion, global configuration, or credentials persisted. Source upload is
restricted to the explicitly selected candidate-ci phase and frozen archive.
"""
import argparse
import base64
import urllib.request
import concurrent.futures
import hashlib
import json
import os
import re
from pathlib import Path
import signal
import subprocess
import time
import tarfile

def disposable_target(value):
    owner = r'[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?'
    repository = r'izu-acceptance-[a-z0-9][a-z0-9-]{0,49}'
    if not re.fullmatch(owner + '/' + repository, value):
        raise argparse.ArgumentTypeError(
            'Select an explicitly authorized disposable OWNER/izu-acceptance-NAME repository')
    return value


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--target', required=True, type=disposable_target,
                   help='Exact authorized disposable OWNER/izu-acceptance-NAME; no implicit target')
    p.add_argument('--binary', required=True, type=Path)
    p.add_argument('--evidence', required=True, type=Path)
    p.add_argument('--phase', choices=['setup', 'exercise', 'acceptance', 'capture', 'final', 'candidate-ci'], required=True)
    p.add_argument('--run-label', default='final', help='Fresh prefix for final fixture branches and receipts')
    p.add_argument('--source-root', type=Path)
    p.add_argument('--source-archive', type=Path)
    p.add_argument('--source-digest', help='Exact SHA-256 of the frozen source archive')
    a = p.parse_args()
    TARGET = a.target
    owner, repository = TARGET.split('/')
    URL = f'https://github.com/{TARGET}.git'
    if not re.fullmatch(r'[a-z][a-z0-9-]{0,30}', a.run_label):
        raise RuntimeError('Run label must be a short portable lowercase name')
    root = a.evidence.resolve()
    root.mkdir(parents=True, exist_ok=True)
    os.chmod(root, 0o700)
    if a.phase == 'candidate-ci' and a.run_label != 'final':
        root = root/('candidate-ci-'+a.run_label)
        root.mkdir(exist_ok=False)
        root.chmod(0o700)
    binary = a.binary.resolve()
    token = subprocess.run(['gh', 'auth', 'token', '--hostname', 'github.com'], capture_output=True, text=True, check=True).stdout.strip()
    if not token:
        raise RuntimeError('Selected authenticated token is absent')
    env = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_SYSTEM=os.devnull,
               GIT_TERMINAL_PROMPT='0', IZU_REMOTE_ACCEPTANCE_TOKEN=token,
               IZU_AUTHOR_NAME='Izu Synthetic Acceptance', IZU_AUTHOR_EMAIL='izu-acceptance@example.invalid')
    env.pop('GH_DEBUG', None)
    env.pop('GIT_TRACE', None)
    env.pop('GIT_CURL_VERBOSE', None)
    askpass = root / 'askpass.py'
    askpass.write_text('#!/usr/bin/env python3\nimport os,sys\nprint("x-access-token" if "username" in sys.argv[1].lower() else os.environ["IZU_REMOTE_ACCEPTANCE_TOKEN"])\n')
    askpass.chmod(0o700)
    env['GIT_ASKPASS'] = str(askpass)
    auth = ['--https-user', 'x-access-token', '--https-token-env', 'IZU_REMOTE_ACCEPTANCE_TOKEN']
    git = ['git', '-c', 'credential.helper=', '-c', 'core.hooksPath=/dev/null']
    receipts = []

    def redact(s):
        return s.replace(token, '[REDACTED]')

    def run(label, argv, cwd=root, expect=0, extra_env=None, cancel_after=None):
        started = time.time()
        selected_env = dict(env)
        if extra_env:
            selected_env.update(extra_env)
        proc = subprocess.Popen([str(x) for x in argv], cwd=cwd, env=selected_env,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        if cancel_after is not None:
            time.sleep(cancel_after)
            if proc.poll() is None:
                proc.send_signal(signal.SIGINT)
        try:
            out, err = proc.communicate(timeout=180)
        except subprocess.TimeoutExpired:
            proc.kill()
            out, err = proc.communicate()
            err += '\nHarness deadline expired'
        receipt = dict(label=label, argv=[str(x) for x in argv], cwd=str(cwd),
                       started_unix=started, duration_seconds=time.time()-started,
                       exit_code=proc.returncode, stdout=redact(out), stderr=redact(err),
                       expected_exit=expect, cancellation_delay=cancel_after,
                       binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
        (root / f'{label}.json').write_text(json.dumps(receipt, indent=2)+'\n')
        receipts.append(receipt)
        print(f'{label}: exit={proc.returncode}', flush=True)
        if expect is not None and proc.returncode != expect:
            raise RuntimeError(f'{label}: expected {expect}, observed {proc.returncode}; inspect private receipt')
        return out

    def izu(label, args, cwd, **kwargs):
        return run(label, [binary, '--json', *args], cwd, **kwargs)

    def refs(label):
        return run(label, git+['ls-remote', URL])

    try:
        login = run('identity-'+a.phase, ['gh', 'api', 'user', '--jq', '.login']).strip()
        if login != owner:
            raise RuntimeError('Exact authorized owner is not authenticated')
        if a.phase == 'setup':
            check = subprocess.run(['gh', 'api', f'repos/{TARGET}'], capture_output=True, text=True)
            if check.returncode == 0 or '404' not in check.stderr:
                raise RuntimeError('Target is not confirmed absent; refusing creation/overwrite')
            run('create-private', ['gh', 'api', '--method', 'POST', 'user/repos', '-f',
                                  'name='+repository, '-F', 'private=true',
                                  '-F', 'auto_init=false'])
            metadata = json.loads(run('target-metadata', ['gh', 'api', f'repos/{TARGET}']))
            if metadata['full_name'] != TARGET or not metadata['private']:
                raise RuntimeError('Exact private target identity mismatch')
            seed = root/'seed'
            seed.mkdir()
            run('seed-init', git+['init', '-b', 'main', seed])
            (seed/'synthetic.txt').write_text('synthetic seed\n')
            run('seed-add', git+['add', '.'], seed)
            run('seed-commit', git+['-c', 'user.name=Izu Synthetic Acceptance', '-c',
                                    'user.email=izu-acceptance@example.invalid', 'commit', '-m', 'Synthetic acceptance seed'], seed)
            run('seed-tag', git+['tag', 'synthetic-v1'], seed)
            run('seed-protected', git+['branch', 'protected'], seed)
            run('seed-push', git+['push', URL, 'main', 'protected', 'refs/tags/synthetic-v1'], seed)
            refs('seed-independent-readback')
            run('independent-clone', git+['clone', URL, root/'readback'])
            run('independent-fsck', git+['fsck', '--full'], root/'readback')
        elif a.phase == 'exercise':
            metadata = json.loads(run('target-metadata-exercise', ['gh', 'api', f'repos/{TARGET}']))
            if metadata['full_name'] != TARGET or not metadata['private']:
                raise RuntimeError('Exact private target identity mismatch')
            native = root/'native'
            izu('izu-init', ['init', native], root)
            imported = json.loads(izu('izu-import-main', ['git', 'import', URL, '--native-ref',
                                'imported-main', '--git-ref', 'refs/heads/main', '--expect-absent', *auth], native))
            (root/'import-result.json').write_text(json.dumps(imported, indent=2)+'\n')
            izu('izu-refs', ['refs', 'list'], native)
            izu('izu-update-import', ['update', 'imported-main'], native)
            izu('izu-inspect', ['inspect'], native)
        elif a.phase == 'acceptance':
            metadata = json.loads(run('target-metadata-acceptance', ['gh', 'api', f'repos/{TARGET}']))
            if metadata['full_name'] != TARGET or not metadata['private']:
                raise RuntimeError('Exact private target identity mismatch')
            native = root/'native'
            initial = json.loads(json.loads((root/'izu-refs.json').read_text())['stdout'])['outcome']['result']['data']
            old_native = initial['imported-main']
            old_git = json.loads(json.loads((root/'izu-import-main.json').read_text())['stdout'])['outcome']['result']['data']['inventory']['branches']['refs/heads/main']
            if not (root/'izu-push-main.json').exists():
                izu('izu-align-main', ['refs', 'set', 'main', old_native, '--expect', initial['main']], native)
                (native/'synthetic.txt').write_text('synthetic native change A\n')
                izu('izu-commit-a', ['commit', '-m', 'Synthetic native change A'], native)
                state = json.loads(izu('izu-refs-a', ['refs', 'list'], native))['outcome']['result']['data']
                a_revision = state['main']
                izu('izu-retain-a', ['refs', 'set', 'race-a', a_revision, '--expect-absent'], native)
                izu('izu-push-main', ['git', 'push', URL, '--native-ref', 'race-a', '--git-ref', 'refs/heads/main', '--expect', old_git, *auth], native, expect=None)
            else:
                a_revision = json.loads(json.loads((root/'izu-refs-a.json').read_text())['stdout'])['outcome']['result']['data']['main']
            new_refs = refs('push-main-independent-readback')
            new_git = next(line.split()[0] for line in new_refs.splitlines() if line.endswith('refs/heads/main'))
            run('readback-fetch-a', git+['fetch', URL, 'main'], root/'readback')
            content = run('readback-content-a', git+['show', 'FETCH_HEAD:synthetic.txt'], root/'readback')
            if content != 'synthetic native change A\n':
                raise RuntimeError('Independent Git content mismatch')
            izu('izu-stale-lease', ['git', 'push', URL, '--native-ref', 'race-a', '--git-ref', 'refs/heads/main', '--expect', old_git, *auth], native, expect=None)
            izu('izu-fetch-main', ['git', 'fetch', URL, '--native-ref', 'imported-main', '--git-ref', 'refs/heads/main', '--expect', old_native, *auth], native)
            izu('izu-race-base', ['refs', 'set', 'race-base', old_native, '--expect-absent'], native)
            izu('izu-race-seed', ['git', 'push', URL, '--native-ref', 'race-base', '--git-ref', 'refs/heads/race', '--expect-absent', *auth], native, expect=None)
            izu('izu-reset-main', ['refs', 'set', 'main', old_native, '--expect', a_revision], native)
            izu('izu-reset-workspace', ['update', 'main'], native)
            (native/'synthetic.txt').write_text('synthetic native change B\n')
            izu('izu-commit-b', ['commit', '-m', 'Synthetic native change B'], native)
            b_revision = json.loads(izu('izu-refs-b', ['refs', 'list'], native))['outcome']['result']['data']['main']
            izu('izu-retain-b', ['refs', 'set', 'race-b', b_revision, '--expect-absent'], native)
            # Start two independent executable writers with the same exact remote lease.
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                results = list(pool.map(lambda ref: izu('izu-concurrent-'+ref, ['git', 'push', URL, '--native-ref', ref, '--git-ref', 'refs/heads/race', '--expect', old_git, '--allow-non-fast-forward', *auth], native, expect=None), ['race-a', 'race-b']))
            refs('concurrent-independent-readback')
            izu('izu-invalid-auth', ['git', 'fetch', URL, '--native-ref', 'invalid-auth', '--git-ref', 'refs/heads/main', '--expect-absent', *auth], native, expect=None, extra_env={'IZU_REMOTE_ACCEPTANCE_TOKEN':'invalid-synthetic-token'})
            izu('izu-no-auth', ['git', 'fetch', URL, '--native-ref', 'no-auth', '--git-ref', 'refs/heads/main', '--expect-absent'], native, expect=None, extra_env={'IZU_REMOTE_ACCEPTANCE_TOKEN':''})
            policy = root/'protection-request.json'
            policy.write_text(json.dumps(dict(required_status_checks=None, enforce_admins=True, required_pull_request_reviews=dict(required_approving_review_count=1), restrictions=None)))
            run('branch-protection-request', ['gh', 'api', '--method', 'PUT', f'repos/{TARGET}/branches/protected/protection', '--input', policy], expect=None)
            izu('izu-protected-push', ['git', 'push', URL, '--native-ref', 'race-b', '--git-ref', 'refs/heads/protected', '--expect', old_git, *auth], native, expect=None)
            refs('protected-independent-readback')
            # Cancellation is timed at process launch; readback distinguishes actual visibility.
            (native/'cancel-payload.bin').write_bytes(os.urandom(4*1024*1024))
            izu('izu-cancel-payload-commit', ['commit', '-m', 'Synthetic cancellation payload'], native)
            izu('izu-cancel-push', ['git', 'push', URL, '--native-ref', 'main', '--git-ref', 'refs/heads/cancel', '--expect-absent', *auth], native, expect=None, cancel_after=0.25)
            refs('cancel-independent-readback')
            run('readback-final-fetch', git+['fetch', URL, '+refs/heads/*:refs/remotes/acceptance/*'], root/'readback')
            run('readback-final-fsck', git+['fsck', '--full'], root/'readback')
        elif a.phase == 'final':
            prefix = a.run_label+'-'
            metadata = json.loads(run(prefix+'target-metadata', ['gh', 'api', f'repos/{TARGET}']))
            if metadata['full_name'] != TARGET or not metadata['private']:
                raise RuntimeError('Exact private target identity mismatch')
            before = refs(prefix+'before-readback')
            for branch in [prefix+'main', prefix+'race', prefix+'cancel']:
                if any(line.endswith('refs/heads/'+branch) for line in before.splitlines()):
                    raise RuntimeError('Final fixture branch must be absent: '+branch)
            baseline_git = next(line.split()[0] for line in before.splitlines() if line.endswith('refs/heads/main'))
            native = root/(prefix+'native')
            izu(prefix+'izu-clone', ['git', 'clone', URL, native, '--native-ref', 'main', '--git-ref', 'refs/heads/main', '--expect-absent', *auth], root)
            baseline = json.loads(izu(prefix+'baseline-refs', ['refs', 'list'], native))['outcome']['result']['data']['main']
            izu(prefix+'retain-base', ['refs', 'set', 'base', baseline, '--expect-absent'], native)
            (native/'synthetic.txt').write_text('final synthetic native A\n')
            izu(prefix+'commit-a', ['commit', '-m', 'Final synthetic A'], native)
            a_revision = json.loads(izu(prefix+'refs-a', ['refs', 'list'], native))['outcome']['result']['data']['main']
            izu(prefix+'retain-a', ['refs', 'set', 'a', a_revision, '--expect-absent'], native)
            fixed = ['--timestamp', '1791180000']
            izu(prefix+'push-a', ['git', 'push', URL, '--native-ref', 'a', '--git-ref', 'refs/heads/'+prefix+'main', '--expect-absent', *fixed, *auth], native)
            refs(prefix+'push-independent-readback')
            run(prefix+'readback-fetch-a', git+['fetch', URL, prefix+'main'], root/'readback')
            content = run(prefix+'readback-content-a', git+['show', 'FETCH_HEAD:synthetic.txt'], root/'readback')
            if content != 'final synthetic native A\n':
                raise RuntimeError('Final independent Git content mismatch')
            izu(prefix+'stale-lease', ['git', 'push', URL, '--native-ref', 'a', '--git-ref', 'refs/heads/'+prefix+'main', '--expect-absent', *fixed, *auth], native, expect=4)
            izu(prefix+'fetch-main', ['git', 'fetch', URL, '--native-ref', 'fetched', '--git-ref', 'refs/heads/'+prefix+'main', '--expect-absent', *auth], native)
            izu(prefix+'race-seed', ['git', 'push', URL, '--native-ref', 'base', '--git-ref', 'refs/heads/'+prefix+'race', '--expect-absent', *fixed, *auth], native)
            izu(prefix+'reset-main', ['refs', 'set', 'main', baseline, '--expect', a_revision], native)
            izu(prefix+'reset-workspace', ['update', 'main'], native)
            (native/'synthetic.txt').write_text('final synthetic native B\n')
            izu(prefix+'commit-b', ['commit', '-m', 'Final synthetic B'], native)
            b_revision = json.loads(izu(prefix+'refs-b', ['refs', 'list'], native))['outcome']['result']['data']['main']
            izu(prefix+'retain-b', ['refs', 'set', 'b', b_revision, '--expect-absent'], native)
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                list(pool.map(lambda ref: izu(prefix+'concurrent-'+ref, ['git', 'push', URL, '--native-ref', ref, '--git-ref', 'refs/heads/'+prefix+'race', '--expect', baseline_git, *fixed, *auth], native, expect=None), ['a', 'b']))
            codes = sorted(json.loads((root/(prefix+'concurrent-'+ref+'.json')).read_text())['exit_code'] for ref in ['a','b'])
            if codes[0] != 0 or codes[1] in [0,6]:
                raise RuntimeError('Final race did not yield one success and one definite rejection: '+str(codes))
            refs(prefix+'race-independent-readback')
            izu(prefix+'invalid-auth', ['git', 'fetch', URL, '--native-ref', 'invalid-auth', '--git-ref', 'refs/heads/main', '--expect-absent', *auth], native, expect=2, extra_env={'IZU_REMOTE_ACCEPTANCE_TOKEN':'invalid-synthetic-token'})
            izu(prefix+'no-auth', ['git', 'fetch', URL, '--native-ref', 'no-auth', '--git-ref', 'refs/heads/main', '--expect-absent'], native, expect=2)
            (native/(prefix+'cancel.bin')).write_bytes(os.urandom(4*1024*1024))
            izu(prefix+'cancel-commit', ['commit', '-m', 'Final synthetic cancellation'], native)
            izu(prefix+'cancel-push', ['git', 'push', URL, '--native-ref', 'main', '--git-ref', 'refs/heads/'+prefix+'cancel', '--expect-absent', *fixed, *auth], native, expect=130, cancel_after=0.25)
            refs(prefix+'cancel-independent-readback')
            run(prefix+'readback-fetch', git+['fetch', URL, '+refs/heads/*:refs/remotes/acceptance/*'], root/'readback')
            run(prefix+'readback-fsck', git+['fsck', '--full'], root/'readback')
        elif a.phase == 'candidate-ci':
            if not a.source_root or not a.source_archive or not a.source_digest:
                raise RuntimeError('Candidate CI requires explicit source-root, frozen archive and exact archive SHA-256')
            source = a.source_root.resolve(strict=True)
            archive = a.source_archive.resolve(strict=True)
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            if digest != a.source_digest or len(a.source_digest) != 64:
                raise RuntimeError('Frozen source archive digest mismatch')
            metadata = json.loads(run('candidate-target-metadata', ['gh', 'api', f'repos/{TARGET}']))
            if metadata['full_name'] != TARGET or not metadata['private']:
                raise RuntimeError('Exact private candidate target identity mismatch')
            advertised = refs('candidate-before-readback')
            candidate_branch = 'candidate-ci' if a.run_label == 'final' else 'candidate-ci-'+a.run_label
            if any(line.endswith('refs/heads/'+candidate_branch) for line in advertised.splitlines()):
                raise RuntimeError('Candidate branch must be absent; refusing overwrite')
            candidate = root/'candidate-source'
            candidate.mkdir()
            # Only lead-provided immutable source. Reject archive links, devices and escapes.
            with tarfile.open(archive) as bundle:
                members = bundle.getmembers()
                if len(members) > 100000 or sum(m.size for m in members)>256*1024*1024:
                    raise RuntimeError('Candidate archive exceeds acceptance bounds')
                for member in members:
                    parts = Path(member.name).parts
                    if member.name.startswith('/') or '..' in parts or '.git' in parts or '.artifacts' in parts or not (member.isfile() or member.isdir()):
                        raise RuntimeError('Unsupported or unsafe source archive member')
                bundle.extractall(candidate, members=members, filter='data')
            if not (candidate/'Cargo.toml').is_file() or not (candidate/'.github/workflows/ci.yml').is_file():
                raise RuntimeError('Frozen candidate is missing root manifest or expected CI workflow')
            run('candidate-init', git+['init', '-b', candidate_branch], candidate)
            run('candidate-add', git+['add', '.'], candidate)
            run('candidate-commit', git+['-c', 'user.name=Izu Synthetic Acceptance', '-c', 'user.email=izu-acceptance@example.invalid', 'commit', '-m', 'Frozen candidate acceptance '+digest[:16]], candidate)
            candidate_oid = run('candidate-oid', git+['rev-parse', 'HEAD'], candidate).strip()
            expected_files = {member.name: hashlib.sha256((candidate/member.name).read_bytes()).hexdigest() for member in members if member.isfile()}
            tracked = subprocess.run(git+['ls-tree', '-rz', '--name-only', 'HEAD'], cwd=candidate, env=env, capture_output=True, check=True).stdout
            actual_files = {name.decode(): hashlib.sha256(subprocess.run(git+['show', 'HEAD:'+name.decode()], cwd=candidate, env=env, capture_output=True, check=True).stdout).hexdigest() for name in tracked.split(b'\0') if name}
            if expected_files != actual_files:
                raise RuntimeError('Disposable Git candidate tree differs from frozen archive; do not publish')
            (root/'candidate-git-tree-binding.json').write_text(json.dumps(dict(commit=candidate_oid, files=len(actual_files), matches_frozen_source=True), indent=2)+'\n')
            (root/'candidate-source-binding.json').write_text(json.dumps(dict(source_root=str(source), source_archive=str(archive), archive_sha256=digest, commit=candidate_oid, target=TARGET, branch=candidate_branch, synthetic_disposable_commit=True),indent=2)+'\n')
            run('candidate-push', git+['push', '--force-with-lease=refs/heads/'+candidate_branch+':', URL, 'HEAD:refs/heads/'+candidate_branch], candidate)
            refs('candidate-after-independent-readback')
            readback = root/'candidate-independent-readback'
            readback.mkdir()
            run('candidate-readback-init', git+['init', '--bare'], readback)
            run('candidate-readback-fetch', git+['fetch', '--no-tags', URL,
                'refs/heads/'+candidate_branch+':refs/heads/verified-candidate'], readback)
            observed_oid = run('candidate-readback-oid', git+['rev-parse',
                'refs/heads/verified-candidate'], readback).strip()
            if observed_oid != candidate_oid:
                raise RuntimeError('Independent remote candidate commit differs from frozen candidate')
            local_tree = subprocess.run(git+['ls-tree', '-rz', 'HEAD'], cwd=candidate,
                env=env, capture_output=True, check=True).stdout
            remote_tree = subprocess.run(git+['ls-tree', '-rz', observed_oid], cwd=readback,
                env=env, capture_output=True, check=True).stdout
            if remote_tree != local_tree:
                raise RuntimeError('Independent remote candidate tree modes/blob IDs differ')
            remote_files = {name: hashlib.sha256(subprocess.run(git+['show',
                observed_oid+':'+name], cwd=readback, env=env, capture_output=True,
                check=True).stdout).hexdigest() for name in expected_files}
            if remote_files != expected_files:
                raise RuntimeError('Independent remote candidate files differ from frozen archive')
            (root/'candidate-independent-tree-binding.json').write_text(json.dumps(dict(
                commit=observed_oid, files=len(remote_files), archive_sha256=digest,
                tree_modes_and_blob_ids_match=True, all_file_sha256_match=True), indent=2)+'\n')
            run('candidate-runs-initial', ['gh', 'api', f'repos/{TARGET}/actions/runs?head_sha={candidate_oid}'])
        else:
            branch = 'receipt-capture'
            advertised = refs('capture-before-readback')
            if any(line.endswith('refs/heads/'+branch) for line in advertised.splitlines()):
                raise RuntimeError('Capture target must be absent')
            exported = root/'capture-export'
            izu('capture-native-export', ['git', 'export', exported], root/'native')
            oid = run('capture-export-head', git+['rev-parse', 'refs/heads/main'], exported).strip()
            packed = subprocess.run(git+['pack-objects', '--all', '--stdout'], cwd=exported, env=env, capture_output=True, check=True).stdout
            command = ('0'*40+' '+oid+' refs/heads/'+branch+'\0report-status\n').encode()
            payload = f'{len(command)+4:04x}'.encode()+command+b'0000'+packed
            request = urllib.request.Request(URL+'/git-receive-pack', data=payload, method='POST',
                headers={'Authorization':'Basic '+base64.b64encode(('x-access-token:'+token).encode()).decode(),
                         'Content-Type':'application/x-git-receive-pack-request',
                         'Accept':'application/x-git-receive-pack-result'})
            class NoRedirect(urllib.request.HTTPRedirectHandler):
                def redirect_request(self, req, fp, code, msg, headers, newurl):
                    raise RuntimeError('Credential-bearing acceptance request refused redirect')
            opener = urllib.request.build_opener(NoRedirect())
            with opener.open(request, timeout=120) as response:
                body = response.read(1048577)
                if len(body)>1048576:
                    raise RuntimeError('Response exceeds bounded capture')
                (root/'github-receive-body.bin').write_bytes(body)
                (root/'github-receive-body.json').write_text(json.dumps(dict(status=response.status,
                    response_bytes=len(body), hex=body.hex(), target=TARGET, branch=branch,
                    expected_old=None, attempted=oid, request_pack_sha256=hashlib.sha256(packed).hexdigest()), indent=2)+'\n')
                print('receive body captured:', len(body), 'bytes', flush=True)
            refs('capture-after-independent-readback')
        (root/f'{a.run_label if a.phase == "final" else a.phase}-summary.json').write_text(json.dumps(dict(target=TARGET, url=URL,
            phase=a.phase, labels=[r['label'] for r in receipts], binary=str(binary),
            sha256=hashlib.sha256(binary.read_bytes()).hexdigest(), deletion_performed=False), indent=2)+'\n')
    finally:
        askpass.unlink(missing_ok=True)


if __name__ == '__main__':
    main()
