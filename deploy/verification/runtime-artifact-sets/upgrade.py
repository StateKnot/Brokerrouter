#!/usr/bin/env python3
"""Fresh isolated schema32 -> schema33 core and purpose-role upgrade acceptance."""
import argparse
import base64
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
from types import SimpleNamespace
import uuid

HERE = Path(__file__).resolve().parent
OLD_IMAGE = 'sha256:f91fefbc1b5dec597983bc26cd5ad6fdc2493fda737075a7bffcedfce719efdc'
OPS = 'sha256:2483917b35fd9b6941d2a3b45a95659ec443d96770cc47201d9b7e347ee42847'
MINIO = 'quay.io/minio/minio@sha256:14cea493d9a34af32f524e538b8346cf79f3321eff8e708c1e2960462bd8936e'
REQUIRED = ['deploy/permissions.sql', 'deploy/production/purpose-logins.py',
            'deploy/production/create-purpose-secrets.sh', 'deploy/production/prepare-purpose-admin.py',
            'deploy/production/pg_hba.conf', 'deploy/production/control-plane-nginx.conf']
FACT_TABLES = ['governance_turns', 'runtime_tool_results', 'runtime_final_publications', 'requests',
               'governance_operations', 'approvals', 'approval_reviews', 'guardrail_grants',
               'governance_events', 'ledger_entries']


def require(ok, fact):
    if not ok:
        raise AssertionError(fact)


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def manifest(source, path, version, image=None):
    value = json.loads(path.read_text())
    files = value['files']
    if image is not None:
        require(value.get('image_id') == image, 'compiled input manifest must bind the exact core image')
    for name, digest in files.items():
        relative = Path(name)
        require(not relative.is_absolute() and '..' not in relative.parts, 'manifest contains unsafe input path')
        actual = source / relative
        require(actual.is_file() and not actual.is_symlink() and sha(actual) == digest,
                'frozen source differs from manifest: ' + name)
    migrations = sorted((source / 'migrations').glob('*.sql'))
    require([int(item.name.split('_', 1)[0]) for item in migrations] == list(range(1, version + 1)),
            'exact migration sequence required, not merely migration count')
    for item in migrations:
        require(str(item.relative_to(source)) in files, 'manifest omits compiled migration')
    return files


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--fixture-source', type=Path, default=HERE.parents[2],
                        help='repository root containing the exact frozen existing verification modules')
    parser.add_argument('--old-image', default=OLD_IMAGE)
    parser.add_argument('--old-source', required=True, type=Path)
    parser.add_argument('--old-source-manifest', required=True, type=Path)
    parser.add_argument('--old-core-input-manifest', required=True, type=Path)
    parser.add_argument('--image', required=True, help='explicit immutable schema33 image ID; no tag fallback')
    parser.add_argument('--source', required=True, type=Path)
    parser.add_argument('--source-manifest', required=True, type=Path)
    parser.add_argument('--core-input-manifest', required=True, type=Path)
    parser.add_argument('--provisioner-image', default=OPS)
    parser.add_argument('--evidence-dir', required=True, type=Path)
    parser.add_argument('--preflight-only', action='store_true', help='verify frozen inputs without starting Docker')
    args = parser.parse_args()
    os.umask(0o077)
    for field in ['fixture_source', 'old_source', 'source', 'evidence_dir']:
        setattr(args, field, getattr(args, field).resolve())
    inputs = json.loads((HERE / 'inputs.json').read_text())
    for name, digest in inputs['reused_files'].items():
        require(sha(args.fixture_source / name) == digest, 'existing repository fixture changed: ' + name)
    for name, digest in inputs.get('candidate_files', {}).items():
        require(sha(HERE / name) == digest, 'candidate acceptance source changed: ' + name)
    for field in ['old_source_manifest', 'old_core_input_manifest', 'core_input_manifest']:
        require(sha(getattr(args, field)) == inputs['input_manifest_hashes'][field],
                'supplied manifest differs from reviewed frozen input: ' + field)
    require(sha(args.source_manifest) == inputs['deployment_source_manifest_sha256'],
            'supplied deployment manifest differs from reviewed frozen input')
    old_files = manifest(args.old_source, args.old_source_manifest, 32)
    manifest(args.old_source, args.old_core_input_manifest, 32, args.old_image)
    new_files = manifest(args.source, args.source_manifest, 33)
    manifest(args.source, args.core_input_manifest, 33, args.image)
    for name in REQUIRED:
        require(name in old_files and name in new_files, 'deployment manifest omitted required input: ' + name)
    for name in old_files:
        if name.startswith('migrations/'):
            require(new_files.get(name) == old_files[name], 'already applied migration must retain exact SQLx checksum')
    native = load('upgrade_reused_native', args.fixture_source / 'deploy/verification/final-publications/native_acceptance.py')
    final = native.final
    for image in [args.old_image, args.image, args.provisioner_image, final.identity.PG,
                  final.identity.BROWSER, final.identity.NGINX, MINIO]:
        require(re.fullmatch(r'(?:[A-Za-z0-9._/:-]+@)?sha256:[0-9a-f]{64}', image), 'immutable image reference required')
    require(args.old_image != args.image, 'upgrade requires distinct compiled schema32 and schema33 images')
    if args.preflight_only:
        print(json.dumps({'status': 'source_preflight_passed_not_deployed', 'schema_from': 32, 'schema_to': 33,
                          'old_source_files': len(old_files), 'new_source_files': len(new_files)}))
        return
    available = shutil.disk_usage('/private/tmp').free
    require(available >= 50 * 1024**3, 'less than50 GiB available: stop before isolated validation')
    require(args.evidence_dir != HERE and not args.evidence_dir.is_relative_to(args.source)
            and not args.evidence_dir.is_relative_to(args.old_source), 'new evidence must be outside frozen inputs')
    require(not args.evidence_dir.exists(), 'fresh evidence directory required; historical reports are immutable')
    args.evidence_dir.mkdir(parents=True, mode=0o700)
    project = 'brokerrouter-schema-upgrade-' + secrets.token_hex(6)
    final.PROJECT = final.identity.PROJECT = project
    final.SOURCE, final.OPS, final.EVIDENCE = args.old_source, args.provisioner_image, args.evidence_dir
    final.purpose = load('upgrade_purposes', args.old_source / 'deploy/production/purpose-logins.py')
    existing = subprocess.run(['docker', 'ps', '-aq', '--filter', 'label=com.docker.compose.project=' + project],
                              capture_output=True, check=True)
    require(not existing.stdout.strip(), 'refuse existing Docker project')
    for image in [args.old_image, args.image, args.provisioner_image, final.identity.PG,
                  final.identity.BROWSER, final.identity.NGINX, MINIO]:
        inspected = subprocess.run(['docker', 'image', 'inspect', '--format', '{{.Id}}', image],
                                   capture_output=True, check=True)
        require(re.fullmatch(rb'sha256:[0-9a-f]{64}\n?', inspected.stdout), 'fixed local image must already exist')

    class Upgrade(final.Fixture):
        # Reuse the exact strict PKI profile that the actual Python3.13 fixture uses.
        ca, leaf = native.NativeFixture.ca, native.NativeFixture.leaf

        def __init__(self, root):
            super().__init__(args.old_image, root)
            self.args = SimpleNamespace(schema_version=32)
            self.legacy = []
            self.created_turns = []

        def start_database(self):
            native.NativeFixture.start_database(self)
            self.sensitive += [(self.root / 'purpose-logins' / (name + '.password')).read_bytes()
                               for name in final.purpose.GROUPS]

        def call(self, arguments, data=None, expect=0, timeout=90):
            arguments = list(arguments)
            if [str(value) for value in arguments[:2]] == ['docker', 'run']:
                extra = ['--label', 'com.docker.compose.project=' + project,
                         '--name', project + '-probe-' + secrets.token_hex(5)]
                for option, value in [('--memory', '128m'), ('--cpus', '0.5'), ('--pids-limit', '64')]:
                    if option not in arguments:
                        extra += [option, value]
                arguments = arguments[:2] + extra + arguments[2:]
            return super().call(arguments, data, expect, timeout)

        def close(self):
            # A timed-out docker client can leave its --rm probe running. Remove
            # only this uniquely labelled task's non-compose probes before its
            # generated Secret files and compose network are removed.
            ids = subprocess.run(['docker', 'ps', '-aq', '--filter', 'label=com.docker.compose.project=' + project],
                                 capture_output=True, check=True).stdout.decode().split()
            for container_id in ids:
                labels = json.loads(self.call(['docker', 'inspect', '--format', '{{json .Config.Labels}}', container_id]))
                require(labels.get('com.docker.compose.project') == project, 'cleanup refuses unrelated container')
                if 'com.docker.compose.service' not in labels:
                    self.call(['docker', 'rm', '--force', container_id])
            super().close()

        def config(self):
            return json.loads((self.root / 'compose.json').read_text())

        def put_config(self, value):
            self.write('compose.json', json.dumps(value, indent=2))

        def provision_again(self):
            owner = self.call(['docker', 'run', '--rm', '--network', 'none', '--mount',
                'type=bind,src=' + str(self.root / 'purpose-logins') + ',dst=/purpose,readonly',
                '--entrypoint', 'stat', final.identity.PG, '-c', '%u:%g', '/purpose']).decode().strip()
            uid, gid = owner.split(':')
            self.call(['docker', 'run', '--rm', '--network', project + '_backend', '--user', owner,
                '--read-only', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges:true',
                '--tmpfs', '/run/purpose-secrets:size=1m,mode=0700,uid=' + uid + ',gid=' + gid,
                '--tmpfs', '/tmp:size=8m,mode=1777', '--mount', 'type=bind,src=' + str(self.root / 'purpose-logins')
                + ',dst=/run/purpose-secrets/purpose-logins,readonly', '--mount', 'type=bind,src=' + str(self.root / '.purpose-logins.lock')
                + ',dst=/run/purpose-secrets/.purpose-logins.lock', '--mount', 'type=bind,src=' + str(self.root / 'purpose-admin.service')
                + ',dst=/run/secrets/purpose_admin_service,readonly', '--mount', 'type=bind,src=' + str(self.root / 'purpose-admin.passfile')
                + ',dst=/run/secrets/purpose_admin_passfile,readonly', '--mount', 'type=bind,src=' + str(self.root / 'pg-ca.crt')
                + ',dst=/run/secrets/pg_ca,readonly', args.provisioner_image], timeout=60)

        def ready(self):
            for name in ['runtime', 'control', 'approvals']:
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    try:
                        if self.http('https://' + name + '.brokerrouter.internal:8443/health/ready',
                                     ca=name + '-ca.crt')['status'] == 204:
                            break
                    except RuntimeError:
                        pass
                    time.sleep(0.25)
                else:
                    raise AssertionError('verified HTTPS readiness failed: ' + name)

        def ttl(self, seconds, restart=True):
            text = (self.root / 'runtime.env').read_text()
            text, replaced = re.subn(r'^BROKERROUTER_RESULT_TTL_SECONDS=.*$',
                                     'BROKERROUTER_RESULT_TTL_SECONDS=' + str(seconds), text, flags=re.M)
            require(replaced == 1, 'exact single runtime TTL setting required')
            self.write('runtime.env', text)
            if restart:
                self.compose('up', '--detach', '--force-recreate', '--wait', '--wait-timeout', '60', 'gateway')
                self.ready()

        def json200(self, response, fact):
            if response['status'] != 200:
                value = json.loads(response['body'])
                code = value.get('error', {}).get('code', '')
                require(isinstance(code, str) and re.fullmatch(r'[a-z_]*', code) is not None,
                        fact + ': unrecognized safe error code')
                raise AssertionError(fact + ': HTTP' + str(response['status']) + '/' + code)
            return json.loads(response['body'])

        def new_turn(self):
            ordinal = len(self.created_turns) + 1
            value = self.json200(self.human_http('/v1/governance/turns', {
                'key_id': self.keyid, 'session_id': 'synthetic-upgrade-session-' + str(ordinal),
                'message_id': 'synthetic-upgrade-message-' + str(ordinal),
                'run_id': 'synthetic-upgrade-run-' + str(ordinal),
                'generation': 1, 'ttl_seconds': 1800}), 'actual controller creates an upgrade turn')
            self.turn = str(uuid.UUID(value['id']))
            require(self.turn not in self.created_turns, 'independent historical cases must have distinct actual turns')
            self.created_turns.append(self.turn)

        def active_legacy_turns(self):
            require(len(self.legacy) == 3, 'all three historical states must exist')
            ids = ','.join("'" + str(uuid.UUID(old['turn'])) + "'" for old in self.legacy)
            observed = self.admin("SELECT count(*),count(DISTINCT session_id),bool_and(state='active' AND expires_at>clock_timestamp()) FROM governance_turns WHERE id IN (" + ids + ");").decode().strip()
            require(observed == '3|3|t', 'independent legacy sessions retain three active live owner turns')

        def tool_input(self, tag, tool='fixture_tool'):
            value = self.json200(self.agent('/v1/governance/checks', {
                'turn_id': self.turn, 'operation_key': 'upgrade-' + tag, 'stage': 'tool_input',
                'model_id': 'fixture', 'tool_name': tool, 'payload': {'fixture': tag}}), 'real runtime input check')
            require(value['state'] == 'ready', 'input must be actually ready')
            token = secrets.token_hex(32)
            self.sensitive.append(token.encode())
            claimed = self.json200(self.agent('/v1/governance/operations/' + value['id'] + '/claim',
                {'token': token, 'payload_hash': value['payload_hash']}), 'real runtime claim')
            require(claimed['dispatch_authorized'] is True, 'exact input claim may dispatch once')
            return value, token

        def approve_text(self, pending, rail, text):
            approval = pending['approval']['id']
            reviewed = self.json200(self.human_http('/approvals/v1/review', {'approval_id': approval},
                                                   key='approvals'), 'legacy current-purpose complete review')
            require(reviewed['can_continue'] is True and json.loads(reviewed['payload_text'])['text'] == text,
                    'actual workbench must show complete exact legacy text')
            self.json200(self.human_http('/approvals/v1/decide', {
                'approval_id': approval, 'review_id': reviewed['review_id'], 'acknowledged_scope': True,
                'decision': {'decision': 'continue_this_turn', 'approved_guardrail_ids': [rail], 'feedback': None}},
                key='approvals'), 'real review-bound owner decision')

        def legacy_policy(self):
            # Genuine output policy that flags transformed finals while leaving
            # the fixed raw model source safe. An always post policy here would
            # gate the source request itself before the publication can exist.
            self.write('rail.json', json.dumps({'id': 'final-text-review', 'version': 1,
                'name': 'Schema upgrade transformed final review', 'stage': 'post',
                'rule': {'kind': 'contains_any', 'terms': ['升级验收最终正文'], 'case_sensitive': True}}))
            self.write('policy.json', json.dumps({'id': str(uuid.uuid4()), 'guardrail_id': 'final-text-review',
                'model_id': None, 'key_id': self.keyid, 'runtime_id': 'final-text-fixture',
                'tool_name': None, 'active': True}))
            self.write('tool-rail.json', json.dumps({'id': 'legacy-tool-review', 'version': 1,
                'name': 'Schema upgrade legacy text review', 'stage': 'tool_output', 'rule': {'kind': 'always'}}))
            self.write('tool-policy.json', json.dumps({'id': str(uuid.uuid4()), 'guardrail_id': 'legacy-tool-review',
                'model_id': None, 'key_id': self.keyid, 'runtime_id': 'final-text-fixture',
                'tool_name': 'fixture_tool', 'active': True}))
            config = self.config()
            config['services']['operator'].setdefault('volumes', []).extend([str(self.root / name) + ':/fixture/' + name + ':ro'
                                                        for name in ['rail.json', 'policy.json', 'tool-rail.json', 'tool-policy.json']]
                                                       )
            self.put_config(config)
            self.cli('guardrail-publish', '--tenant', self.tenant, '--file', '/fixture/rail.json')
            self.cli('policy-put', '--tenant', self.tenant, '--file', '/fixture/policy.json')
            self.cli('guardrail-publish', '--tenant', self.tenant, '--file', '/fixture/tool-rail.json')
            self.cli('policy-put', '--tenant', self.tenant, '--file', '/fixture/tool-policy.json')

        def legacy_roundtrip(self, tag, approve=False):
            self.new_turn()
            op, token = self.tool_input(tag)
            text = '升级验收工具正文 🐎 ' + tag
            final_text = '升级验收最终正文\nUTF-8 exact text ' + tag
            self.sensitive += [text.encode(), final_text.encode()]
            record = {'input_operation_id': op['id'], 'claim_token': token,
                      'evidence_ref': 'runtime://schema-upgrade/' + tag, 'text': text}
            tool = self.json200(self.agent('/v1/governance/tool-results', record), 'legacy tool result record')
            tool_path = '/v1/governance/tool-results/' + tool['id'] + '/release'
            tool_response = self.json200(self.agent(tool_path, {}), 'legacy gated tool release')
            require(tool_response['state'] == 'waiting_approval' and 'text' not in tool_response,
                    'legacy unapproved tool text withheld')
            if approve:
                self.approve_text(tool_response, 'legacy-tool-review', text)
                tool_response = self.json200(self.agent(tool_path, {}), 'approved legacy tool release')
                require(tool_response['state'] == 'ready' and tool_response['text'] == text, 'exact approved tool bytes')
            chat = self.agent('/v1/chat/completions', {'model': 'fixture', 'messages': [
                {'role': 'user', 'content': 'Synthetic local model input'}], 'max_tokens': 20, 'stream': False},
                extra_headers={'Idempotency-Key': 'upgrade-source-' + tag, 'x-brokerrouter-turn-id': self.turn})
            raw = self.json200(chat, 'actual schema32 model source')
            source = str(uuid.UUID(chat['headers']['x-brokerrouter-request-id']))
            require(raw['id'] != source, 'server UUID, not provider ID')
            final_record = {'turn_id': self.turn, 'model_id': 'fixture', 'source_request_ids': [source], 'text': final_text}
            publication = self.json200(self.agent('/v1/governance/final-publications', final_record), 'legacy final record')
            final_path = '/v1/governance/final-publications/' + publication['id'] + '/release'
            final_response = self.json200(self.agent(final_path, {}), 'legacy gated final release')
            require(final_response['state'] == 'waiting_approval' and 'text' not in final_response,
                    'legacy unapproved final text withheld')
            if approve:
                self.approve_text(final_response, 'final-text-review', final_text)
                final_response = self.json200(self.agent(final_path, {}), 'approved legacy final release')
                require(final_response['state'] == 'ready' and final_response['text'] == final_text, 'exact approved final bytes')
            self.legacy.append({'tag': tag, 'tool_id': tool['id'], 'publication_id': publication['id'],
                'turn': self.turn, 'tool_record': record, 'final_record': final_record,
                'tool_metadata': tool, 'final_metadata': publication,
                'tool_path': tool_path, 'final_path': final_path,
                'tool_response': tool_response, 'final_response': final_response})

        def fingerprints(self):
            values = {}
            for table in FACT_TABLES:
                # Ciphertext participates in the digest, but never leaves PostgreSQL.
                sql = "SELECT encode(sha256(convert_to(COALESCE(jsonb_agg(v ORDER BY v::text)::text,'[]'),'UTF8')),'hex') FROM (SELECT to_jsonb(r)-'artifact_set'-'artifact_set_hash' AS v FROM " + table + " r) facts;"
                values[table] = self.admin(sql).decode().strip()
            return values

        def identity_fingerprint(self):
            return self.admin("SELECT encode(sha256(convert_to(COALESCE(jsonb_agg(to_jsonb(r) ORDER BY rolname)::text,'[]'),'UTF8')),'hex') FROM (SELECT rolname,rolpassword,rolsuper,rolcreatedb,rolcreaterole,rolreplication,rolbypassrls,rolcanlogin FROM pg_authid WHERE rolname LIKE 'brokerrouter_%') r;").decode().strip()

        def purpose_sql(self, purpose_name, sql, denied=False):
            if purpose_name in ['runtime', 'operator']:
                login, password = 'brokerrouter_' + purpose_name + '_login', self.passwords[purpose_name]
            else:
                login = final.purpose.LOGINS[purpose_name]
                password = (self.root / 'purpose-logins' / (purpose_name + '.password')).read_text()
            passfile = self.write('probe.pgpass', 'postgres:5432:brokerrouter:' + login + ':' + password + '\n')
            owner = self.call(['docker', 'run', '--rm', '--network', 'none', '--mount',
                'type=bind,src=' + str(passfile) + ',dst=/pass,readonly', '--entrypoint', 'stat',
                final.identity.PG, '-c', '%u:%g', '/pass']).decode().strip()
            command = ['docker', 'run', '--rm', '-i', '--network', project + '_backend', '--user', owner,
                '--label', 'com.docker.compose.project=' + project, '--name', project + '-role-' + secrets.token_hex(5),
                '--memory', '128m', '--cpus', '0.5', '--pids-limit', '64',
                '--read-only', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges:true',
                '--mount', 'type=bind,src=' + str(passfile) + ',dst=/run/probe.pgpass,readonly',
                '--mount', 'type=bind,src=' + str(self.root / 'pg-ca.crt') + ',dst=/run/ca.crt,readonly',
                '--env', 'PGPASSFILE=/run/probe.pgpass', '--env', 'PGSSLMODE=verify-full',
                '--env', 'PGSSLROOTCERT=/run/ca.crt', '--entrypoint', 'psql', final.identity.PG,
                '-h', 'postgres', '-U', login, '-d', 'brokerrouter', '-X', '-q', '-A', '-t',
                '--set=ON_ERROR_STOP=1', '--set=VERBOSITY=verbose']
            result = subprocess.run(command, input=sql.encode(), capture_output=True, timeout=30)
            self.transcript += [result.stdout, result.stderr]
            if denied:
                require(result.returncode == 3 and re.search(rb'\b42501\b', result.stderr) is not None,
                        'negative authority check must be actual42501, not arbitrary SQL failure')
            else:
                require(result.returncode == 0, 'actual purpose SQL failed; no administrator substitution')
            return result.stdout.decode().strip()

        def upgrade(self):
            self.active_legacy_turns()
            self.compose('stop', '--timeout', '10', 'gateway', 'control', 'approvals')
            expired = self.legacy[0]
            deadline = time.monotonic() + 75
            while time.monotonic() < deadline:
                expired_now = self.admin("SELECT bool_and(expires_at<=clock_timestamp()) FROM (SELECT expires_at FROM runtime_tool_results WHERE id='" + expired['tool_id'] + "' UNION ALL SELECT expires_at FROM runtime_final_publications WHERE id='" + expired['publication_id'] + "') r;").decode().strip()
                if expired_now == 't':
                    break
                time.sleep(0.25)
            require(expired_now == 't', 'natural old server TTL must expire by PostgreSQL clock')
            self.before = self.fingerprints()
            identity_before = self.identity_fingerprint()
            config = self.config()
            config['services']['candidate-gateway'] = {**config['services']['gateway'], 'image': args.image,
                                                      'profiles': ['upgrade-probe'], 'restart': 'no'}
            self.put_config(config)
            self.compose('up', '--detach', 'candidate-gateway')
            candidate_id = self.compose('ps', '--all', '--quiet', 'candidate-gateway').decode().strip()
            require(candidate_id, 'candidate schema check container must exist')
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                state = json.loads(self.call(['docker', 'inspect', '--format', '{{json .State}}', candidate_id]))
                if state['Status'] == 'exited':
                    break
                time.sleep(0.25)
            require(state['Status'] == 'exited' and state['ExitCode'] != 0,
                    'new core must fail closed before schema33, never listen as ready on schema32')
            self.compose('rm', '--force', 'candidate-gateway')
            del config['services']['candidate-gateway']
            for name in ['gateway', 'control', 'approvals', 'operator', 'migration']:
                config['services'][name]['image'] = args.image
            self.put_config(config)
            self.cli(kind='migration')
            require(self.fingerprints() == self.before, 'migration changed original source/body/hash/approval/expiry facts')
            self.admin((args.source / 'deploy/permissions.sql').read_text())
            # Idempotent migration and canonical-purpose application must preserve originals.
            self.cli(kind='migration')
            self.admin((args.source / 'deploy/permissions.sql').read_text())
            self.provision_again()
            require(self.fingerprints() == self.before, 'upgrade retry changed historical facts')
            require(self.identity_fingerprint() == identity_before, 'canonical grants must not rotate LOGIN Secrets')
            require(self.admin('SELECT version FROM _sqlx_migrations WHERE success ORDER BY version;').decode().split()
                    == [str(n) for n in range(1, 34)], 'actual upgraded schema is exactly33')
            require(self.admin('SELECT bool_and(artifact_set IS NULL AND artifact_set_hash IS NULL) FROM (SELECT artifact_set,artifact_set_hash FROM runtime_tool_results UNION ALL SELECT artifact_set,artifact_set_hash FROM runtime_final_publications) r;').decode().strip() == 't',
                    'all historical text-only rows retain pairedNULL set facts')
            self.passed('schema33 core rejects schema32; actual migrate+permissions rerun preserves historical ciphertext/source/hash/approval/expiry facts and canonical LOGIN identities')
            self.compose('up', '--detach', '--force-recreate', '--wait', '--wait-timeout', '60', 'gateway', 'control', 'approvals')
            self.ready()
            self.active_legacy_turns()
            for old in self.legacy:
                for path, expected in [(old['tool_path'], old['tool_response']), (old['final_path'], old['final_response'])]:
                    response = self.agent(path, {})
                    observed = json.loads(response['body'])
                    code = observed.get('error', {}).get('code')
                    require(code is None or isinstance(code, str) and re.fullmatch(r'[a-z_]+', code) is not None,
                            'unexpected safe error category')
                    observation = {'case': old['tag'], 'lane': 'tool' if path == old['tool_path'] else 'final',
                        'status': response['status'], 'error_code': code,
                        'state': observed.get('state'), 'has_text': 'text' in observed,
                        'response_sha256': hashlib.sha256(response['body'].encode()).hexdigest()}
                    with (args.evidence_dir / 'legacy-http-observations.jsonl').open('a') as evidence:
                        evidence.write(json.dumps(observation) + '\n')
                    if old['tag'] == 'expired':
                        require(response['status'] == 410 and code == 'result_unavailable' and 'text' not in observed,
                                'expired historical plaintext remains unavailable: HTTP'
                                + str(response['status']) + '/' + str(code))
                    else:
                        require(self.json200(response, 'legacy upgraded exact response') == expected,
                                'legacy ready/pending exact JSON changed across upgrade')
                require(self.json200(self.agent('/v1/governance/tool-results', old['tool_record']), 'old tool record idempotency')
                        == old['tool_metadata'], 'old immutable tool record retry changed')
                require(self.json200(self.agent('/v1/governance/final-publications', old['final_record']), 'old final record idempotency')
                        == old['final_metadata'], 'old immutable final record retry changed')
            require(self.stats()['calls'] == 3, 'upgrade/replay must not repeat model execution')
            self.passed('upgraded actual Runtime HTTP keeps approved/pending exact UTF8 JSON; old expired records deny plaintext; all record retries recover immutable metadata with model calls remaining3')

        def role_matrix(self):
            for purpose_name, group in final.purpose.GROUPS.items():
                login = final.purpose.LOGINS[purpose_name]
                actual = self.purpose_sql(purpose_name, "SELECT current_user,(SELECT ssl FROM pg_stat_ssl WHERE pid=pg_backend_pid()),pg_has_role(current_user,'" + group + "','MEMBER');")
                require(actual == login + '|t|t', 'registered actual LOGIN must use own role and PostgreSQL TLS')
            self.passed('all20 independently generated canonical purpose LOGINs actually connect with verify-full PostgreSQL TLS and intended group membership')

        def capture(self):
            self.new_turn()
            self.cli('asset-quota-set', '--tenant', self.tenant, '--bytes', '1048576')
            self.cli('asset-quota-set', '--tenant', self.tenant, '--key', self.keyid, '--bytes', '1048576')
            self.ca('s3-ca')
            self.leaf('s3-server', 's3-ca', ['s3'])
            self.ca('capture-ca')
            self.leaf('capture-server', 'capture-ca', ['capture.brokerrouter.internal'])
            access, secret = 'task-' + secrets.token_hex(12), secrets.token_hex(32)
            self.sensitive += [access.encode(), secret.encode()]
            self.write('s3-access', access)
            self.write('s3-secret', secret)
            password = (self.root / 'purpose-logins/runtime-assets.password').read_text()
            self.sensitive.append(password.encode())
            self.envfile('capture.env', {'DATABASE_URL': 'postgres://brokerrouter_runtime_assets_login:' + password
                + '@postgres:5432/brokerrouter?sslmode=verify-full&sslrootcert=/run/secrets/pg_ca',
                'BROKERROUTER_ACTIVE_RESULT_KEY': 'result-v1', 'BROKERROUTER_RESULT_KEYS': json.dumps({'result-v1': self.result}),
                'BROKERROUTER_ASSET_S3_ENDPOINT': 'https://s3:9000/', 'BROKERROUTER_ASSET_S3_BUCKET': 'upgrade-candidates',
                'BROKERROUTER_ASSET_S3_REGION': 'us-east-1', 'BROKERROUTER_ASSET_S3_ACCESS_KEY_ID': access,
                'BROKERROUTER_ASSET_S3_SECRET_ACCESS_KEY': secret, 'BROKERROUTER_ASSET_S3_CA_FILE': '/run/secrets/s3_ca'})
            config = self.config()
            for name, path in [('s3_access', 's3-access'), ('s3_secret', 's3-secret'), ('s3_cert', 's3-server.crt'),
                               ('s3_key', 's3-server.key'), ('s3_ca', 's3-ca.crt'), ('capture_cert', 'capture-server.crt'),
                               ('capture_key', 'capture-server.key'), ('capture_env', 'capture.env')]:
                config['secrets'][name] = {'file': str(self.root / path)}
            config['services']['s3'] = {'image': MINIO, 'entrypoint': ['sh', '-ec',
                'mkdir -p /tls; cp /run/secrets/cert /tls/public.crt; cp /run/secrets/key /tls/private.key; chmod 600 /tls/private.key; exec minio server /data --certs-dir /tls --console-address :9001'],
                'environment': {'MINIO_ROOT_USER_FILE': '/run/secrets/access', 'MINIO_ROOT_PASSWORD_FILE': '/run/secrets/password'},
                'secrets': [{'source': src, 'target': dst} for src, dst in [('s3_access', 'access'), ('s3_secret', 'password'), ('s3_cert', 'cert'), ('s3_key', 'key')]],
                'read_only': True, 'tmpfs': ['/data:size=64m', '/tls:size=8m', '/tmp:size=8m'],
                'cap_drop': ['ALL'], 'security_opt': ['no-new-privileges:true'], 'mem_limit': '256m', 'cpus': 0.5,
                'pids_limit': 64, 'networks': ['backend']}
            config['services']['capture'] = {**config['services']['gateway'],
                'command': ['serve-assets', '--runtime-output', '--bind', '0.0.0.0:8082'],
                'secrets': ['pg_ca', 's3_ca', {'source': 'capture_env', 'target': 'application_env'}],
                'healthcheck': {'test': ['CMD', 'curl', '-fsS', 'http://127.0.0.1:8082/health/ready'], 'interval': '2s', 'timeout': '2s', 'retries': 30}}
            template = (args.source / 'deploy/production/control-plane-nginx.conf').read_text()
            head = template[:template.index('    server { listen 8443 ssl default_server;')]
            self.write('capture-nginx.conf', head + '    server { listen 8443 ssl default_server; ssl_reject_handshake on; return 421; }\n    server { listen 8443 ssl; server_name capture.brokerrouter.internal; if ($host != capture.brokerrouter.internal) { return 421; } error_log /dev/null; location / { set $fixed_upstream capture:8082; proxy_pass http://$fixed_upstream; } }\n}\n', 0o444)
            config['services']['capture-tls'] = {**config['services']['runtime-tls'],
                'secrets': [{'source': 'capture_cert', 'target': 'tls_fullchain'}, {'source': 'capture_key', 'target': 'tls_private_key'}],
                'volumes': [str(self.root / 'capture-nginx.conf') + ':/etc/nginx/nginx.conf:ro'],
                'networks': {'backend': {'aliases': ['capture.brokerrouter.internal']}}}
            self.put_config(config)
            self.compose('up', '--detach', 's3')
            curl = ['docker', 'run', '--rm', '-i', '--network', project + '_backend', '--read-only', '--cap-drop', 'ALL',
                    '--label', 'com.docker.compose.project=' + project, '--memory', '128m', '--cpus', '0.5', '--pids-limit', '64',
                    '--security-opt', 'no-new-privileges:true', '--mount', 'type=bind,src=' + str(self.root / 's3-ca.crt')
                    + ',dst=/run/ca.crt,readonly', '--entrypoint', 'curl', args.image, '--config', '-']
            curl_config = ('url="https://s3:9000/upgrade-candidates"\nrequest="PUT"\naws-sigv4="aws:amz:us-east-1:s3"\nuser="'
                           + access + ':' + secret + '"\ncacert="/run/ca.crt"\nfail=true\nsilent=true\nshow-error=true\nconnect-timeout=2\nmax-time=10\n').encode()
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                created = subprocess.run(curl, input=curl_config, capture_output=True, timeout=15)
                self.transcript += [created.stdout, created.stderr]
                if created.returncode == 0:
                    break
                time.sleep(0.25)
            require(created.returncode == 0, 'strict TLS signed isolated S3 bucket bootstrap failed')
            self.compose('up', '--detach', '--wait', '--wait-timeout', '60', 'capture')
            self.compose('up', '--detach', 'capture-tls')
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                try:
                    if self.http('https://capture.brokerrouter.internal:8443/health/ready', ca='capture-ca.crt')['status'] == 204:
                        break
                except RuntimeError:
                    pass
                time.sleep(0.25)
            else:
                raise AssertionError('capture purpose verified HTTPS readiness failed')
            op, token = self.tool_input('coupled-files', 'brokerrouter_fixed_mp4')
            post = lambda path, body: self.http('https://capture.brokerrouter.internal:8443' + path, body,
                {'Content-Type': 'application/json', 'Authorization': 'Bearer ' + self.key}, ca='capture-ca.crt')
            batch = self.json200(post('/v1/runtime-artifacts', {'input_operation_id': op['id'], 'claim_token': token,
                'input_payload_hash': op['payload_hash'], 'original_payload_hash': op['payload_hash'],
                'maximum_bytes': 1024, 'maximum_artifacts': 2}), 'real isolated RuntimeAssets reserve')
            # Small quarantined format candidates only. This upgrade does not certify playable media.
            files = [bytes.fromhex('000000186674797069736f6d0000000069736f6d6d703432000000086d646174') + bytes([n]) for n in range(2)]
            evidence = 'runtime://schema-upgrade/two-candidates'
            manifest_body = {'claim_token': token, 'evidence_ref': evidence, 'artifacts': [
                {'sha256': hashlib.sha256(body).hexdigest(), 'byte_length': len(body), 'media_type': 'video/mp4',
                 'producer_revision': hashlib.sha256(b'schema-upgrade-fixture-v1').hexdigest()} for body in files]}
            current = self.json200(post('/v1/runtime-artifacts/' + batch['id'] + '/manifest', manifest_body), 'real completed manifest receipt')
            for ordinal, body in enumerate(files):
                payload = json.dumps({'url': 'https://capture.brokerrouter.internal:8443/v1/runtime-artifacts/' + batch['id']
                    + '/artifacts/' + str(ordinal) + '/chunks/0', 'key': self.key, 'claim': token,
                    'body': base64.b64encode(body).decode()}).encode()
                script = "let b='';process.stdin.on('data',x=>b+=x);process.stdin.on('end',()=>{const x=JSON.parse(b),https=require('https'),fs=require('fs');const q=https.request(x.url,{method:'PUT',ca:fs.readFileSync('/run/ca.crt'),headers:{Authorization:'Bearer '+x.key,'content-type':'application/octet-stream','x-brokerrouter-claim-token':x.claim},timeout:15000},r=>{let s='';r.on('data',y=>s+=y);r.on('end',()=>process.stdout.write(JSON.stringify({status:r.statusCode,body:s})));});q.on('error',()=>{process.exitCode=2;});q.on('timeout',()=>q.destroy());q.end(Buffer.from(x.body,'base64'));});"
                response = json.loads(self.call(['docker', 'run', '--rm', '-i', '--network', project + '_backend',
                    '--read-only', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges:true', '--mount',
                    'type=bind,src=' + str(self.root / 'capture-ca.crt') + ',dst=/run/ca.crt,readonly',
                    '--entrypoint', 'node', final.identity.BROWSER, '-e', script], payload, timeout=30))
                self.json200(response, 'real exact candidate chunk upload')
            sealed = self.json200(post('/v1/runtime-artifacts/' + batch['id'] + '/complete', {'claim_token': token}), 'real capture sealed')
            require(sealed['state'] == 'quarantined' and len(sealed['artifacts']) == 2, 'only quarantined whole two-file capture')
            text = json.dumps({'artifact_count': 2, 'contract': 'brokerrouter-fixed-mp4-result-v1'}, sort_keys=True, separators=(',', ':'))
            self.sensitive.append(text.encode())
            recorded = self.json200(self.agent('/v1/governance/tool-results', {'input_operation_id': op['id'],
                'claim_token': token, 'evidence_ref': evidence, 'text': text}), 'real Runtime coupled text record')
            require(recorded['artifact_set']['contract'] == 'runtime-artifact-set-v1'
                    and len(recorded['artifact_set']['files']) == 2 and 'text' not in recorded,
                    'record must server-derive entire two-file immutable set without plaintext')
            profile = {'tenant_id': self.tenant, 'id': str(uuid.uuid4()), 'model_id': 'fixture', 'version': 1,
                       'engine_revision': hashlib.sha256(b'upgrade-only-fixture-v1').hexdigest()}
            self.write('profile.json', json.dumps(profile))
            config = self.config()
            config['services']['operator']['volumes'].append(str(self.root / 'profile.json') + ':/fixture/profile.json:ro')
            self.put_config(config)
            self.cli('output-scan-profile-publish', '--file', '/fixture/profile.json')
            pending = self.json200(self.agent('/v1/governance/tool-results/' + recorded['id'] + '/release', {}), 'real Runtime reads narrow native asset facts')
            require(pending['state'] == 'waiting_artifacts' and len(pending['artifacts']) == 2
                    and all(file['state'] == 'checking_scan' for file in pending['artifacts']) and 'text' not in pending,
                    'unscanned whole set cannot release the coupled body')
            self.coupled = {'batch_id': batch['id'], 'result_id': recorded['id'], 'input_operation_id': op['id'],
                            'artifact_set_sha256': recorded['artifact_set']['sha256'], 'file_count': 2}
            self.passed('actual RuntimeAssets HTTPS/S3-TLS captures two quarantined candidates; Runtime derives complete text+set and returns checking_scan metadata with zero plaintext/scan/grant')

        def c_roles(self):
            where = "tenant_id='" + self.tenant + "'"
            require(self.purpose_sql('runtime', 'SELECT count(*) FROM runtime_artifact_asset_facts WHERE ' + where + ';') == '2', 'Runtime sees two narrow native metadata rows')
            require(self.purpose_sql('runtime', "SELECT count(*) FROM assets WHERE " + where + " AND kind='runtime_output';") == '0', 'Runtime raw native assets remain filtered')
            for name in ['runtime', 'operator', 'approvals', 'control']:
                require(self.purpose_sql(name, 'SELECT count(*) FROM runtime_artifact_source_facts WHERE ' + where
                    + '; SELECT count(*) FROM runtime_tool_result_review_facts WHERE ' + where + ';').splitlines()[0] == '1',
                    'actual source fact invoker functions work for ' + name)
            for name in ['approvals', 'control', 'runtime-assets']:
                self.purpose_sql(name, 'SELECT result_ciphertext FROM runtime_tool_results;', denied=True)
                self.purpose_sql(name, 'SELECT result_ciphertext FROM runtime_final_publications;', denied=True)
            self.purpose_sql('runtime', 'SELECT * FROM asset_chunks;', denied=True)
            self.purpose_sql('runtime', 'UPDATE runtime_artifact_batches SET state=state;', denied=True)
            self.purpose_sql('runtime-assets', 'INSERT INTO runtime_artifact_grants SELECT * FROM runtime_artifact_grants;', denied=True)
            self.purpose_sql('approvals', 'UPDATE guardrail_evaluations SET state=state;', denied=True)
            for name in ['mcp-server', 'mcp-worker']:
                require(int(self.purpose_sql(name, 'SELECT count(*) FROM runtime_output_set_freezes WHERE ' + where + ';')) >= 7,
                        'MCP narrow freeze identity projection is usable')
                self.purpose_sql(name, 'SELECT result_ciphertext FROM runtime_tool_results;', denied=True)
            self.passed('actual LOGIN C matrix: narrow source/native facts succeed, Runtime raw native metadata0/chunks42501, Human/storage runtime bodies42501, no producer/grant/evaluation escalation, MCP narrow freezes usable')

    root = Path(tempfile.mkdtemp(prefix=project + '-private-', dir='/private/tmp'))
    root.rmdir()
    fixture = Upgrade(root)
    start = time.monotonic()
    try:
        fixture.setup()
        fixture.ttl(60, restart=False)
        fixture.start_database()
        fixture.seed()
        fixture.legacy_policy()
        fixture.legacy_roundtrip('expired')
        fixture.ttl(3600)
        fixture.legacy_roundtrip('ready', approve=True)
        fixture.legacy_roundtrip('pending')
        fixture.passed('actual old schema32 Runtime/controller/workbench created pending and approved tool/final text; first result/source expires through real server TTL')
        fixture.upgrade()
        fixture.role_matrix()
        fixture.capture()
        fixture.c_roles()
        fixture.logs_safe()
        logs = args.evidence_dir / 'r1-final-publications-services.log'
        report = {'result': 'passed', 'scope': 'core_schema_roles_upgrade_only', 'schema_from': 32, 'schema_to': 33,
            'project': project, 'facts': fixture.facts, 'groups': len(fixture.facts), 'old_core_image': args.old_image,
            'new_core_image': args.image, 'provisioner_image': args.provisioner_image,
            'history_fingerprints': fixture.before, 'coupled': fixture.coupled, 'provider_calls': fixture.stats()['calls'],
            'native_hermes_verified': False, 'producer_certified': False, 'external_idp_verified': False,
            'scanner_certified': False, 'file_grants_or_binary_delivery_verified': False,
            'old_source_manifest_sha256': sha(args.old_source_manifest), 'source_manifest_sha256': sha(args.source_manifest),
            'old_compiled_inputs_sha256': sha(args.old_core_input_manifest), 'compiled_inputs_sha256': sha(args.core_input_manifest),
            'candidate_inputs_sha256': sha(HERE / 'inputs.json'), 'services_log_sha256': sha(logs),
            'duration_seconds': round(time.monotonic() - start, 2), 'own_cleanup_verified': False}
    except Exception as error:
        print('FAIL: ' + type(error).__name__, flush=True)
        if fixture.started:
            logs = fixture.compose('logs', '--no-color', timeout=30)
            for value in fixture.sensitive:
                logs = logs.replace(value, b'[REDACTED]')
            (args.evidence_dir / 'failure-services.log').write_bytes(logs)
        raise
    finally:
        fixture.close()
    remaining = subprocess.run(['docker', 'ps', '-aq', '--filter', 'label=com.docker.compose.project=' + project],
                               capture_output=True, check=True)
    require(not remaining.stdout.strip() and not root.exists(), 'own project and generated Secrets must be removed')
    report['own_cleanup_verified'] = True
    (args.evidence_dir / 'upgrade-manifest.json').write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
    print('FINAL: ' + json.dumps({'result': 'passed', 'scope': report['scope'], 'groups': report['groups'],
                                'schema_from': 32, 'schema_to': 33, 'own_cleanup_verified': True}), flush=True)


if __name__ == '__main__':
    main()
