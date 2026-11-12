#!/usr/bin/env python3
"""Destination image build step only. Never used by runtime initialization.

Recovery assembly must independently extract the old artifact inventory and
author reviewed recovery metadata. This script does not select or build one.
"""
import os
from pathlib import Path
import pwd
import runpy


def main():
    helper = runpy.run_path('/usr/local/lib/terrarium-bootstrap.py')
    bundle = Path('/bootstrap-layer/opt/terrarium/bundled')
    fd = os.open(bundle, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        inventory = helper['inventory'](fd)
    finally:
        os.close(fd)
    user = pwd.getpwnam('terrarium')
    value = {'schema': 1, 'protocol': 'bootstrap-v1', 'mode': 'destination',
             'uid': user.pw_uid, 'gid': user.pw_gid,
             'release_id': (bundle / 'release-id').read_text().strip(),
             'payload_sha256': helper['payload_hash'](inventory), 'inventory': inventory}
    helper['require'](helper['re'].fullmatch(helper['HEX'], value['release_id']), 'build release ID')
    target = Path('/bootstrap-layer/usr/local/share/terrarium/bootstrap-v1.json')
    target.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
    with target.open('xb') as stream:
        stream.write(helper['canonical'](value))
    target.chmod(0o644)


if __name__ == '__main__':
    main()
