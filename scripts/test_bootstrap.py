"""Bootstrap-v1 models. These do NOT prove OCI, old runtime or host isolation.

Filesystem work is private scratch only. No Docker, daemon or harness starts.
Target-only recipe after permission: pytest scripts/test_bootstrap.py --junitxml=...
"""
import contextlib
import fcntl
import importlib.util
import os
from pathlib import Path
import stat
import struct
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('bootstrap', ROOT / 'docker/bootstrap.py')
B = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(B)


@contextlib.contextmanager
def generation():
    with tempfile.TemporaryDirectory() as temp:
        base = Path(temp)
        bundle, state = base / 'bundle', base / 'state'
        bundle.mkdir(mode=0o755)
        bundle.chmod(0o755)
        state.mkdir(mode=0o700)
        root_path = state / 'releases'
        root_path.mkdir(mode=0o700)
        (bundle / 'bin').mkdir(mode=0o755)
        (bundle / 'bin/terrariumd').write_bytes(b'unchanged-old-payload')
        (bundle / 'bin/terrariumd').chmod(0o755)
        (bundle / 'runner-link').symlink_to('bin/terrariumd')
        (bundle / 'release-id').write_text('a' * 64 + '\n')
        (bundle / 'release-id').chmod(0o644)
        bundle_fd = os.open(bundle, os.O_RDONLY | os.O_DIRECTORY)
        try:
            inventory = B.inventory(bundle_fd)
        finally:
            os.close(bundle_fd)
        image = {'schema': 1, 'protocol': 'bootstrap-v1', 'mode': 'recovery',
                 'uid': os.geteuid(), 'gid': os.getegid(), 'release_id': 'a' * 64,
                 'payload_sha256': B.payload_hash(inventory), 'inventory': inventory}
        info = state.stat()
        descriptor = {'schema': 1, 'protocol': 'bootstrap-v1', 'run_id': 'run-1',
                      'generation_id': 'r0', 'mode': 'recovery', 'manifest_sha256': 'b' * 64,
                      'platform_digest': 'sha256:' + 'c' * 64,
                      'payload_sha256': image['payload_sha256'],
                      'state_binding': {'device': str(info.st_dev), 'inode': str(info.st_ino),
                                        'mount_binding_sha256': 'd' * 64}}
        root = os.open(root_path, os.O_RDONLY | os.O_DIRECTORY)
        if root == 9:
            replacement = fcntl.fcntl(root, fcntl.F_DUPFD_CLOEXEC, 10)
            os.close(root)
            root = replacement
        try:
            saved = os.dup(9)
        except OSError:
            saved = None
        lock = os.open(root_path / 'service.lock', os.O_WRONLY | os.O_CREAT, 0o600)
        os.dup2(lock, 9)
        if lock != 9:
            os.close(lock)
        fcntl.flock(9, fcntl.LOCK_EX | fcntl.LOCK_NB)
        controls = {'descriptor': descriptor, 'admission': None}
        real_directory = B.directory
        def fixture_directory(path, owner):
            # Only image ownership is abstracted for the unprivileged model.
            return real_directory(path, os.geteuid() if path == bundle else owner)
        with patch.object(B, 'BUNDLE', bundle), patch.object(B, 'STATE', state), \
                patch.object(B, 'control', side_effect=lambda kind, optional=False: controls[kind]), \
                patch.object(B, 'directory', side_effect=fixture_directory):
            try:
                yield SimpleNamespace(root=root, path=root_path, bundle=bundle, state=state,
                                      image=image, descriptor=descriptor, controls=controls)
            finally:
                os.close(root)
                if saved is None:
                    os.close(9)
                else:
                    os.dup2(saved, 9)
                    os.close(saved)


def admit(g):
    receipt = B.expected_receipt(g.image, g.descriptor)
    g.controls['admission'] = {key: g.descriptor[key] for key in B.BASE} | {
        'descriptor_sha256': B.identity('descriptor', g.descriptor),
        'receipt_sha256': B.identity('receipt', receipt), 'commit_sha256': 'e' * 64}


class BootstrapModels(unittest.TestCase):
    def test_descriptor_schema_canonical_domains_and_negative_inputs(self):
        with generation() as g:
            original = g.descriptor
            self.assertEqual(B.object_value('descriptor', B.decode(B.canonical(original))), original)
            self.assertNotEqual(B.identity('descriptor', original), B.identity('receipt', original))
            for data in (b'{}\n', b'{"schema":1,"schema":1}', b'{"x":1.0}', b'{"x":NaN}', b'\xff'):
                with self.subTest(data=data), self.assertRaises((RuntimeError, ValueError)):
                    B.decode(data)
            mutations = [dict(original, extra='no'), dict(original, schema=True),
                         dict(original, schema=2), dict(original, protocol='bootstrap-v2'),
                         dict(original, run_id='../run'), dict(original, generation_id='A'),
                         dict(original, payload_sha256='A' * 64),
                         dict(original, platform_digest='latest'), dict(original, mode='command'),
                         dict(original, state_binding={'device': '1', 'inode': '2'}),
                         dict(original, state_binding=dict(original['state_binding'], device='01'))]
            for value in mutations:
                with self.subTest(value=value), self.assertRaises(RuntimeError):
                    B.object_value('descriptor', value)

    def test_init_receipt_before_launch_and_every_spawn_tree_validation(self):
        with generation() as g:
            with self.assertRaises(RuntimeError):
                B.validate(g.root, g.image, g.descriptor)
            B.initialize(g.root, g.image, g.descriptor)
            self.assertEqual(B.local(g.root, 'probation', 'marker'), B.expected_marker(g.descriptor))
            B.validate(g.root, g.image, g.descriptor)
            payload = g.path / g.image['release_id'] / 'bin/terrariumd'
            payload.write_bytes(b'same-release-id-wrong-payload')
            for _ in range(2):
                with self.assertRaisesRegex(RuntimeError, 'inventory'):
                    B.validate(g.root, g.image, g.descriptor)
            self.assertEqual(payload.read_bytes(), b'same-release-id-wrong-payload')

    def test_restart_before_and_after_admission_no_reprobation(self):
        with generation() as g:
            B.initialize(g.root, g.image, g.descriptor)
            B.initialize(g.root, g.image, g.descriptor)
            admit(g)
            B.initialize(g.root, g.image, g.descriptor)  # admitted marker-present
            (g.path / 'probation').unlink()  # host action, never init
            for _ in range(2):
                B.initialize(g.root, g.image, g.descriptor)
                B.validate(g.root, g.image, g.descriptor)
                self.assertFalse((g.path / 'probation').exists())

    def test_admission_marker_table_invalid_and_foreign_controls(self):
        for admission in ('absent', 'matching', 'wrong-receipt', 'wrong-generation'):
            for marker in ('matching', 'absent', 'foreign', 'symlink'):
                with self.subTest(admission=admission, marker=marker), generation() as g:
                    B.initialize(g.root, g.image, g.descriptor)
                    if admission != 'absent':
                        admit(g)
                        if admission == 'wrong-receipt':
                            g.controls['admission']['receipt_sha256'] = 'f' * 64
                        if admission == 'wrong-generation':
                            g.controls['admission']['generation_id'] = 'g1'
                    if marker != 'matching':
                        (g.path / 'probation').unlink()
                        if marker == 'foreign':
                            B.durable(g.root, 'probation', dict(B.expected_marker(g.descriptor), run_id='other'))
                        elif marker == 'symlink':
                            (g.path / 'probation').symlink_to('bootstrap-v1.json')
                    permitted = admission in ('absent', 'matching') and marker == 'matching'
                    permitted |= admission == 'matching' and marker == 'absent'
                    if permitted:
                        B.validate(g.root, g.image, g.descriptor)
                    else:
                        with self.assertRaises((RuntimeError, OSError)):
                            B.validate(g.root, g.image, g.descriptor)

    def test_corrupt_receipt_and_stale_descriptor_never_repaired(self):
        for field in ('descriptor_sha256', 'payload_sha256', 'release_id', 'marker_sha256', 'generation_id', 'state_binding'):
            with self.subTest(field=field), generation() as g:
                B.initialize(g.root, g.image, g.descriptor)
                value = B.expected_receipt(g.image, g.descriptor)
                value[field] = 'wrong'
                (g.path / 'bootstrap-v1.json').write_bytes(B.canonical(value))
                with self.assertRaises((RuntimeError, ValueError)):
                    B.initialize(g.root, g.image, g.descriptor)
                self.assertEqual((g.path / 'bootstrap-v1.json').read_bytes(), B.canonical(value))

    def test_payload_extra_truncated_mode_link_and_special_entries_reject(self):
        for mutation in ('extra', 'truncate', 'mode', 'escape', 'loop', 'hardlink', 'fifo'):
            with self.subTest(mutation=mutation), generation() as g:
                B.initialize(g.root, g.image, g.descriptor)
                payload = g.path / g.image['release_id']
                if mutation == 'extra':
                    (payload / 'extra').write_bytes(b'not in inventory')
                elif mutation == 'truncate':
                    (payload / 'bin/terrariumd').write_bytes(b'')
                elif mutation == 'mode':
                    (payload / 'bin/terrariumd').chmod(0o644)
                elif mutation in ('escape', 'loop'):
                    (payload / 'runner-link').unlink()
                    (payload / 'runner-link').symlink_to('../outside' if mutation == 'escape' else 'runner-link')
                elif mutation == 'hardlink':
                    os.link(payload / 'bin/terrariumd', payload / 'second')
                else:
                    os.mkfifo(payload / 'fifo')
                with self.assertRaises((RuntimeError, OSError)):
                    B.validate(g.root, g.image, g.descriptor)

    def test_only_fixed_bookkeeping_allowlist_is_not_payload_authority(self):
        with generation() as g:
            B.initialize(g.root, g.image, g.descriptor)
            payload = g.path / g.image['release_id']
            (payload / 'image.json').write_text('{"image":"untrusted-label-not-authority"}')
            (payload / 'image.json').chmod(0o644)
            B.validate(g.root, g.image, g.descriptor)
            (payload / 'image.json.new').write_text('{}')
            with self.assertRaisesRegex(RuntimeError, 'inventory'):
                B.validate(g.root, g.image, g.descriptor)
            (payload / 'image.json.new').unlink()
            (payload / 'image.json').unlink()
            (payload / 'image.json').symlink_to('release-id')
            with self.assertRaises(OSError):
                B.validate(g.root, g.image, g.descriptor)

    def test_same_id_wrong_tree_partial_stage_and_stale_current_refuse(self):
        for mutation in ('same-id', 'stage', 'current', 'intent', 'admission'):
            with self.subTest(mutation=mutation), generation() as g:
                if mutation == 'same-id':
                    (g.path / g.image['release_id']).mkdir()
                elif mutation == 'stage':
                    (g.path / 'bootstrap-r0.staging').mkdir()
                elif mutation == 'current':
                    (g.path / 'current').symlink_to('f' * 64)
                elif mutation == 'intent':
                    B.durable(g.root, 'bootstrap-intent-v1.json', {'foreign': True})
                else:
                    admit(g)
                with self.assertRaises((RuntimeError, OSError)):
                    B.initialize(g.root, g.image, g.descriptor)
                self.assertFalse((g.path / 'bootstrap-v1.json').exists())

    def test_destination_replaces_inventoried_old_selection_never_executes_it(self):
        with generation() as g:
            old = 'b' * 64
            (g.path / old).mkdir(mode=0o755)
            (g.path / old / 'old-payload').write_bytes(b'host-verified-old-generation')
            (g.path / 'current').symlink_to(old)
            observed = B.tree_hash(g.root, old)
            B.initialize(g.root, g.image, g.descriptor)
            intent = B.local(g.root, 'bootstrap-intent-v1.json')
            self.assertEqual(intent['prior_release_id'], old)
            self.assertEqual(intent['prior_payload_sha256'], observed)
            self.assertEqual((g.path / 'current').readlink().as_posix(), g.image['release_id'])
            self.assertTrue((g.path / old / 'old-payload').exists())

    def test_private_control_files_symlink_link_mode_owner_oversize(self):
        for mutation in ('symlink', 'hardlink', 'mode', 'owner', 'oversize'):
            with self.subTest(mutation=mutation), generation() as g:
                target = g.path / 'probation'
                target.write_bytes(b'x' * (16385 if mutation == 'oversize' else 1))
                target.chmod(0o600)
                owner = os.geteuid()
                if mutation == 'symlink':
                    target.unlink()
                    target.symlink_to('service.lock')
                elif mutation == 'hardlink':
                    os.link(target, g.path / 'other')
                elif mutation == 'mode':
                    target.chmod(0o644)
                elif mutation == 'owner':
                    owner += 1
                with self.assertRaises((RuntimeError, OSError)):
                    B.read_file(g.root, 'probation', owner)

    def test_missing_controls_recovery_metadata_cannot_fall_back_to_normal_init(self):
        with generation() as g, patch.object(B, 'CONTROL', g.state / 'absent-controls'), \
                patch.object(B, 'METADATA', g.bundle / 'release-id'), \
                patch.object(B, 'metadata', return_value=g.image):
            with self.assertRaisesRegex(RuntimeError, 'requires controls'):
                B.enabled(['--daemon-service', 'terrariumd'])
            g.image['mode'] = 'destination'
            self.assertFalse(B.enabled(['--daemon-service', 'terrariumd']))
            B.durable(g.root, 'bootstrap-intent-v1.json', {'retained': True})
            with self.assertRaisesRegex(RuntimeError, 'controls missing'):
                B.enabled(['--daemon-service', 'terrariumd'])

    def test_real_service_lock_excludes_second_open_description(self):
        with generation() as g:
            B.check_lock(g.root)
            second = os.open(g.path / 'service.lock', os.O_WRONLY)
            try:
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(second, fcntl.LOCK_EX | fcntl.LOCK_NB)
            finally:
                os.close(second)
            (g.path / 'service.lock').unlink()
            (g.path / 'service.lock').touch(mode=0o600)
            with self.assertRaises(RuntimeError):
                B.check_lock(g.root)

    def test_control_acl_effective_mask_and_readonly_mount_model(self):
        uid = 1001
        entries = [(1, 7, 0xffffffff), (2, 5, uid), (4, 0, 0xffffffff),
                   (16, 5, 0xffffffff), (32, 0, 0xffffffff)]
        def acl(items):
            return struct.pack('<I', 2) + b''.join(struct.pack('<HHI', *entry) for entry in items)
        info = SimpleNamespace(st_uid=0, st_mode=stat.S_IFDIR | 0o750, st_nlink=1)
        with patch.object(B.os, 'geteuid', return_value=uid), \
                patch.object(B.os, 'fstat', return_value=info), \
                patch.object(B.os, 'fstatvfs', return_value=SimpleNamespace(f_flag=os.ST_RDONLY)), \
                patch.object(B.os, 'listxattr', return_value=['system.posix_acl_access']), \
                patch.object(B.os, 'getxattr', return_value=acl(entries)) as get:
            B.control_access(123, True)
            for items in (entries[:-1], entries + [(8, 4, 99)],
                          [entry if entry[0] != 16 else (16, 7, 0xffffffff) for entry in entries],
                          [entry if entry[0] != 2 else (2, 7, uid) for entry in entries]):
                get.return_value = acl(items)
                with self.assertRaises(RuntimeError):
                    B.control_access(123, True)
            get.return_value = acl(entries)
            info.st_uid = 10
            with self.assertRaises(RuntimeError):
                B.control_access(123, True)
            info.st_uid = 0
            with patch.object(B.os, 'fstatvfs', return_value=SimpleNamespace(f_flag=0)), self.assertRaises(RuntimeError):
                B.control_access(123, True)

    def test_stale_state_binding_image_digest_and_path_overrides(self):
        with generation() as g:
            # Context uses real state fstat, mocked signed image and links.
            image = dict(g.image, uid=1001, gid=1001)
            with patch.object(B, 'metadata', return_value=image), \
                    patch.object(B.os, 'geteuid', return_value=1001), \
                    patch.object(B.os, 'getegid', return_value=1001), \
                    patch.object(B.os, 'readlink', side_effect=lambda p: B.LINKS[p]), \
                    patch.object(B, 'verify_resolution'), \
                    patch.object(B, 'directory', return_value=g.root), \
                    patch.object(B.os, 'close'), patch.object(B, 'verify_tree'), \
                    patch.dict(os.environ, {}, clear=True):
                # g.root is releases, deliberately not descriptor's state inode.
                with self.assertRaisesRegex(RuntimeError, 'state device/inode'):
                    B.context(['--daemon-service', 'terrariumd'])
                for key in B.ENVIRONMENT:
                    with patch.dict(os.environ, {key: '/wrong'}), self.assertRaisesRegex(RuntimeError, 'path environment'):
                        B.context(['--daemon-service', 'terrariumd'])
                with self.assertRaisesRegex(RuntimeError, 'command override'):
                    B.context(['--daemon-service', 'terrariumd', '--state-dir=/other'])
                g.descriptor['payload_sha256'] = 'f' * 64
                with self.assertRaisesRegex(RuntimeError, 'image/descriptor'):
                    B.context()

    def test_canonical_path_cannot_shadow_daemon_runner_or_shell_utilities(self):
        def fixed(command):
            if command in ('terrariumd', 'terrarium-runner'):
                return '/usr/local/bin/' + command
            return '/usr/bin/' + command
        with patch.object(B.shutil, 'which', side_effect=fixed), \
                patch.object(B, 'payload_command', side_effect=fixed):
            B.verify_resolution()
        for command in ('terrariumd', 'terrarium-runner', 'env', 'mkdir', 'sleep'):
            def shadow(name):
                return '/opt/terrarium/tools/bin/' + name if name == command else fixed(name)
            with self.subTest(command=command), patch.object(B.shutil, 'which', side_effect=shadow), \
                    patch.object(B, 'payload_command', side_effect=shadow):
                with self.assertRaisesRegex(RuntimeError, 'shadowing'):
                    B.verify_resolution()

    def test_crash_each_init_fsync_rename_and_selection_boundary(self):
        # Exceptions model crash boundaries, not power-loss filesystem physics.
        real = {name: getattr(B.os, name) for name in ('fsync', 'rename', 'symlink', 'mkdir')}
        def attempt(fail=None, after=False):
            events = []
            def wrapper(name):
                def call(*args, **kwargs):
                    index = len(events)
                    events.append(name)
                    if index == fail and not after:
                        raise OSError('injected crash before boundary')
                    result = real[name](*args, **kwargs)
                    if index == fail and after:
                        raise OSError('injected crash after boundary')
                    return result
                return call
            with generation() as g:
                with contextlib.ExitStack() as stack:
                    for name in real:
                        stack.enter_context(patch.object(B.os, name, side_effect=wrapper(name)))
                    if fail is None:
                        B.initialize(g.root, g.image, g.descriptor)
                    else:
                        with self.assertRaises(OSError):
                            B.initialize(g.root, g.image, g.descriptor)
                # Never start a process. Validation alone grants no repair.
                before = sorted(os.listdir(g.root))
                try:
                    B.validate(g.root, g.image, g.descriptor)
                except (RuntimeError, OSError):
                    pass
                self.assertEqual(sorted(os.listdir(g.root)), before)
                if (g.path / 'current').is_symlink():
                    self.assertEqual((g.path / 'current').readlink().as_posix(), g.image['release_id'])
                    B.verify_tree(g.root, g.image['release_id'], g.image)
                if (g.path / 'bootstrap-v1.json').exists():
                    self.assertTrue((g.path / 'probation').is_file())
            return events
        count = len(attempt())
        self.assertGreater(count, 10)
        for index in range(count):
            for after in (False, True):
                with self.subTest(boundary=index, after=after):
                    attempt(index, after)

    def test_validation_fsync_failure_starts_nothing_and_does_not_repair(self):
        with generation() as g:
            B.initialize(g.root, g.image, g.descriptor)
            before = (g.path / 'probation').read_bytes()
            with patch.object(B.os, 'fsync', side_effect=OSError('disk failed')), self.assertRaises(OSError):
                B.validate(g.root, g.image, g.descriptor)
            self.assertEqual((g.path / 'probation').read_bytes(), before)

    def test_entrypoint_order_every_spawn_and_no_early_harness_source_contract(self):
        source = (ROOT / 'docker/entrypoint.sh').read_text()
        self.assertLess(source.index('terrarium-bootstrap.py preflight'), source.index('STATE_DIRS='))
        self.assertLess(source.index('terrarium-bootstrap.py init'), source.index('STATE_DIRS='))
        self.assertIn('[ "$bootstrap" != 1 ] && [ "${TERRARIUM_HARNESS_INSTALL:-1}"', source)
        loop = source[source.index('backoff=1\nwhile :; do'):]
        self.assertLess(loop.index('terrarium-bootstrap.py validate || exit 1'), loop.index('env --default-signal "$@"'))
        self.assertIn('mandatory init helper missing', source)
        self.assertIn('another daemon container owns this state volume', source)
        self.assertIn('!docker/bootstrap.py', (ROOT / 'docker/Dockerfile.dockerignore').read_text())
        self.assertIn('COPY docker/bootstrap.py /usr/local/lib/terrarium-bootstrap.py',
                      (ROOT / 'docker/Dockerfile').read_text())


if __name__ == '__main__':
    unittest.main()
