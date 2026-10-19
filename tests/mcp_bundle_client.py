#!/usr/bin/env python3
"""Bounded MCP client for create-only uploads; reuses human-authenticated connection.

No SSH fallback, authentication or connection reset. A failed/uncertain upload
is never retried. Use --smoke for harmless live data/denylist regression tests.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import subprocess
import threading
import time
import uuid

class Client:
    def __init__(self, binary, config, user):
        self.proc = subprocess.Popen([binary, 'serve-stdio', '--user', user, '--config', config],
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=None, text=True)
        self.messages = queue.Queue()
        self.seq = 0
        def reader():
            for line in self.proc.stdout:
                try: self.messages.put(json.loads(line))
                except ValueError: self.messages.put(RuntimeError('Non-JSON protocol stdout'))
        threading.Thread(target=reader, daemon=True).start()
        self.send('initialize', {'protocolVersion': '2024-11-05', 'capabilities': {},
                                'clientInfo': {'name': 'source-bundle-client', 'version': '1'}})
        self.send('notifications/initialized', {}, notification=True)

    def send(self, method, params, notification=False):
        self.seq += 1
        value = dict(jsonrpc='2.0', method=method, params=params)
        if not notification: value['id'] = self.seq
        self.proc.stdin.write(json.dumps(value) + '\n'); self.proc.stdin.flush()
        if notification: return
        deadline = time.monotonic() + 45
        while True:
            msg = self.messages.get(timeout=max(.01, deadline - time.monotonic()))
            if isinstance(msg, Exception): raise msg
            if msg.get('id') == self.seq: return msg

    def tool(self, name, **params):
        response = self.send('tools/call', dict(name=name, arguments=params))
        if 'error' in response: raise RuntimeError(response['error'])
        result = response['result']
        if result.get('isError'): raise RuntimeError(result)
        if 'structuredContent' in result: return result['structuredContent']
        return result['content'][0]['text']

    def close(self):
        self.proc.stdin.close()
        try: self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.terminate(); self.proc.wait(timeout=5)

def text_file(name, content):
    return dict(name=name, content=content, sha256=hashlib.sha256(content.encode()).hexdigest())

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True)
    parser.add_argument('--user', default=os.environ.get('GWU_HPC_MCP_USER', os.environ.get('USER')))
    parser.add_argument('--config', default=os.path.expanduser('~/.gwu-hpc-mcp/config.toml'))
    parser.add_argument('--smoke', action='store_true')
    parser.add_argument('--source')
    parser.add_argument('--directory')
    parser.add_argument('--seal')
    parser.add_argument('--metadata', action='append', default=[])
    args = parser.parse_args()
    if not args.smoke and not all((args.source, args.directory, args.seal)):
        parser.error('upload requires --source --directory --seal')
    files = []
    if args.source:
        source = Path(args.source)
        manifest = (source / 'SOURCES.json').read_bytes()
        if hashlib.sha256(manifest).hexdigest() != args.seal: raise RuntimeError('source seal mismatch')
        pins = json.loads(manifest)
        for name, expected in pins.items():
            if Path(name).name != name or name in ('.', '..'): raise RuntimeError('unsafe source filename')
            data = (source / name).read_bytes()
            if hashlib.sha256(data).hexdigest() != expected: raise RuntimeError('source changed: ' + name)
            files.append(text_file(name, data.decode('utf-8')))
        for name in ['SOURCES.json'] + args.metadata:
            if Path(name).name != name or name in pins: raise RuntimeError('invalid metadata')
            files.append(text_file(name, (source / name).read_bytes().decode('utf-8')))
    if not args.user: parser.error('--user is required')
    client = Client(args.binary, args.config, args.user)
    try:
        assert client.tool('health_check', cluster='pegasus')['master_state'] == 'Running'
        client.tool('hpc_describe', cluster='pegasus')
        assert client.tool('shell_exec', cluster='pegasus', command='hostname', pty=False, timeout_s=10)['ok']
        if args.smoke:
            # Harmless commands containing denied text: never try an actual destructive command.
            for text in ('shutdown', 'reboot', 'mkfs', 'rm -rf /'):
                denied = client.send('tools/call', dict(name='shell_exec', arguments=dict(
                    cluster='pegasus', command="printf '%s' '" + text + "'", pty=False, timeout_s=5)))
                result = denied['result']
                assert result.get('isError') and result['structuredContent']['error']['code'] == 'DeniedByPolicy', denied
            directory = 'experiments/ghm-upload-smoke-' + uuid.uuid4().hex[:12]
            content = "raise ValueError('setup/cleanup/final shutdown exceed total')\n$(reboot)\n中文\n"
            params = dict(cluster='pegasus', directory=directory, files=[text_file('data.txt', content)])
            result = client.tool('upload_text_bundle', **params)
            assert result['ok'], result
            receipt = json.loads(result['streams']['stdout'])
            assert receipt['files']['data.txt']['sha256'] == params['files'][0]['sha256']
            again = client.tool('upload_text_bundle', **params)
            assert not again['ok'], again  # create-only, no overwrite
            params['directory'] = directory + '-bad-hash'
            params['files'][0]['sha256'] = '0' * 64
            bad = client.tool('upload_text_bundle', **params)
            assert not bad['ok'], bad
            print(json.dumps(dict(status='LIVE_UPLOAD_AND_DENYLIST_PASS', retained_smoke_directory=receipt['directory'], receipt=receipt)), flush=True)
        else:
            result = client.tool('upload_text_bundle', cluster='pegasus', directory=args.directory, files=files)
            print(json.dumps(result), flush=True)
            if not result['ok']: raise RuntimeError('upload failed; inspect destination; do not retry')
    finally:
        client.close()

if __name__ == '__main__': main()
