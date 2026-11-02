#!/usr/bin/env python3
"""Reviewed, narrow Xray routing change. Run --inspect first; requires root.
Never prints the VPN configuration or credentials. Default is read-only.
"""
import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

CONFIG = Path('/etc/dft-vpn/config.json')
XRAY = '/usr/local/lib/dft-vpn/xray'
SERVICE = 'dft-vpn.service'
PROOF = Path('/run/avito-direct-route.json')
DOMAINS = ['domain:avito.ru', 'domain:avito.st', 'domain:avito.com']
TAG = 'avito-direct-freedom'


def run(*args):
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout.strip()


def inspect(config):
    if config.get('routing', {}).get('script'):
        raise RuntimeError('Custom routing script exists; manual review required, nothing changed')
    # Domain routing is not sufficient for transparent TLS unless names are sniffed.
    transparent = [i for i in config.get('inbounds', [])
                   if i.get('protocol') == 'dokodemo-door']
    if not transparent:
        raise RuntimeError('No recognized transparent inbound; manual review required')
    for inbound in transparent:
        sniff = inbound.get('sniffing', {})
        if not sniff.get('enabled') or not {'http', 'tls', 'quic'}.issubset(set(sniff.get('destOverride', []))):
            raise RuntimeError('Transparent inbound needs existing HTTP/TLS/QUIC sniffing; nothing changed')
    return transparent


def expected(config):
    inspect(config)
    outbound = next((o for o in config.get('outbounds', []) if o.get('tag') == TAG), None)
    if outbound != {'tag': TAG, 'protocol': 'freedom', 'settings': {}}:
        raise RuntimeError('Dedicated direct outbound is missing or altered')
    rule = config.get('routing', {}).get('rules', [None])[0]
    if rule != {'type': 'field', 'domain': DOMAINS, 'outboundTag': TAG}:
        raise RuntimeError('First routing rule does not match the reviewed Avito-only rule')


def atomic_write(path, content, mode, uid=0, gid=0):
    fd, name = tempfile.mkstemp(prefix='.avito-direct-', dir=path.parent)
    try:
        os.fchmod(fd, mode)
        os.fchown(fd, uid, gid)
        with os.fdopen(fd, 'wb') as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, path)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def attest():
    config = json.loads(CONFIG.read_text())
    expected(config)
    state = run('systemctl', 'show', SERVICE, '-p', 'ActiveState', '--value')
    if state not in ('active', 'activating'):
        raise RuntimeError('VPN service is not running')
    invocation = run('systemctl', 'show', SERVICE, '-p', 'InvocationID', '--value')
    pid = int(run('systemctl', 'show', SERVICE, '-p', 'MainPID', '--value'))
    if not invocation or pid <= 0:
        raise RuntimeError('Running VPN invocation not found')
    proof = {'kind': 'xray-avito-freedom-v1', 'invocation_id': invocation,
             'pid': pid, 'domains': DOMAINS, 'created': int(time.time()),
             'config_sha256': hashlib.sha256(CONFIG.read_bytes()).hexdigest()}
    atomic_write(PROOF, json.dumps(proof).encode(), 0o644)
    print('Verified: Avito domains use dedicated freedom outbound; other routing unchanged.')


def prepare(config):
    inspect(config)
    updated = copy.deepcopy(config)
    old = next((o for o in updated.get('outbounds', []) if o.get('tag') == TAG), None)
    direct = {'tag': TAG, 'protocol': 'freedom', 'settings': {}}
    if old is not None and old != direct:
        raise RuntimeError('Outbound tag already exists with different settings; nothing changed')
    if old is None:
        updated.setdefault('outbounds', []).append(direct)
    rules = updated.setdefault('routing', {}).setdefault('rules', [])
    rule = {'type': 'field', 'domain': DOMAINS, 'outboundTag': TAG}
    updated['routing']['rules'] = [rule] + [r for r in rules if r != rule]
    expected(updated)
    return updated


def apply():
    original = CONFIG.read_bytes()
    metadata = CONFIG.stat()
    config = json.loads(original)
    updated = prepare(config)
    payload = (json.dumps(updated, ensure_ascii=False, indent=2) + '\n').encode()
    # Validate before replacing the active config. Never echo xray errors: they may contain secrets.
    fd, candidate = tempfile.mkstemp(prefix='.avito-validate-', suffix='.json', dir=CONFIG.parent)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, 'wb') as stream:
            stream.write(payload)
        result = subprocess.run([XRAY, 'run', '-test', '-config', candidate], capture_output=True)
        if result.returncode:
            raise RuntimeError('Xray validation failed; active config unchanged')
    finally:
        os.unlink(candidate)
    backup = CONFIG.with_name('config.before-avito-' + str(time.time_ns()) + '.json')
    atomic_write(backup, original, 0o600)
    helper = Path('/usr/local/sbin/avito-direct-config')
    atomic_write(helper, Path(__file__).read_bytes(), 0o755)
    dropin = Path('/etc/systemd/system/dft-vpn.service.d/avito-direct.conf')
    dropin.parent.mkdir(parents=True, exist_ok=True)
    if dropin.exists() and dropin.read_text() != '[Service]\nExecStartPost=+/usr/local/sbin/avito-direct-config --verify\n':
        raise RuntimeError('Different service drop-in exists; active config unchanged')
    atomic_write(dropin, b'[Service]\nExecStartPost=+/usr/local/sbin/avito-direct-config --verify\n', 0o644)
    atomic_write(CONFIG, payload, metadata.st_mode & 0o777, metadata.st_uid, metadata.st_gid)
    try:
        run('systemctl', 'daemon-reload')
        run('systemctl', 'restart', SERVICE)
        attest()
    except Exception:
        atomic_write(CONFIG, original, metadata.st_mode & 0o777, metadata.st_uid, metadata.st_gid)
        dropin.unlink(missing_ok=True)
        PROOF.unlink(missing_ok=True)
        subprocess.run(['systemctl', 'daemon-reload'], capture_output=True)
        subprocess.run(['systemctl', 'restart', SERVICE], capture_output=True)
        raise RuntimeError('Apply failed; original config restored, check VPN service locally') from None
    print('Backup saved privately:', backup)
    print('Configuration changed; existing browser connections may need to reconnect.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group()
    group.add_argument('--apply', action='store_true')
    group.add_argument('--verify', action='store_true')
    parser.add_argument('--inspect', action='store_true')
    args = parser.parse_args()
    if os.geteuid() != 0:
        raise RuntimeError('Run this reviewed script with sudo in your own terminal; do not share passwords')
    if args.apply:
        apply()
    elif args.verify:
        attest()
    else:
        config = json.loads(CONFIG.read_text())
        inbounds = inspect(config)
        print('Recognized transparent inbounds:', len(inbounds))
        print('Plan: prepend Avito-only domain rule and add dedicated freedom outbound.')
        print('Domains:', ', '.join(DOMAINS))
        print('No configuration changed. Use --apply to apply after review.')


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        # Only static errors from our checks; do not print provider errors/configs.
        if isinstance(error, (RuntimeError, PermissionError, FileNotFoundError)):
            print(str(error), file=sys.stderr)
        else:
            print('Operation failed:', type(error).__name__, file=sys.stderr)
        sys.exit(1)
