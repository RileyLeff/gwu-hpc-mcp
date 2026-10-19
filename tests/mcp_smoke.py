#!/usr/bin/env python3
"""Opt-in live MCP integration test; requires the user's authenticated cluster."""
import argparse
import json
import os
import pathlib
import queue
import subprocess
import tempfile
import threading
import time
import uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', default='ghm')
args = parser.parse_args()

with tempfile.TemporaryDirectory(prefix='ghm-smoke-') as temp:
    config = pathlib.Path(temp) / 'config.toml'
    source = pathlib.Path(os.environ.get('GWU_HPC_MCP_CONFIG', '~/.gwu-hpc-mcp/config.toml')).expanduser()
    config.write_bytes(source.read_bytes())
    with (pathlib.Path(temp) / 'stderr').open('w+') as err:
        proc = subprocess.Popen([args.binary, 'serve-stdio', '--config', str(config)],
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=err, text=True, env={**os.environ, 'RUST_LOG': 'info'})
        messages = queue.Queue()
        def reader():
            for line in proc.stdout:
                try:
                    messages.put(json.loads(line))
                except ValueError:
                    messages.put(RuntimeError('Non-JSON stdout: ' + line))
        threading.Thread(target=reader, daemon=True).start()
        seq = 0
        def send(method, params, request=True):
            global seq
            seq += 1
            payload = {'jsonrpc': '2.0', 'method': method, 'params': params}
            if request:
                payload['id'] = seq
            proc.stdin.write(json.dumps(payload) + '\n')
            proc.stdin.flush()
            if not request:
                return
            deadline = time.monotonic() + 45
            while True:
                msg = messages.get(timeout=max(.1, deadline - time.monotonic()))
                if isinstance(msg, Exception):
                    raise msg
                if msg.get('id') == seq:
                    assert 'error' not in msg, msg
                    return msg['result']
        def tool(tool_name, **params):
            result = send('tools/call', {'name': tool_name, 'arguments': params})
            assert not result.get('isError'), result
            return result.get('structuredContent') or json.loads(result['content'][0]['text'])
        def expected_error(tool_name, **params):
            result = send('tools/call', {'name': tool_name, 'arguments': params})
            assert result.get('isError') and result['structuredContent']['error']['code'] == 'Timeout', result
            return result
        session = 'mcp_smoke_' + uuid.uuid4().hex[:12]
        opened = False
        try:
            init = send('initialize', {'protocolVersion': '2024-11-05', 'capabilities': {},
                                      'clientInfo': {'name': 'ghm-smoke', 'version': '1'}})
            send('notifications/initialized', {}, request=False)
            assert tool('health_check')['master_state'] == 'Running'
            tool('hpc_describe')
            print('PASS protocol handshake, health, describe', flush=True)
            # Trigger a real tracing message: it must never enter protocol stdout.
            config.write_text(config.read_text() + '\n# smoke reload\n')
            time.sleep(.4)
            tool('health_check')
            err.flush()
            err.seek(0)
            assert 'Reloaded config' in err.read(), 'reload log missing from stderr'
            print('PASS hot reload and stderr logging', flush=True)
            r = tool('shell_exec', command="printf 'hello'; printf 'problem' >&2; exit 7", cwd='/tmp', pty=False, timeout_s=10)
            assert r['ok'] is False and r['exit_code'] == 7, r
            assert r['streams']['stdout'] == 'hello' and r['streams']['stderr'] == 'problem', r
            r = tool('shell_exec', command='printf SHOULD_NOT_RUN; printf ALSO_NOT_RUN', cwd='/nonexistent-ghm-smoke', pty=False, timeout_s=10)
            assert not r['ok'] and 'SHOULD_NOT_RUN' not in r['streams'].get('stdout', ''), r
            print('PASS shell errors, stream separation, invalid cwd', flush=True)
            tool('session_open', name=session, cwd='/tmp')
            opened = True
            r = tool('session_exec', name=session, command="printf '%s' \"$GHM_TEST\"; exit 7", env={'GHM_TEST': 'quoted 中文 🦀'}, timeout_s=10)
            assert r['ok'] is False and r['exit_code'] == 7 and r['streams']['tmux'] == 'quoted 中文 🦀', r
            command = "printf '\\033[2J'; for i in {1..2000}; do printf 'line-%s 中文 🦀\\n' \"$i\"; done"
            r = tool('session_exec', name=session, command=command, timeout_s=10)
            assert r['ok'] and r['streams']['tmux'].count('\n') == 2000, str(r)[:200]
            assert 'line-2000 中文 🦀' in r['streams']['tmux']
            r = tool('session_exec', name=session, command='printf SHOULD_NOT_RUN; printf ALSO_NOT_RUN', cwd='/nonexistent-ghm-smoke', timeout_s=10)
            assert not r['ok'] and 'SHOULD_NOT_RUN' not in r['streams']['tmux'], r
            print('PASS tmux nonzero exit, Unicode, clearing, large output, invalid cwd', flush=True)
            expected_error('session_exec', name=session, command='sleep 10', timeout_s=1)
            r = tool('session_exec', name=session, command='printf recovered', timeout_s=10)
            assert r['ok'] and r['streams']['tmux'] == 'recovered', r
            print('PASS tmux timeout and reuse', flush=True)
        finally:
            if opened:
                tool('session_close', name=session)
                print('PASS session cleanup', flush=True)
            proc.stdin.close()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.terminate()
                proc.wait(timeout=5)
