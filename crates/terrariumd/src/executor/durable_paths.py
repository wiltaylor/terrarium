"""Open the durable spool through checked, no-follow directory descriptors.

Only the remote UID is trusted inside these directories. As with target_roots,
directory FDs prevent redirection, not a trusted same-UID process renaming an
already opened directory. No path below the checked root is reopened absolute.

A caller without an owned root and without a target work_root uses the default
root, else ~/.terrarium-work of the remote user when that user may not create
the default (EACCES, EPERM, EROFS, or a missing parent in a lookup). A claim
that already exists under the fallback stays there, so a resume never moves.
"""
import errno
import os
import stat
import sys

# The file system root of the walk. Tests replace it with a fake root.
ROOT = '/'
FALLBACK = '.terrarium-work'
UNAVAILABLE = (errno.EACCES, errno.EPERM, errno.EROFS, errno.ENOENT)


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


def parts_of(root):
    parts = root.strip('/').split('/')
    if not root.startswith('/') or any(p in ('', '.', '..') for p in parts):
        raise RuntimeError('invalid root')
    return parts


def open_root(root, create):
    parts = parts_of(root)
    fd = os.open(ROOT, os.O_RDONLY | os.O_DIRECTORY)
    try:
        for i, part in enumerate(parts):
            next_fd, _ = directory(fd, part, create=create, validate=(i == len(parts)-1))
            os.close(fd)
            fd = next_fd
    except BaseException:
        os.close(fd)
        raise
    return fd


def fallback_root():
    home = os.path.expanduser('~')
    if not home.startswith('/'):
        return None
    root = home.rstrip('/') + '/' + FALLBACK
    try:
        parts_of(root)
    except RuntimeError:
        return None
    return root


def has_claim(root, token):
    """Whether root already holds this claim. Only a hint: the real open
    below checks every directory again."""
    try:
        fd = open_root(root, False)
    except (OSError, RuntimeError):
        return False
    try:
        os.stat('.terrarium-runs/' + token, dir_fd=fd, follow_symlinks=False)
        return True
    except OSError:
        return False
    finally:
        os.close(fd)


def hint(target):
    return ('set a writable work_root with `terrarium admin target edit '
            + (target or '<target>') + ' --work-root <absolute path>`')


def resolve(root, card, token, mode, fallback, target):
    create = mode == 'run' and card == '0'
    if card == '0' and fallback == '1':
        home = fallback_root()
        if home is not None and has_claim(home, token):
            return open_root(home, create)
        try:
            return open_root(root, create)
        except OSError as error:
            if error.errno not in UNAVAILABLE:
                raise
            if home is None:
                raise RuntimeError('cannot use ' + root + ' (' + str(error)
                                   + ') and the remote user has no absolute home for the fallback; ' + hint(target))
            try:
                return open_root(home, create)
            except OSError as second:
                raise RuntimeError('cannot use ' + root + ' (' + str(error) + ') or the fallback '
                                   + home + ' (' + str(second) + '); ' + hint(target))
    try:
        return open_root(root, create)
    except OSError as error:
        if card == '0' and error.errno in (errno.EACCES, errno.EPERM, errno.EROFS):
            raise RuntimeError('work_root ' + root + ' is not writable by the remote user ('
                               + str(error) + '); fix its permissions or ' + hint(target))
        raise


def main():
    root, owner, card, token, mode, script, fallback, target = sys.argv[1:]
    os.umask(0o077)
    fd = resolve(root, card, token, mode, fallback, target)
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


if __name__ == '__main__':
    try:
        main()
    except (OSError, RuntimeError) as error:
        # No secrets or environment values in this diagnostic.
        print('Bspool error: ' + str(error), flush=True)
        sys.exit(1)
