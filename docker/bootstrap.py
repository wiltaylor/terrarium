#!/usr/bin/env python3
"""Fixed first-front bootstrap-v1 startup, not an updater or admission writer.

The host verifies OCI provenance and excludes concurrent writers. This helper
checks the container-visible bindings. It never asserts an OCI digest from
inside the image. See docs/first-front-bootstrap.md for the source ABI.
"""
import base64
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import struct
import sys

STATE = Path('/state')
BUNDLE = Path('/opt/terrarium/bundled')
CONTROL = Path('/run/terrarium-bootstrap')
METADATA = Path('/usr/local/share/terrarium/bootstrap-v1.json')
ENTRYPOINT = '/usr/local/bin/terrarium-entrypoint'
HEX = r'[0-9a-f]{64}'
ID = r'[a-z0-9][a-z0-9-]{0,63}'
BASE = {'schema', 'protocol', 'run_id', 'generation_id'}
FIELDS = {
    'descriptor': BASE | {'mode', 'manifest_sha256', 'platform_digest',
                          'payload_sha256', 'state_binding'},
    'receipt': BASE | {'descriptor_sha256', 'payload_sha256', 'release_id',
                       'state_binding', 'marker_sha256', 'phase'},
    'marker': {'schema', 'run_id', 'generation_id', 'descriptor_sha256'},
    'admission': BASE | {'descriptor_sha256', 'receipt_sha256', 'commit_sha256'},
}
ENVIRONMENT = {
    'PATH': '/opt/terrarium/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin',
    'HOME': '/home/terrarium',
    'TERRARIUM_STATE_DIR': '/state',
    'TERRARIUM_RUNTIME_DIR': '/run/terrarium',
    'TERRARIUM_BUNDLED_RELEASE': '/opt/terrarium/bundled',
    'TERRARIUM_RUNNER': '/usr/local/bin/terrarium-runner',
    'TERRARIUM_RUNNER_MODE': 'process',
    'TERRARIUM_TOOLS_DIR': '/opt/terrarium/tools',
    'NPM_CONFIG_PREFIX': '/opt/terrarium/tools',
    'TERRARIUM_CONFIG_DIR': '/etc/terrarium',
    'TERRARIUM_CONFIG_REPO': '/config/terrarium',
    'TERRARIUM_DEFAULTS_DIR': '/opt/terrarium/defaults',
    'TERRARIUM_HOST_DIST_DIR': '/state/releases/current/dist',
    'TERRARIUM_API_SOCKET': '/run/terrarium/api.sock',
    'CLAUDE_CONFIG_DIR': '/auth/claude',
    'CODEX_HOME': '/auth/codex',
    'COPILOT_HOME': '/auth/copilot',
    'PI_CODING_AGENT_DIR': '/auth/pi',
}
LINKS = {
    '/usr/local/bin/terrariumd': '/state/releases/current/bin/terrariumd',
    '/usr/local/bin/terrarium': '/state/releases/current/bin/terrarium',
    '/usr/local/bin/terrarium-runner': '/state/releases/current/bin/terrarium-runner',
    '/opt/terrarium/defaults': '/state/releases/current/defaults',
    '/etc/terrarium/harnesses.toml': '/state/releases/current/harnesses.toml',
    '/opt/terrarium/dist': '/opt/terrarium/bundled/dist',
}


def require(condition, reason):
    if not condition:
        raise RuntimeError('bootstrap-v1: ' + reason)


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'),
                      ensure_ascii=True, allow_nan=False).encode('ascii')


def identity(kind, value):
    return hashlib.sha256(('terrarium-bootstrap-' + kind + '-v1').encode()
                          + b'\0' + canonical(value)).hexdigest()


def unique(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, 'duplicate JSON key')
        result[key] = value
    return result


def decode(data):
    value = json.loads(data.decode('ascii'), object_pairs_hook=unique,
                       parse_float=lambda _: require(False, 'float'),
                       parse_constant=lambda _: require(False, 'constant'))
    require(data == canonical(value), 'noncanonical JSON')
    return value


def object_value(kind, value):
    require(isinstance(value, dict) and set(value) == FIELDS[kind], 'invalid ' + kind + ' fields')
    require(type(value['schema']) is int and value['schema'] == 1, 'schema')
    if kind != 'marker':
        require(value['protocol'] == 'bootstrap-v1', 'protocol')
    for key, item in value.items():
        if key in ('run_id', 'generation_id'):
            require(isinstance(item, str) and re.fullmatch(ID, item), key)
        elif key.endswith('_sha256') or key == 'release_id':
            require(isinstance(item, str) and re.fullmatch(HEX, item), key)
    if kind == 'descriptor':
        require(value['mode'] in ('destination', 'recovery'), 'mode')
        require(isinstance(value['platform_digest'], str)
                and re.fullmatch('sha256:' + HEX, value['platform_digest']), 'platform digest')
    if kind in ('descriptor', 'receipt'):
        binding = value['state_binding']
        require(isinstance(binding, dict)
                and set(binding) == {'device', 'inode', 'mount_binding_sha256'}, 'state binding fields')
        for key in ('device', 'inode'):
            require(isinstance(binding[key], str)
                    and re.fullmatch(r'0|[1-9][0-9]{0,19}', binding[key])
                    and int(binding[key]) <= 2**64 - 1, 'state ' + key)
        require(isinstance(binding['mount_binding_sha256'], str)
                and re.fullmatch(HEX, binding['mount_binding_sha256']), 'mount binding')
    if kind == 'receipt':
        require(value['phase'] == 'prepared', 'receipt phase')
    return value


def directory(path, owner):
    """Open every component without following symlinks, retaining the leaf fd."""
    fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in Path(path).parts[1:]:
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = child
        info = os.fstat(fd)
        require(info.st_uid == owner, 'directory owner')
        require(not info.st_mode & 0o022, 'writable directory peer')
        return fd
    except BaseException:
        os.close(fd)
        raise


def read_file(fd, name, owner, limit=16384, optional=False, private=True):
    try:
        source = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd)
    except FileNotFoundError:
        if optional:
            return None
        raise
    with os.fdopen(source, 'rb') as stream:
        info = os.fstat(stream.fileno())
        require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1
                and info.st_uid == owner, 'file type/link/owner: ' + name)
        if private:
            require(stat.S_IMODE(info.st_mode) == 0o600, 'private file mode: ' + name)
            require(not os.listxattr(stream.fileno()), 'private file xattrs: ' + name)
        data = stream.read(limit + 1)
        require(len(data) <= limit, 'oversize: ' + name)
        if private:
            os.fsync(stream.fileno())
        return data


def control_access(fd, is_directory):
    """Linux POSIX ACL ABI. Only root and this service may read/search.

    No group/other/named-group grants, no service write, no default ACL.
    Fail closed on unsupported xattrs/ACL formats, including extra security
    policy xattrs. Host must separately check bind-source ancestors.
    """
    info = os.fstat(fd)
    require(info.st_uid == 0, 'control owner')
    require((stat.S_ISDIR(info.st_mode) if is_directory else stat.S_ISREG(info.st_mode)), 'control type')
    require(is_directory or info.st_nlink == 1, 'control hardlink')
    expected = 0o700 if is_directory else 0o600
    require(stat.S_IMODE(info.st_mode) & ~0o070 == expected, 'control mode')
    attrs = os.listxattr(fd)
    require(set(attrs) <= {'system.posix_acl_access'}, 'control xattrs')
    if attrs:
        raw = os.getxattr(fd, 'system.posix_acl_access')
        require(len(raw) >= 4 and (len(raw) - 4) % 8 == 0
                and struct.unpack('<I', raw[:4])[0] == 2, 'ACL format')
        entries = [struct.unpack('<HHI', raw[i:i + 8]) for i in range(4, len(raw), 8)]
        required = 5 if is_directory else 4
        expected_entries = [(1, 7 if is_directory else 6, 0xffffffff),
                            (2, required, os.geteuid()), (4, 0, 0xffffffff),
                            (16, required, 0xffffffff), (32, 0, 0xffffffff)]
        require(entries == expected_entries, 'control effective ACL')
    else:
        # Non-root service cannot read a 0600 root file without its ACL.
        require(os.geteuid() == 0 and stat.S_IMODE(info.st_mode) == expected, 'missing service read ACL')
    require(os.fstatvfs(fd).f_flag & os.ST_RDONLY, 'control mount is not read-only')


def control(kind, optional=False):
    fd = directory(CONTROL, 0)
    try:
        control_access(fd, True)
        try:
            source = os.open(kind + '.json', os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd)
        except FileNotFoundError:
            if optional:
                return None
            raise
        with os.fdopen(source, 'rb') as stream:
            control_access(stream.fileno(), False)
            data = stream.read(16385)
            require(len(data) <= 16384, 'oversize control')
            return object_value(kind, decode(data))
    finally:
        os.close(fd)


def metadata():
    fd = directory(METADATA.parent, 0)
    try:
        data = read_file(fd, METADATA.name, 0, limit=16 * 1024 * 1024, private=False)
        info = os.stat(METADATA.name, dir_fd=fd, follow_symlinks=False)
        require(not info.st_mode & 0o022, 'writable image metadata')
        value = decode(data)
        require(set(value) == {'schema', 'protocol', 'mode', 'uid', 'gid', 'release_id',
                               'payload_sha256', 'inventory'}, 'image metadata fields')
        require(type(value['schema']) is int and value['schema'] == 1
                and value['protocol'] == 'bootstrap-v1'
                and value['mode'] in ('destination', 'recovery'), 'image protocol')
        require(type(value['uid']) is int and value['uid'] > 0
                and type(value['gid']) is int and value['gid'] > 0, 'image service identity')
        require(re.fullmatch(HEX, value['release_id']) and re.fullmatch(HEX, value['payload_sha256']), 'image hashes')
        require(payload_hash(value['inventory']) == value['payload_sha256'], 'image inventory hash')
        return value
    finally:
        os.close(fd)


def payload_hash(inventory):
    return hashlib.sha256(b'terrarium-payload-manifest-v1\0' + canonical(inventory)).hexdigest()


def inventory(root, bookkeeping=False):
    """No-follow fd walk, exact path set, no special files or hardlinks."""
    root_info = os.fstat(root)
    require(not root_info.st_mode & 0o7022, 'unsafe payload root mode')
    require(not os.listxattr(root), 'unsupported payload root xattrs')
    records = [{'path': '', 'mode': format(stat.S_IMODE(root_info.st_mode), '04o'),
                'type': 'directory'}]
    links = []
    def walk(fd, prefix):
        for name in sorted(os.listdir(fd), key=os.fsencode):
            if bookkeeping and not prefix and name == 'image.json':
                data = read_file(fd, name, os.geteuid(), private=False)
                info = os.stat(name, dir_fd=fd, follow_symlinks=False)
                require(not info.st_mode & 0o022
                        and not os.listxattr('/proc/self/fd/' + str(fd) + '/' + name,
                                             follow_symlinks=False), 'unsafe image bookkeeping')
                require(isinstance(json.loads(data), dict), 'image bookkeeping object')
                # Data only. Never selects a path, command, release or digest.
                continue
            path = prefix + os.fsencode(name)
            info = os.stat(name, dir_fd=fd, follow_symlinks=False)
            require(not os.listxattr('/proc/self/fd/' + str(fd) + '/' + name,
                                    follow_symlinks=False), 'unsupported payload xattrs')
            record = {'path': base64.b64encode(path).decode(),
                      'mode': format(stat.S_IMODE(info.st_mode), '04o')}
            if not stat.S_ISLNK(info.st_mode):
                require(not info.st_mode & 0o7022, 'unsafe payload mode')
            if stat.S_ISLNK(info.st_mode):
                target = os.fsencode(os.readlink(name, dir_fd=fd))
                require(info.st_nlink == 1, 'payload symlink hardlink')
                # Resolve separately after hashing. Absolute links are not part
                # of this bounded payload graph.
                require(target and not target.startswith(b'/'), 'absolute payload link')
                record.update(type='symlink', target=base64.b64encode(target).decode())
                links.append(path)
            elif stat.S_ISDIR(info.st_mode):
                record['type'] = 'directory'
                child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
                try:
                    require((os.fstat(child).st_dev, os.fstat(child).st_ino) == (info.st_dev, info.st_ino), 'payload directory changed')
                    walk(child, path + b'/')
                finally:
                    os.close(child)
            else:
                require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1, 'payload special file/hardlink')
                child = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd)
                with os.fdopen(child, 'rb') as stream:
                    require(os.fstat(stream.fileno()) == info, 'payload file changed')
                    digest = hashlib.sha256()
                    while True:
                        chunk = stream.read(1024 * 1024)
                        if not chunk:
                            break
                        digest.update(chunk)
                    record.update(type='file', size=info.st_size, sha256=digest.hexdigest())
            records.append(record)
    walk(root, b'')
    records.sort(key=lambda record: base64.b64decode(record['path']))
    graph = {base64.b64decode(record['path']): record for record in records}
    for link in links:
        pending = link.split(b'/')
        resolved = []
        visited = set()
        while pending:
            part = pending.pop(0)
            if part in (b'', b'.'):
                continue
            if part == b'..':
                require(resolved, 'payload link escape')
                resolved.pop()
                continue
            candidate = b'/'.join(resolved + [part])
            require(candidate in graph, 'dangling payload link')
            item = graph[candidate]
            if item['type'] == 'symlink':
                require(candidate not in visited, 'payload link loop')
                visited.add(candidate)
                pending = base64.b64decode(item['target']).split(b'/') + pending
            else:
                require(not pending or item['type'] == 'directory', 'payload link traverses file')
                resolved.append(part)
    return records


def verify_tree(parent, name, image):
    fd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
    try:
        require(inventory(fd, bookkeeping=name != BUNDLE.name) == image['inventory'], 'complete payload inventory mismatch')
        release = read_file(fd, 'release-id', os.geteuid() if name != BUNDLE.name else 0,
                            limit=65, private=False)
        require(release in (image['release_id'].encode(), (image['release_id'] + '\n').encode()), 'release ID bytes')
    finally:
        os.close(fd)


def tree_hash(parent, name):
    require(isinstance(name, str) and re.fullmatch(HEX, name), 'pre-bootstrap selection path')
    fd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
    try:
        return payload_hash(inventory(fd, bookkeeping=True))
    finally:
        os.close(fd)


def context(argv=None):
    image = metadata()
    require(os.geteuid() == image['uid'] and os.getegid() == image['gid'], 'service uid/gid (root startup unsupported)')
    if argv is not None:
        require(argv == ['--daemon-service', 'terrariumd'], 'command override')
    for key, value in ENVIRONMENT.items():
        require(os.environ.get(key, value) == value, 'path environment: ' + key)
    for key in ('XDG_RUNTIME_DIR', 'PYTHONPATH', 'PYTHONHOME', 'LD_PRELOAD', 'LD_LIBRARY_PATH',
                'BASH_ENV', 'ENV', 'TERRARIUM_USER'):
        require(key not in os.environ, 'unsupported environment: ' + key)
    require(os.environ.get('TERRARIUM_SUPERVISOR', '1') == '1', 'supervision disabled')
    for path, target in LINKS.items():
        require(os.readlink(path) == target, 'image executable/defaults link')
    verify_resolution()
    descriptor = control('descriptor')
    require(descriptor['mode'] == image['mode'] and descriptor['payload_sha256'] == image['payload_sha256'], 'image/descriptor binding')
    admission = control('admission', optional=True)
    if admission is not None:
        require(all(admission[key] == descriptor[key] for key in BASE)
                and admission['descriptor_sha256'] == identity('descriptor', descriptor), 'stale admission')
    state = directory(STATE, image['uid'])
    try:
        info = os.fstat(state)
        require(descriptor['state_binding']['device'] == str(info.st_dev)
                and descriptor['state_binding']['inode'] == str(info.st_ino), 'state device/inode binding')
    finally:
        os.close(state)
    bundle_parent = directory(BUNDLE.parent, 0)
    try:
        verify_tree(bundle_parent, BUNDLE.name, image)
    finally:
        os.close(bundle_parent)
    return image, descriptor


def payload_command(command):
    # lexists deliberately sees the image's dangling current link before
    # first init. Any earlier named entry is rejected, executable or not.
    for part in ENVIRONMENT['PATH'].split(':'):
        candidate = part + '/' + command
        if os.path.lexists(candidate):
            return candidate
    return None


def verify_resolution():
    # The canonical image PATH includes writable harness bins first. Its text
    # alone is NOT proof of which command a shell or an old reexec will use.
    for command in ('terrariumd', 'terrarium-runner'):
        require(payload_command(command) == '/usr/local/bin/' + command, 'payload command shadowing')
    for command in ('id', 'ls', 'mkdir', 'chown', 'chmod', 'setpriv', 'flock',
                    'basename', 'date', 'env', 'sleep', 'tr', 'head', 'cat', 'touch', 'rm'):
        require(shutil.which(command) in ('/usr/bin/' + command, '/bin/' + command), 'supervisor command shadowing')


def enabled(argv):
    mounted = os.path.lexists(CONTROL)
    if mounted:
        context(argv)
        if os.environ.get('TERRARIUM_BOOTSTRAP_LOCKED') == '1':
            root = releases()
            try:
                check_lock(root)
            finally:
                os.close(root)
        return True
    if os.path.lexists(METADATA):
        require(metadata()['mode'] != 'recovery', 'recovery requires controls')
    require(not os.path.lexists(STATE / 'releases/bootstrap-v1.json')
            and not os.path.lexists(STATE / 'releases/bootstrap-intent-v1.json'), 'bootstrap controls missing')
    return False


def releases(create=False):
    state = directory(STATE, os.geteuid())
    try:
        if create:
            try:
                os.mkdir('releases', 0o700, dir_fd=state)
                os.fsync(state)
            except FileExistsError:
                pass
        fd = os.open('releases', os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=state)
        if fd == 9:
            replacement = fcntl.fcntl(fd, fcntl.F_DUPFD_CLOEXEC, 10)
            os.close(fd)
            fd = replacement
        info = os.fstat(fd)
        require(info.st_uid == os.geteuid() and not info.st_mode & 0o022, 'releases owner/mode')
        return fd
    finally:
        os.close(state)


def check_lock(root):
    lock = os.fstat(9)
    named = os.stat('service.lock', dir_fd=root, follow_symlinks=False)
    require(stat.S_ISREG(lock.st_mode) and lock.st_nlink == 1 and lock.st_uid == os.geteuid()
            and not lock.st_mode & 0o022
            and (lock.st_dev, lock.st_ino) == (named.st_dev, named.st_ino), 'service.lock identity')
    fcntl.flock(9, fcntl.LOCK_EX | fcntl.LOCK_NB)


def durable(root, name, value):
    """Never overwrite an existing temporary or final object on ambiguity."""
    data = canonical(value)
    require(len(data) <= 16384, 'oversize durable object')
    fd = os.open(name + '.new', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                 0o600, dir_fd=root)
    with os.fdopen(fd, 'wb') as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())
    os.rename(name + '.new', name, src_dir_fd=root, dst_dir_fd=root)
    os.fsync(root)


def expected_marker(descriptor):
    return {key: descriptor[key] for key in ('schema', 'run_id', 'generation_id')} | {
        'descriptor_sha256': identity('descriptor', descriptor)}


def expected_receipt(image, descriptor):
    return {key: descriptor[key] for key in BASE} | {
        'descriptor_sha256': identity('descriptor', descriptor),
        'payload_sha256': image['payload_sha256'], 'release_id': image['release_id'],
        'state_binding': descriptor['state_binding'], 'phase': 'prepared',
        'marker_sha256': identity('marker', expected_marker(descriptor))}


def local(root, name, kind=None):
    data = read_file(root, name, os.geteuid(), optional=True)
    if data is None:
        return None
    value = decode(data)
    return object_value(kind, value) if kind else value


def check_intent(value, image, descriptor):
    expected = {'descriptor_sha256': identity('descriptor', descriptor),
                'release_id': image['release_id'], 'schema': 1}
    require(isinstance(value, dict) and set(value) == set(expected) | {
        'prior_release_id', 'prior_payload_sha256'}
        and type(value['schema']) is int
        and all(value[key] == item for key, item in expected.items()), 'foreign bootstrap intent')
    prior, prior_hash = value['prior_release_id'], value['prior_payload_sha256']
    require((prior is None and prior_hash is None) or (
        isinstance(prior, str) and re.fullmatch(HEX, prior)
        and isinstance(prior_hash, str) and re.fullmatch(HEX, prior_hash)), 'intent prior binding')


def validate(root, image, descriptor):
    check_lock(root)
    check_intent(local(root, 'bootstrap-intent-v1.json'), image, descriptor)
    receipt = local(root, 'bootstrap-v1.json', 'receipt')
    require(receipt == expected_receipt(image, descriptor), 'receipt mismatch or absent')
    require(os.readlink('current', dir_fd=root) == image['release_id'], 'selected payload mismatch')
    verify_tree(root, image['release_id'], image)
    marker = local(root, 'probation', 'marker')
    require(marker is None or marker == expected_marker(descriptor), 'foreign probation marker')
    admission = control('admission', optional=True)
    if admission is None:
        require(marker is not None, 'missing probation without admission')
    else:
        expected = {key: descriptor[key] for key in BASE} | {
            'descriptor_sha256': identity('descriptor', descriptor),
            'receipt_sha256': identity('receipt', receipt),
            'commit_sha256': admission['commit_sha256']}
        require(admission == expected, 'admission binding')
    os.fsync(root)


def copy_bundle(source, destination):
    """Copy through owned descriptors. Preserve bytes/modes/literal links only."""
    for name in sorted(os.listdir(source), key=os.fsencode):
        info = os.stat(name, dir_fd=source, follow_symlinks=False)
        if stat.S_ISLNK(info.st_mode):
            os.symlink(os.readlink(name, dir_fd=source), name, dir_fd=destination)
        elif stat.S_ISDIR(info.st_mode):
            os.mkdir(name, 0o700, dir_fd=destination)
            src = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=source)
            dst = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=destination)
            try:
                copy_bundle(src, dst)
                os.fchmod(dst, stat.S_IMODE(info.st_mode))
                os.fsync(dst)
            finally:
                os.close(src)
                os.close(dst)
        else:
            require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1, 'copy special file/hardlink')
            src = os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=source)
            dst = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=destination)
            with os.fdopen(src, 'rb') as incoming, os.fdopen(dst, 'wb') as outgoing:
                while True:
                    chunk = incoming.read(1024 * 1024)
                    if not chunk:
                        break
                    outgoing.write(chunk)
                outgoing.flush()
                os.fchmod(outgoing.fileno(), stat.S_IMODE(info.st_mode))
                os.fsync(outgoing.fileno())
    os.fsync(destination)


def initialize(root, image, descriptor):
    check_lock(root)
    if local(root, 'bootstrap-v1.json', 'receipt') is not None:
        validate(root, image, descriptor)
        return
    require(control('admission', optional=True) is None, 'admission before receipt')
    marker = expected_marker(descriptor)
    intent = {'descriptor_sha256': identity('descriptor', descriptor),
              'release_id': image['release_id'], 'schema': 1}
    existing = local(root, 'bootstrap-intent-v1.json')
    if existing is None:
        require(not os.path.lexists('/run/terrarium/upgrade-initialized'), 'stale initialized sentinel')
        require(local(root, 'probation', 'marker') is None, 'foreign existing probation')
        # Host already verified the cold old tree against its independently
        # extracted artifact inventory. Bind its observed content here for
        # interrupted selection, but NEVER execute or call it trusted based
        # on this local observation. Only the new image inventory may launch.
        try:
            selected = os.readlink('current', dir_fd=root)
        except FileNotFoundError:
            selected = None
        if selected is not None:
            prior_hash = tree_hash(root, selected)
            if selected == image['release_id']:
                verify_tree(root, selected, image)
        else:
            prior_hash = None
        intent.update(prior_release_id=selected, prior_payload_sha256=prior_hash)
        durable(root, 'bootstrap-intent-v1.json', intent)
    else:
        check_intent(existing, image, descriptor)
        intent = existing
    observed = local(root, 'probation', 'marker')
    if observed is None:
        durable(root, 'probation', marker)
    else:
        require(observed == marker, 'foreign probation')
    try:
        os.stat(image['release_id'], dir_fd=root, follow_symlinks=False)
    except FileNotFoundError:
        stage = 'bootstrap-' + descriptor['generation_id'] + '.staging'
        # A partial stage is retained as evidence. Reconciliation must stop,
        # not guess which writes reached stable storage or delete evidence.
        os.mkdir(stage, 0o700, dir_fd=root)
        os.fsync(root)
        dst = os.open(stage, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=root)
        src = directory(BUNDLE, 0)
        try:
            copy_bundle(src, dst)
            os.fchmod(dst, stat.S_IMODE(os.fstat(src).st_mode))
            os.fsync(dst)
        finally:
            os.close(src)
            os.close(dst)
        verify_tree(root, stage, image)
        os.rename(stage, image['release_id'], src_dir_fd=root, dst_dir_fd=root)
        os.fsync(root)
    verify_tree(root, image['release_id'], image)
    try:
        selected = os.readlink('current', dir_fd=root)
    except FileNotFoundError:
        selected = None
    require(selected in (intent['prior_release_id'], image['release_id']), 'foreign selection during init')
    if selected is not None and selected != image['release_id']:
        require(tree_hash(root, selected) == intent['prior_payload_sha256'], 'prior tree changed')
    if selected != image['release_id']:
        os.symlink(image['release_id'], 'bootstrap-current.new', dir_fd=root)
        os.rename('bootstrap-current.new', 'current', src_dir_fd=root, dst_dir_fd=root)
    os.fsync(root)
    durable(root, 'bootstrap-v1.json', expected_receipt(image, descriptor))
    validate(root, image, descriptor)


def main(args):
    require(args, 'missing internal operation')
    if args[0] == 'preflight':
        print('1' if enabled(args[1:]) else '0')
        return
    image, descriptor = context(['--daemon-service', 'terrariumd'])
    root = releases(create=args[0] == 'lock')
    try:
        if args == ['lock']:
            fd = os.open('service.lock', os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK,
                         0o600, dir_fd=root)
            os.dup2(fd, 9)
            if fd != 9:
                os.close(fd)
            os.set_inheritable(9, True)
            check_lock(root)
            os.environ['TERRARIUM_BOOTSTRAP_LOCKED'] = '1'
            os.execv(ENTRYPOINT, [ENTRYPOINT, '--daemon-service', 'terrariumd'])
        elif args == ['init']:
            initialize(root, image, descriptor)
        elif args == ['validate']:
            validate(root, image, descriptor)
        else:
            require(False, 'unknown internal operation')
    finally:
        os.close(root)


if __name__ == '__main__':
    try:
        main(sys.argv[1:])
    except (OSError, ValueError, TypeError, KeyError, RuntimeError) as error:
        print('bootstrap-v1: startup rejected (' + type(error).__name__ + ')', file=sys.stderr)
        sys.exit(1)
