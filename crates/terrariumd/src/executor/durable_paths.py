"""Open the durable spool through checked, no-follow directory descriptors.

Only the remote UID is trusted inside these directories. As with target_roots,
directory FDs prevent redirection, not a trusted same-UID process renaming an
already opened directory. No path below the checked root is reopened absolute.
"""
import os
import stat
import sys


def check(fd, label, directory=True):
    meta = os.fstat(fd)
    kind = stat.S_ISDIR if directory else stat.S_ISREG
    if not kind(meta.st_mode):
        raise RuntimeError(label + ': wrong file type')
    if meta.st_uid != os.geteuid():
        raise RuntimeError(label + ': foreign owner')
    if meta.st_mode & 0o022:
        raise RuntimeError(label + ': group/other writable')


def directory(parent, name, create=False, validate=True):
    new = False
    if create:
        try:
            os.mkdir(name, 0o700, dir_fd=parent)
            new = True
        except FileExistsError:
            pass
    # O_NOFOLLOW plus fstat, not a path check followed by a following open.
    fd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
    if validate:
        check(fd, name)
    return fd, new


def marker(fd, value, create=False):
    if create:
        out = os.open('.terrarium-owner', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=fd)
        with os.fdopen(out, 'w') as stream:
            stream.write(value)
    inp = os.open('.terrarium-owner', os.O_RDONLY | os.O_NOFOLLOW, dir_fd=fd)
    check(inp, 'owner marker', False)
    with os.fdopen(inp) as stream:
        if stream.read(4096) != value:
            raise RuntimeError('owner marker mismatch')


def main():
    root, owner, card, token, mode, script = sys.argv[1:]
    os.umask(0o077)
    fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY)
    parts = root.strip('/').split('/')
    if not parts or any(p in ('', '.', '..') for p in parts):
        raise RuntimeError('invalid root')
    for i, part in enumerate(parts):
        next_fd, _ = directory(fd, part, create=(mode == 'run' and card == '0'), validate=(i == len(parts)-1))
        os.close(fd)
        fd = next_fd
    if card == '1':
        marker(fd, owner)
    parent, new = directory(fd, '.terrarium-runs', create=(mode == 'run'))
    os.close(fd)
    marker(parent, owner, new)
    claim, new = directory(parent, token, create=(mode == 'run'))
    marker(claim, token, new)
    # Keep both directory identities across exec and through detached children.
    os.dup2(parent, 8, inheritable=True)
    os.dup2(claim, 7, inheritable=True)
    os.environ['TERRARIUM_ORIGINAL_CWD'] = os.getcwd()
    os.fchdir(claim)
    os.environ['TERRARIUM_NEW_CLAIM'] = '1' if new else '0'
    if mode == 'cancel':
        cancel = os.open('cancel', os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW, 0o600, dir_fd=claim)
        os.close(cancel)
    else:
        os.execv('/bin/sh', ['sh', '-c', script])


try:
    main()
except (OSError, RuntimeError) as error:
    # No secrets or environment values in this diagnostic.
    print('Bspool error: ' + str(error), flush=True)
    sys.exit(1)
