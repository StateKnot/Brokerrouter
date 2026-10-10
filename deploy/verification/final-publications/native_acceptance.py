#!/usr/bin/env python3
"""Separate full native Hermes, real HTTPS, workbench and restart acceptance."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import subprocess
import tempfile
import time
import uuid

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('final_fixture', HERE / 'acceptance.py')
final = importlib.util.module_from_spec(spec)
spec.loader.exec_module(final)
require = final.require
HERMES = 'agentos-mcp-runtime-contract@sha256:943ff581ef4016d9385cbf91b85be7dc5f7176e02091f359fd097432aedfa30f'


class NativeFixture(final.Fixture):
    def __init__(self, args, root, agent_root):
        super().__init__(args.image, root)
        self.args = args
        self.agent_root = agent_root
        self.agent_root.mkdir(mode=0o700)
        self.running = set()
        self.native_logs = []

    def ca(self, name):
        # Python3.13's default VERIFY_X509_STRICT requires proper CA key usage.
        self.openssl('req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '14',
                     '-subj', '/CN=Task-only ' + name + ' CA',
                     '-addext', 'basicConstraints=critical,CA:TRUE',
                     '-addext', 'keyUsage=critical,keyCertSign,cRLSign',
                     '-addext', 'subjectKeyIdentifier=hash',
                     '-keyout', self.root / (name + '.key'), '-out', self.root / (name + '.crt'))
        (self.root / (name + '.key')).chmod(0o600)
        (self.root / (name + '.crt')).chmod(0o444)

    def leaf(self, name, ca, hosts):
        self.openssl('req', '-new', '-newkey', 'rsa:2048', '-nodes', '-subj', '/CN=' + hosts[0],
                     '-keyout', self.root / (name + '.key'), '-out', self.root / (name + '.csr'))
        self.write(name + '.ext', 'subjectAltName=' + ','.join('DNS:' + value for value in hosts)
                   + '\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\n'
                   + 'extendedKeyUsage=serverAuth\nsubjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid,issuer\n')
        self.openssl('x509', '-req', '-days', '14', '-in', self.root / (name + '.csr'),
                     '-CA', self.root / (ca + '.crt'), '-CAkey', self.root / (ca + '.key'), '-CAcreateserial',
                     '-extfile', self.root / (name + '.ext'), '-out', self.root / (name + '.crt'))
        (self.root / (name + '.key')).chmod(0o600)
        (self.root / (name + '.crt')).chmod(0o444)

    def configuration(self):
        super().configuration()
        config = json.loads((self.root / 'compose.json').read_text())
        edge = config['services']['runtime-tls']
        edge.update(image=final.identity.BROWSER,
                    entrypoint=['node', '/fixture/native_ack_edge.js'],
                    volumes=[str(HERE / 'native_ack_edge.js') + ':/fixture/native_ack_edge.js:ro'],
                    cap_add=['SETUID', 'SETGID'])
        config['services']['provider']['volumes'] = [str(HERE / 'native_provider.js') + ':/fixture/provider.js:ro']
        self.write('compose.json', json.dumps(config, indent=2))

    def start_database(self):
        # This new acceptance has an explicit image/schema pair. The historical
        # sixteen-group schema31 acceptance stays immutable and is not widened.
        self.started = True
        self.compose('config', '--quiet')
        self.compose('up', '--detach', '--wait', '--wait-timeout', '60', 'postgres', timeout=90)
        self.cli(kind='migration')
        self.admin((final.SOURCE / 'deploy/permissions.sql').read_text())
        self.admin('REVOKE ALL ON DATABASE brokerrouter FROM PUBLIC; GRANT CONNECT ON DATABASE brokerrouter TO brokerrouter_migration;')
        for name in ['runtime', 'operator']:
            self.admin("CREATE ROLE brokerrouter_" + name + "_login LOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD '" + final.purpose.scram(self.passwords[name].encode()) + "'; GRANT brokerrouter_" + name + " TO brokerrouter_" + name + "_login; GRANT CONNECT ON DATABASE brokerrouter TO brokerrouter_" + name + "_login;")
        owner = self.call(['docker', 'run', '--rm', '--network', 'none', '--mount', 'type=bind,src=' + str(self.root / 'purpose-logins') + ',dst=/purpose,readonly', '--entrypoint', 'stat', final.identity.PG, '-c', '%u:%g', '/purpose']).decode().strip()
        uid, gid = owner.split(':')
        self.call(['docker', 'run', '--rm', '--network', self.project + '_backend', '--user', owner, '--read-only', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges:true', '--tmpfs', '/run/purpose-secrets:size=1m,mode=0700,uid=' + uid + ',gid=' + gid, '--tmpfs', '/tmp:size=8m,mode=1777', '--mount', 'type=bind,src=' + str(self.root / 'purpose-logins') + ',dst=/run/purpose-secrets/purpose-logins,readonly', '--mount', 'type=bind,src=' + str(self.root / '.purpose-logins.lock') + ',dst=/run/purpose-secrets/.purpose-logins.lock', '--mount', 'type=bind,src=' + str(self.root / 'purpose-admin.service') + ',dst=/run/secrets/purpose_admin_service,readonly', '--mount', 'type=bind,src=' + str(self.root / 'purpose-admin.passfile') + ',dst=/run/secrets/purpose_admin_passfile,readonly', '--mount', 'type=bind,src=' + str(self.root / 'pg-ca.crt') + ',dst=/run/secrets/pg_ca,readonly', final.OPS], timeout=60)
        require(int(self.admin('SELECT count(*) FROM brokerrouter_deployment.purpose_logins;')) == 20, 'all twenty canonical purpose LOGIN registrations')
        versions = self.admin('SELECT version FROM _sqlx_migrations WHERE success ORDER BY version;').decode().split()
        require(versions == [str(n) for n in range(1, self.args.schema_version + 1)], 'actual database exactly matches explicit compiled schema')
        self.passed('actual release migration' + str(self.args.schema_version) + ' and matching canonical grants; twenty isolated purpose LOGINs; measured Secret owner and strict TLS/SCRAM')

    def native_command(self, phase):
        name = self.project + '-native-' + phase + '-' + secrets.token_hex(3)
        command = ['docker', 'run', '--rm', '--name', name,
                   '--label', 'com.docker.compose.project=' + self.project,
                   '--network', 'none' if phase == 'prepare-session' else self.project + '_backend',
                   '--user', str(os.geteuid()) + ':' + str(os.getegid()),
                   '--read-only', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges:true',
                   '--memory', '768m', '--cpus', '1', '--pids-limit', '128',
                   '--tmpfs', '/tmp:size=32m,mode=1777',
                   '--tmpfs', '/opt/data:size=1m,mode=0700,uid=' + str(os.geteuid()) + ',gid=' + str(os.getegid()),
                   '--mount', 'type=bind,src=' + str(self.agent_root) + ',dst=/agent-state',
                   '--mount', 'type=bind,src=' + str(self.args.host_source) + ',dst=/host-source,readonly',
                   '--mount', 'type=bind,src=' + str(self.args.stage_script) + ',dst=/fixture/native_stage.py,readonly']
        if phase != 'prepare-session':
            command += ['--mount', 'type=bind,src=' + str(self.agent_root / 'runtime-config.json') + ',dst=/runtime-config.json,readonly',
                        '--mount', 'type=bind,src=' + str(self.root / 'runtime-ca.crt') + ',dst=/run/runtime-ca.crt,readonly']
        command += ['--entrypoint', '/opt/hermes/.venv/bin/python', self.args.hermes_image,
                    '/fixture/native_stage.py', phase, '--state-dir', '/agent-state',
                    '--host-source', '/host-source', '--observation', '/agent-state/observation.json']
        if phase != 'prepare-session':
            command += ['--config', '/runtime-config.json']
        return name, command

    def native(self, phase, asynchronous=False):
        observation = self.agent_root / 'observation.json'
        observation.unlink(missing_ok=True)
        name, command = self.native_command(phase)
        if asynchronous:
            process = subprocess.Popen([str(item) for item in command], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            self.running.add(name)
            return name, process
        process = subprocess.run([str(item) for item in command], stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=90)
        self.native_logs += [process.stdout, process.stderr]
        require(process.returncode == 0, 'actual native process failed; sanitized stderr saved in evidence')
        require(observation.is_file(), 'native process writes safe atomic observation')
        return json.loads(observation.read_text())

    def seed(self):
        initial = self.native('prepare-session')
        self.session = initial['session_id']
        require(isinstance(self.session, str) and 1 <= len(self.session) <= 256, 'actual native session ID is bounded text')
        self.message = 'synthetic-native-final-message'
        self.run_id = 'synthetic-native-final-run'
        self.passed('fixed complete Hermes creates a real native SessionStore routing identity with network disabled')
        self.tenant = self.cli('tenant-create', '--name', 'Synthetic native final acceptance', '--budget-cny', '10').decode().strip()
        self.cli('model-put', '--tenant', self.tenant, '--id', 'fixture', '--name', 'Zero-cost actual primary SDK model')
        self.cli('endpoint-add', '--tenant', self.tenant, '--model', 'fixture', '--name', 'Native local fixture only', '--upstream-model', 'local-upstream', '--base-url', 'http://provider:8099/v1', '--credential-env', 'FIXTURE_MODEL_KEY', '--input-per-million', '0', '--output-per-million', '0', '--context-tokens', '32768', '--max-output-tokens', '1024', '--supports-tools', '--supports-streaming', '--allow-private')
        key = json.loads(self.cli('key-create', '--tenant', self.tenant, '--name', 'Native Agent virtual key only', '--budget-cny', '1', '--model', 'fixture'))
        self.key, self.keyid = key['key'], key['id']
        self.sensitive.append(self.key.encode())
        self.owner = self.cli('governance-user-add', '--tenant', self.tenant, '--issuer', final.ISSUER, '--subject', 'synthetic-owner').decode().strip()
        self.cli('runtime-bind', '--tenant', self.tenant, '--key', self.keyid, '--user', self.owner, '--runtime', 'final-text-fixture', '--agent', 'synthetic-native-agent')
        self.compose('up', '--detach', 'provider', 'agent')
        self.compose('up', '--detach', '--wait', '--wait-timeout', '60', 'gateway', 'control', 'approvals', timeout=90)
        self.compose('up', '--detach', 'runtime-tls', 'control-tls', 'approvals-tls')
        for purpose in ['runtime', 'control', 'approvals']:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                try:
                    if self.http('https://' + purpose + '.brokerrouter.internal:8443/health/ready', ca=purpose + '-ca.crt')['status'] == 204:
                        break
                except RuntimeError:
                    pass
                time.sleep(0.5)
            else:
                raise RuntimeError('actual HTTPS readiness deadline: ' + purpose)
        response = self.human_http('/v1/governance/turns', {'key_id': self.keyid, 'session_id': self.session,
            'message_id': self.message, 'run_id': self.run_id, 'generation': 1, 'ttl_seconds': 1800})
        require(response['status'] == 200, 'controller creates a turn bound to the actual native SessionStore identity')
        self.turn = json.loads(response['body'])['id']
        state_key = secrets.token_hex(32)
        self.sensitive += [state_key.encode(), b'zero fee native final publication acceptance']
        config = {'runtime_base_url': 'https://runtime.brokerrouter.internal:8443', 'runtime_ca_path': '/run/runtime-ca.crt',
            'virtual_key': self.key, 'approvals_url': 'https://synthetic-workbench.fixture.invalid', 'state_key_hex': state_key,
            'turn_id': self.turn, 'model_id': 'fixture', 'session_id': self.session,
            'message_id': self.message, 'run_id': self.run_id, 'generation': 1}
        path = self.agent_root / 'runtime-config.json'
        path.write_text(json.dumps(config))
        path.chmod(0o600)
        self.passed('controller creates exact native session/message/run/generation turn through separate HUMAN HTTPS; runtime config has only Agent-purpose credentials')

    def inspect_native(self, name):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try:
                raw = json.loads(self.call(['docker', 'inspect', name]))[0]
                break
            except RuntimeError:
                time.sleep(0.1)
        else:
            raise RuntimeError('native container did not start')
        destinations = {mount['Destination'] for mount in raw['Mounts']}
        require(destinations == {'/agent-state', '/host-source', '/fixture/native_stage.py', '/runtime-config.json', '/run/runtime-ca.crt'}, 'native Agent has only runtime trust/config, own state and public fixed source mounts')
        require(not any('DATABASE_URL=' in item or 'BROKERROUTER_HUMAN' in item or 'BROKERROUTER_BFF' in item for item in raw['Config']['Env']), 'native Agent image environment has no privileged identity')
        require(raw['Config']['User'] == str(os.geteuid()) + ':' + str(os.getegid()) and os.geteuid() != 0, 'actual native Agent runs as non-root')
        require(raw['HostConfig']['ReadonlyRootfs'] and raw['HostConfig']['CapDrop'] == ['ALL'], 'actual native Agent read-only root with all capabilities dropped')
        self.passed('actual complete native Agent is non-root/read-only/no-capabilities; no HUMAN/BFF/DB/admin keys or Docker socket')

    def approve_native(self, observation):
        approval = str(uuid.UUID(observation['approval_id']))
        inbox = self.human_http('/approvals/v1/query', {'limit': 20, 'pending_only': True, 'turn_id': self.turn}, key='approvals')
        require(inbox['status'] == 200 and any(row['id'] == approval and row['stage'] == 'post' for row in json.loads(inbox['body'])['data']), 'actual owner workbench contains native final pending post approval')
        response = self.human_http('/approvals/v1/review', {'approval_id': approval}, key='approvals')
        require(response['status'] == 200, 'actual owner protected full-text review succeeds')
        review = json.loads(response['body'])
        require(review['can_continue'] is True and review['stage'] == 'post', 'server verified final source is reviewable')
        payload = json.loads(review['payload_text'])
        self.sensitive.append(payload['text'].encode())
        require(hashlib.sha256(payload['text'].encode()).hexdigest() == self.ack['text_sha256'] and len(payload['text'].encode()) == self.ack['text_bytes'], 'complete review exactly matches committed immutable candidate text hash and UTF-8 bytes')
        require(payload['publication_id'] == self.ack['publication_id'] and payload['session_id'] == self.session
                and payload['message_id'] == self.message and payload['run_id'] == self.run_id
                and payload['generation'] == 1 and len(payload['source_request_ids']) == 1, 'review binds real source request and actual native owner tuple')
        self.request = str(uuid.UUID(payload['source_request_ids'][0]))
        decision = {'approval_id': approval, 'review_id': review['review_id'], 'acknowledged_scope': True,
            'decision': {'decision': 'continue_this_turn', 'approved_guardrail_ids': ['final-text-review'], 'feedback': None}}
        decided = self.human_http('/approvals/v1/decide', decision, key='approvals')
        require(decided['status'] == 200, 'real acknowledged native owner decision')
        self.passed('actual owner workbench query→complete exact-text review→explicit acknowledged decision; original native tuple and source UUID verified')
        return approval

    def verify(self):
        name, process = self.native('initial-run', asynchronous=True)
        try:
            self.inspect_native(name)
            stdout, stderr = process.communicate(timeout=90)
            self.native_logs += [stdout, stderr]
            require(process.returncode == 0, 'actual native initial process completes safely')
        finally:
            if process.poll() is None:
                subprocess.run(['docker', 'rm', '--force', name], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                stdout, stderr = process.communicate(timeout=15)
                self.native_logs += [stdout, stderr]
            self.running.discard(name)
        initial = json.loads((self.agent_root / 'observation.json').read_text())
        require(initial['state'] == 'held' and initial['code'] == 'final_record_not_persisted'
                and initial['ordinary_rows'] == 0 and initial['platform_calls'] == 0
                and re.fullmatch('[0-9a-f]{64}', initial.get('candidate_text_sha256', '')), 'record transport failure leaves frozen private candidate, zero ordinary history and no body send')
        self.ack = json.loads(self.call(['docker', 'exec', '--user', '10001:10001', self.container('runtime-tls'), 'cat', '/tmp/committed-record-ack-loss.json']))
        require(self.ack['dropped'] == 1 and self.ack['status'] == 200 and self.ack['ack_body_forwarded'] is False
                and self.ack['text_sha256'] == initial['candidate_text_sha256'], 'edge received real committed record response then destroyed client socket without forwarding ACK')
        require(self.stats()['calls'] == 1 and self.stats()['streaming_calls'] == 1, 'actual primary SDK called real streaming HTTP provider exactly once')
        wire = json.loads(self.call(['docker', 'exec', '--user', '10001:10001', self.container('runtime-tls'), 'cat', '/tmp/native-wire-field-profile.json']))
        require(wire['stream'] is True and wire['stream_options_types'] == {'include_usage': 'boolean'}
                and wire['stream_options_include_usage'] is True, 'actual SDK wire preserves the explicit streaming usage contract')
        (final.EVIDENCE / 'native-wire-field-profile.json').write_text(json.dumps(wire, indent=2) + '\n')
        self.passed('actual primary SDK→one real model source→committed record ACK socket loss; native candidate frozen, ordinary history0 and body sends0')
        self.policy(1)
        name, process = self.native('resume', asynchronous=True)
        try:
            deadline = time.monotonic() + 45
            while time.monotonic() < deadline and process.poll() is None:
                path = self.agent_root / 'observation.json'
                if path.exists():
                    pending = json.loads(path.read_text())
                    if pending.get('state') == 'waiting_approval':
                        break
                time.sleep(0.1)
            else:
                raise AssertionError('fresh native recovery must expose real pending approval metadata')
            denied = self.http('https://approvals.brokerrouter.internal:8443/approvals/v1/query', {'limit': 20},
                {'Content-Type': 'application/json', 'Authorization': 'Bearer ' + self.key}, ca='approvals-ca.crt')
            require(denied['status'] == 401, 'Agent virtual key cannot approve its own native final')
            self.approval = self.approve_native(pending)
            stdout, stderr = process.communicate(timeout=45)
            self.native_logs += [stdout, stderr]
            require(process.returncode == 0, 'actual native resumed process completes')
        finally:
            if process.poll() is None:
                subprocess.run(['docker', 'rm', '--force', name], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                stdout, stderr = process.communicate(timeout=15)
                self.native_logs += [stdout, stderr]
            self.running.discard(name)
        delivered = json.loads((self.agent_root / 'observation.json').read_text())
        require(delivered['state'] == 'delivered' and delivered['ordinary_rows'] == 2
                and delivered['ordinary_assistant_rows'] == 1 and delivered['platform_calls'] == 1
                and delivered['text_sha256'] == self.ack['text_sha256'], 'only approved exact native user/final history and one actual sender acceptance')
        accepted = json.loads((self.agent_root / 'platform-accepted.json').read_text())
        require(accepted == [self.ack['text_sha256']] and self.stats()['calls'] == 1, 'real isolated platform accepted once; no repeated model')
        self.passed('fresh native process recovers same private candidate; only current approved exact text enters two ordinary rows and actual Base sender once')
        replay = self.native('resume')
        require(replay['ordinary_rows'] == 2 and replay['platform_calls'] == 0
                and json.loads((self.agent_root / 'platform-accepted.json').read_text()) == accepted
                and self.stats()['calls'] == 1, 'another fresh native process uses real history cursor and sender ACK without repeating model/history/platform effect')
        self.passed('another complete native process restart preserves exact history cursor and accepted sender effect; zero new model calls, rows or sends')
        counts = self.admin("SELECT (SELECT count(*) FROM requests),(SELECT count(*) FROM runtime_final_publications),(SELECT count(*) FROM approvals),(SELECT count(*) FROM guardrail_grants),(SELECT count(*) FROM ledger_entries WHERE request_id='" + self.request + "' AND kind='finalize');").decode().strip()
        require(counts == '1|1|1|1|1', 'actual native backend one source/publication/approval/grant/final settlement')
        self.passed('actual PostgreSQL source/publication/approval/grant/final settlement are each1; external paid provider cost0')
        for data in self.native_logs:
            for value in self.sensitive:
                require(value not in data, 'native process leaked a generated secret or synthetic request/final body')
        (final.EVIDENCE / 'native-processes.log').write_bytes(b''.join(self.native_logs))
        self.logs_safe()
        return {'initial': initial, 'delivered': delivered, 'replayed': replay, 'ack_loss': self.ack,
                'provider_calls': 1, 'provider_streaming_calls': 1, 'ordinary_rows': 2, 'accepted_platform_sends': 1,
                'counts': dict(zip(['requests', 'publications', 'approvals', 'grants', 'finalize_ledger_entries'], [1]*5))}

    def close(self):
        for name in list(self.running):
            subprocess.run(['docker', 'rm', '--force', name], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        super().close()
        shutil.rmtree(self.agent_root)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', required=True)
    parser.add_argument('--schema-version', type=int, required=True)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--host-source', type=Path, required=True)
    parser.add_argument('--stage-script', type=Path, required=True)
    parser.add_argument('--host-manifest', type=Path, required=True)
    parser.add_argument('--installer-source', type=Path, default=final.REPO / 'adapters/hermes')
    parser.add_argument('--core-manifest', type=Path, required=True)
    parser.add_argument('--deployment-manifest', type=Path, required=True)
    parser.add_argument('--evidence-dir', type=Path, required=True)
    parser.add_argument('--hermes-image', default=HERMES)
    parser.add_argument('--provisioner-image', default=final.OPS)
    args = parser.parse_args()
    os.umask(0o077)
    for name in ['source', 'host_source', 'stage_script', 'host_manifest', 'installer_source', 'core_manifest', 'deployment_manifest', 'evidence_dir']:
        setattr(args, name, getattr(args, name).resolve())
    require(not args.evidence_dir.is_relative_to(final.REPO), 'evidence outside repository')
    args.evidence_dir.mkdir(parents=True, exist_ok=True)
    require(not any(args.evidence_dir.iterdir()), 'fresh native evidence directory required')
    for image in [args.image, args.hermes_image, args.provisioner_image]:
        require(re.fullmatch(r'(?:[A-Za-z0-9._/:-]+@)?sha256:[0-9a-f]{64}', image), 'immutable image required')
    require(os.geteuid() != 0, 'native runner must be non-root')
    require(31 <= args.schema_version <= 9999, 'explicit supported publication schema version required')
    migrations = list((args.source / 'migrations').glob('*.sql'))
    require(sorted(int(path.name.split('_', 1)[0]) for path in migrations) == list(range(1, args.schema_version + 1)), 'matching exact frozen migration sequence required')
    host_manifest = json.loads(args.host_manifest.read_text())
    for name, expected in host_manifest['host_files'].items():
        require(hashlib.sha256((args.host_source / name).read_bytes()).hexdigest() == expected['patched_sha256'], 'native host differs from strict installation manifest: ' + name)
    for name, expected in host_manifest['adapter_files'].items():
        path = args.installer_source / name if name == 'install.py' else args.host_source / name
        require(hashlib.sha256(path.read_bytes()).hexdigest() == expected, 'native adapter/installer differs from strict installation manifest: ' + name)
    core_manifest = json.loads(args.core_manifest.read_text())
    require(core_manifest['image_id'] == args.image, 'core inputs belong to another image')
    require(core_manifest.get('schema_version', args.schema_version) == args.schema_version, 'core manifest belongs to another schema')
    deployment_manifest = json.loads(args.deployment_manifest.read_text())
    required_deployment_inputs = [
        'deploy/permissions.sql',
        'deploy/production/purpose-logins.py',
        'deploy/production/create-purpose-secrets.sh',
        'deploy/production/prepare-purpose-admin.py',
        'deploy/production/pg_hba.conf',
        'deploy/production/control-plane-nginx.conf',
    ]
    for name in required_deployment_inputs:
        require(name in deployment_manifest['files'] and (args.source / name).is_file(),
                'complete frozen deployment input required before creating Secrets: ' + name)
    for name, expected in deployment_manifest['files'].items():
        path = Path(name)
        require(not path.is_absolute() and '..' not in path.parts, 'unsafe frozen deployment input path')
        require(hashlib.sha256((args.source / path).read_bytes()).hexdigest() == expected, 'deployment source differs from frozen inputs: ' + name)
    for path in (args.source / 'migrations').glob('*.sql'):
        name = str(path.relative_to(args.source))
        require(core_manifest['files'][name] == hashlib.sha256(path.read_bytes()).hexdigest(), 'migration differs from actual compiled image: ' + name)
    host_hashes_before = {str(path.relative_to(args.host_source)): hashlib.sha256(path.read_bytes()).hexdigest()
                          for path in sorted(args.host_source.rglob('*.py'))}
    (args.evidence_dir / 'host-inputs.json').write_bytes(args.host_manifest.read_bytes())
    (args.evidence_dir / 'core-inputs.json').write_bytes(args.core_manifest.read_bytes())
    (args.evidence_dir / 'deployment-inputs.json').write_bytes(args.deployment_manifest.read_bytes())
    final.SOURCE, final.EVIDENCE, final.OPS = args.source, args.evidence_dir, args.provisioner_image
    final.PROJECT = 'brokerrouter-native-final-test-' + secrets.token_hex(6)
    final.identity.PROJECT = final.PROJECT
    module = importlib.util.spec_from_file_location('native_purpose', args.source / 'deploy/production/purpose-logins.py')
    final.purpose = importlib.util.module_from_spec(module)
    module.loader.exec_module(final.purpose)
    roots = [Path(tempfile.mkdtemp(prefix='brokerrouter-native-final-' + purpose + '-', dir='/private/tmp')).resolve() for purpose in ['secrets', 'state']]
    for root in roots:
        root.rmdir()
    fixture = NativeFixture(args, *roots)
    started = time.monotonic()
    result = None
    try:
        fixture.setup()
        fixture.start_database()
        fixture.seed()
        result = fixture.verify()
    except Exception as error:
        print('FAIL: ' + type(error).__name__ + ': ' + str(error), flush=True)
        if fixture.started:
            logs = fixture.compose('logs', '--no-color', timeout=30)
            for value in fixture.sensitive:
                logs = logs.replace(value, b'[REDACTED]')
            (args.evidence_dir / 'native-failure-services.log').write_bytes(logs)
            try:
                metrics = {'provider': fixture.stats(), 'postgres': fixture.admin('SELECT (SELECT count(*) FROM requests),(SELECT count(*) FROM runtime_final_publications);').decode().strip()}
                (args.evidence_dir / 'native-failure-counts.json').write_text(json.dumps(metrics, indent=2))
            except RuntimeError:
                pass
            try:
                profile = fixture.call(['docker', 'exec', '--user', '10001:10001', fixture.container('runtime-tls'), 'cat', '/tmp/native-wire-field-profile.json'])
                (args.evidence_dir / 'native-failure-wire-field-profile.json').write_bytes(profile)
            except RuntimeError:
                pass
        native_logs = b''.join(fixture.native_logs)
        for value in fixture.sensitive:
            native_logs = native_logs.replace(value, b'[REDACTED]')
        (args.evidence_dir / 'native-failure-processes.log').write_bytes(native_logs)
        observation = fixture.agent_root / 'observation.json'
        if observation.is_file():
            raw = json.loads(observation.read_text())
            allowed = {'session_id', 'session_key', 'phase', 'state', 'code', 'ordinary_rows', 'ordinary_assistant_rows', 'platform_calls', 'publication_id', 'output_operation_id', 'candidate_text_sha256', 'candidate_bytes', 'source_request_ids', 'approval_id', 'native_exception_type', 'native_exception_frames', 'native_exception_causes'}
            (args.evidence_dir / 'native-failure-observation.json').write_text(json.dumps({key: value for key, value in raw.items() if key in allowed}, indent=2))
        request_fields = []
        for path in sorted((fixture.agent_root / 'sessions').glob('request_dump_*.json')):
            raw = json.loads(path.read_text())
            body = raw.get('request', {}).get('body')
            if isinstance(body, dict):
                names = sorted(body)
                require(len(names) <= 64 and all(re.fullmatch(r'[A-Za-z_][A-Za-z0-9_]{0,80}', name) for name in names), 'invalid private request field-name observation')
                request_fields.append({'field_names': names, 'stream': body.get('stream') is True,
                    'stream_options_field_names': sorted(body.get('stream_options', {})) if isinstance(body.get('stream_options'), dict) else []})
        (args.evidence_dir / 'native-failure-request-field-names.json').write_text(json.dumps(request_fields, indent=2))
        raise
    finally:
        fixture.close()
    remaining = subprocess.run(['docker', 'ps', '-aq', '--filter', 'label=com.docker.compose.project=' + final.PROJECT], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)
    require(not remaining.stdout.strip() and not any(root.exists() for root in roots), 'all own native containers and generated Secrets/state removed')
    fixture.passed('own isolated project and generated private Secrets/Agent state removed; frozen source and sanitized evidence retained')
    sources = {str(path.relative_to(args.host_source)): hashlib.sha256(path.read_bytes()).hexdigest()
               for path in sorted(args.host_source.rglob('*.py'))}
    require(sources == host_hashes_before, 'native Python inputs changed during acceptance')
    report = {'result': 'passed', 'groups': len(fixture.facts), 'facts': fixture.facts,
              'schema_version': args.schema_version, 'purpose_registry_count': 20, 'synthetic_owner': True,
              'real_external_idp_verified': False, 'external_platform_sender_verified': False,
              'full_control_plane_verified': False, 'native_roundtrip': result,
              'core_image': args.image, 'hermes_image': args.hermes_image, 'provisioner_image': args.provisioner_image,
              'native_source_sha256': sources, 'stage_script_sha256': hashlib.sha256(args.stage_script.read_bytes()).hexdigest(),
              'installed_host_files': len(host_manifest['host_files']),
              'host_manifest_sha256': hashlib.sha256(args.host_manifest.read_bytes()).hexdigest(),
              'core_manifest_sha256': hashlib.sha256(args.core_manifest.read_bytes()).hexdigest(),
              'deployment_manifest_sha256': hashlib.sha256(args.deployment_manifest.read_bytes()).hexdigest(),
              'deployment_source_sha256': {str(path.relative_to(args.source)): hashlib.sha256(path.read_bytes()).hexdigest()
                  for path in sorted(args.source.rglob('*')) if path.is_file()},
              'test_source_sha256': {name: hashlib.sha256((HERE / name).read_bytes()).hexdigest()
                  for name in ['native_acceptance.py', 'native_ack_edge.js', 'native_provider.js', 'acceptance.py', 'provider.js']},
              'own_cleanup_verified': True, 'duration_seconds': round(time.monotonic() - started, 2)}
    (args.evidence_dir / 'native-manifest.json').write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
    print('FINAL: ' + json.dumps({'result': 'passed', 'groups': len(fixture.facts), 'provider_calls': 1, 'ordinary_rows': 2, 'accepted_platform_sends': 1, 'own_cleanup_verified': True}), flush=True)


if __name__ == '__main__':
    main()
