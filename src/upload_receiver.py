"""Fixed create-only source receiver. JSON stdin is data, never executable code.

Keep Python 3.6 compatibility for Pegasus login nodes. This performs bounded
file IO and SHA-256 only; no subprocesses, imports from uploads, or model work.
"""
import hashlib
import json
import os
import re
import sys

MAX_BYTES = 8 * 1024 * 1024
raw = sys.stdin.buffer.read(MAX_BYTES + 1)
if len(raw) > MAX_BYTES:
    raise ValueError('upload request exceeds 8 MiB')
request = json.loads(raw)
root = request['root']  # selected by server config, NOT a caller parameter
directory = request['directory']
files = request['files']

def component(s):
    return isinstance(s, str) and re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]{0,199}', s)

if not isinstance(root, str) or not root.startswith('/') or len(root.strip('/').split('/')) < 3:
    raise ValueError('configured project root must be an absolute, bounded directory')
parts = directory.split('/')
if not parts or len(parts) > 12 or not all(component(p) for p in parts):
    raise ValueError('directory must be a safe project-relative path')
if not isinstance(files, list) or not 1 <= len(files) <= 128:
    raise ValueError('expected 1..128 text files')
prepared = []
names = set()
for f in files:
    name, content, expected = f['name'], f['content'], f['sha256']
    if not component(name) or name in names or not isinstance(content, str):
        raise ValueError('invalid or duplicate flat filename/content')
    if not isinstance(expected, str) or not re.fullmatch(r'[0-9a-f]{64}', expected):
        raise ValueError('invalid SHA-256')
    data = content.encode('utf-8')
    if hashlib.sha256(data).hexdigest() != expected:
        raise ValueError('content SHA-256 mismatch: ' + name)
    names.add(name)
    prepared.append((name, data, expected))

# Authenticate all data before any filesystem mutation. Follow only the
# configured root's established alias (e.g. /scratch -> /gpfs/.../scratch).
# Below it, anchored dirfds and O_NOFOLLOW reject symlink escapes/races.
flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
root_path = os.path.realpath(root)
fd = os.open(root_path, flags)
created = False
try:
    for part in parts[:-1]:
        # Parent directories must already exist. Do not build arbitrary trees.
        next_fd = os.open(part, flags, dir_fd=fd)
        os.close(fd)
        fd = next_fd
    os.mkdir(parts[-1], mode=0o700, dir_fd=fd)  # never replace an existing path
    created = True
    next_fd = os.open(parts[-1], flags, dir_fd=fd)
    os.close(fd)
    fd = next_fd
    receipt = {}
    for name, data, expected in prepared:
        fno = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                      0o600, dir_fd=fd)
        with os.fdopen(fno, 'wb') as output:
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        with os.fdopen(os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=fd), 'rb') as check:
            actual = hashlib.sha256(check.read()).hexdigest()
        if actual != expected:
            raise ValueError('readback mismatch: ' + name)
        receipt[name] = {'sha256': actual, 'bytes': len(data)}
    os.fsync(fd)
    print(json.dumps({'status': 'CREATED_NOT_EXECUTED',
                      'directory': root_path + '/' + directory, 'files': receipt}))
except Exception:
    if created:
        print('Partial destination retained; inspect it before any retry.', file=sys.stderr)
    raise
finally:
    os.close(fd)
