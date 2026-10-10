#!/usr/bin/env python3
"""Actual isolated schema31 final-text HTTP publication/approval/restart acceptance."""
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
import uuid

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
spec = importlib.util.spec_from_file_location('identity_fixture', HERE.parent / 'identity' / 'acceptance.py')
identity = importlib.util.module_from_spec(spec)
spec.loader.exec_module(identity)
# Locally built tool content is addressed by its image ID, not a nonexistent registry RepoDigest.
identity.BROWSER = 'sha256:595490e76f3a0db1125d1846f84253c506ac219140c0fb82f00e8cc5582854a6'
PROJECT = 'brokerrouter-final-text-test-' + secrets.token_hex(6)
ISSUER = 'https://human.final-text.fixture.invalid'
OPS = 'sha256:2483917b35fd9b6941d2a3b45a95659ec443d96770cc47201d9b7e347ee42847'
SOURCE = None
EVIDENCE = None
purpose = None


def require(ok, fact):
    if not ok:
        raise AssertionError(fact)


class Fixture(identity.Fixture):
    def __init__(self, image, root):
        self.image = image
        self.project = PROJECT
        self.root = root
        self.root.mkdir(mode=0o700)
        self.transcript = []
        self.started = False
        self.sensitive = []
        self.facts = []
        self.passwords = {name: secrets.token_hex(32) for name in ['migration', 'runtime', 'operator']}
        self.model_key = secrets.token_hex(32)
        self.result = secrets.token_hex(32)
        self.sensitive += [value.encode() for value in self.passwords.values()] + [self.model_key.encode(), self.result.encode()]
        self.sensitive += [b'Synthetic local model input', b'raw synthetic provider answer']

    def passed(self, fact):
        self.facts.append(fact)
        print('PASS: ' + fact, flush=True)

    def setup(self):
        for name in ['pg-ca', 'runtime-ca', 'control-ca', 'approvals-ca']:
            self.ca(name)
        self.leaf('pg-server', 'pg-ca', ['postgres'])
        for name in ['runtime', 'control', 'approvals']:
            self.leaf(name + '-server', name + '-ca', [name + '.brokerrouter.internal'])
        self.keys = {name: self.openssl('genpkey', '-algorithm', 'RSA', '-pkeyopt', 'rsa_keygen_bits:2048').decode() for name in ['human', 'approvals']}
        self.pubs = {name: self.openssl('pkey', '-pubout', data=value.encode()).decode() for name, value in self.keys.items()}
        self.sensitive += [value.encode() for value in self.keys.values()]
        common = {'BROKERROUTER_ACTIVE_RESULT_KEY': 'result-v1', 'BROKERROUTER_RESULT_KEYS': json.dumps({'result-v1': self.result})}

        def dsn(name, password):
            return 'postgres://brokerrouter_' + name + ':' + password + '@postgres:5432/brokerrouter?sslmode=verify-full&sslrootcert=/run/secrets/pg_ca'

        for name in ['migration', 'runtime', 'operator']:
            self.write('postgres-' + name + '-password', self.passwords[name])
            values = {'DATABASE_URL': dsn('migration' if name == 'migration' else name + '_login', self.passwords[name])}
            if name == 'runtime':
                values.update(common, FIXTURE_MODEL_KEY=self.model_key, BROKERROUTER_RESULT_TTL_SECONDS='3600', BROKERROUTER_REQUEST_TIMEOUT_SECONDS='30')
            self.envfile(name + '.env', values)
        self.call(['sh', SOURCE / 'deploy/production/create-purpose-secrets.sh', self.root])
        self.call(['python3', SOURCE / 'deploy/production/prepare-purpose-admin.py', self.root])
        for name in ['control', 'approvals']:
            password = (self.root / 'purpose-logins' / (name + '.password')).read_text()
            self.sensitive.append(password.encode())
            values = {'DATABASE_URL': dsn(name + '_login', password), **common}
            prefix = 'BROKERROUTER_' + ('HUMAN' if name == 'control' else 'APPROVALS')
            values.update({prefix + '_ISSUER': ISSUER, prefix + '_AUDIENCE': 'fixture-' + name, prefix + '_PUBLIC_KEYS': json.dumps({('human' if name == 'control' else 'approvals') + '-v1': self.pubs['human' if name == 'control' else 'approvals']})})
            self.envfile(name + '.env', values)
        self.write('provider.key', self.model_key)
        self.write('pg_hba.conf', (SOURCE / 'deploy/production/pg_hba.conf').read_bytes(), 0o444)
        self.configuration()
        self.passed('four independent fixture CAs; separate HUMAN and approvals RSA purposes; generated private Secrets stay outside evidence')

    def configuration(self):
        secrets_config = {}

        def secret(name, path):
            secrets_config[name] = {'file': str(self.root / path)}
            return name

        for name in ['pg', 'runtime', 'control', 'approvals']:
            prefix = 'pg' if name == 'pg' else name
            secret(name + '_cert', prefix + '-server.crt')
            secret(name + '_key', prefix + '-server.key')
        secret('pg_ca', 'pg-ca.crt')
        secret('migration_password', 'postgres-migration-password')
        services = {}
        services['postgres'] = {
            'image': identity.PG, 'hostname': 'postgres',
            'environment': {'POSTGRES_USER': 'brokerrouter_migration', 'POSTGRES_DB': 'brokerrouter', 'POSTGRES_PASSWORD_FILE': '/run/secrets/admin_password', 'POSTGRES_INITDB_ARGS': '--auth-host=scram-sha-256 --auth-local=trust'},
            'entrypoint': ['sh', '-ec', 'mkdir -p /tls; cp /run/secrets/server_key /tls/server.key; cp /run/secrets/server_cert /tls/server.crt; chmod 600 /tls/server.key; chown 999:999 /tls/*; exec /usr/local/bin/docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/tls/server.crt -c ssl_key_file=/tls/server.key -c hba_file=/fixture/pg_hba.conf -c password_encryption=scram-sha-256 -c log_statement=none -c log_min_duration_statement=-1 -c log_min_error_statement=panic -c max_connections=80'],
            'secrets': [{'source': 'migration_password', 'target': 'admin_password'}, {'source': 'pg_cert', 'target': 'server_cert'}, {'source': 'pg_key', 'target': 'server_key'}],
            'volumes': [str(self.root / 'pg_hba.conf') + ':/fixture/pg_hba.conf:ro'],
            'tmpfs': ['/var/lib/postgresql/data:size=384m', '/var/run/postgresql:size=8m', '/tls:size=8m', '/tmp:size=16m'],
            'read_only': True, 'networks': ['backend'], 'healthcheck': {'test': ['CMD-SHELL', 'pg_isready -U brokerrouter_migration -d brokerrouter'], 'interval': '2s', 'timeout': '2s', 'retries': 30},
            'mem_limit': '512m', 'cpus': 1, 'pids_limit': 128, 'security_opt': ['no-new-privileges:true'],
        }
        for name, purpose_name, command in [
            ('migration', 'migration', ['migrate']), ('operator', 'operator', ['--help']),
            ('gateway', 'runtime', ['serve', '--bind', '0.0.0.0:8080']),
            ('control', 'control', ['serve-control', '--bind', '0.0.0.0:8081']),
            ('approvals', 'approvals', ['serve-approvals', '--bind', '0.0.0.0:8090']),
        ]:
            secret(name + '_env', purpose_name + '.env')
            item = {'image': self.image, 'command': command, 'environment': {'BROKERROUTER_ENV_FILE': '/run/secrets/application_env', 'BROKERROUTER_DATABASE_MAX_CONNECTIONS': '2', 'RUST_LOG': 'brokerrouter=warn'}, 'secrets': ['pg_ca', {'source': name + '_env', 'target': 'application_env'}], 'networks': ['backend'], 'read_only': True, 'cap_drop': ['ALL'], 'cap_add': ['SETUID', 'SETGID'], 'security_opt': ['no-new-privileges:true'], 'tmpfs': ['/tmp:size=16m,mode=1777'], 'mem_limit': '256m', 'cpus': 1, 'pids_limit': 128}
            if name in ['migration', 'operator']:
                item['profiles'] = ['ops']
            else:
                port = command[-1].split(':')[-1]
                item['healthcheck'] = {'test': ['CMD', 'curl', '-fsS', 'http://127.0.0.1:' + port + '/health/ready'], 'interval': '2s', 'timeout': '2s', 'retries': 30}
            services[name] = item
        for name in ['runtime', 'control', 'approvals']:
            upstream, port = ('gateway', 8080) if name == 'runtime' else (name, 8081 if name == 'control' else 8090)
            # Keep the deployed TLS, proxy and Docker DNS contract; enable one fixed destination per edge.
            template = (SOURCE / 'deploy/production/control-plane-nginx.conf').read_text()
            head = template[:template.index('    server { listen 8443 ssl default_server;')]
            server = '    server { listen 8443 ssl default_server; ssl_reject_handshake on; return 421; }\n'
            server += '    server { listen 8443 ssl; server_name ' + name + '.brokerrouter.internal; if ($host != ' + name + '.brokerrouter.internal) { return 421; } error_log /dev/null; location / { set $fixed_upstream ' + upstream + ':' + str(port) + '; proxy_pass http://$fixed_upstream; } }\n'
            self.write(name + '-nginx.conf', head + server + '}\n', 0o444)
            services[name + '-tls'] = {'image': identity.NGINX, 'entrypoint': ['nginx', '-g', 'daemon off;'], 'secrets': [{'source': name + '_cert', 'target': 'tls_fullchain'}, {'source': name + '_key', 'target': 'tls_private_key'}], 'volumes': [str(self.root / (name + '-nginx.conf')) + ':/etc/nginx/nginx.conf:ro'], 'read_only': True, 'tmpfs': ['/tmp:size=16m,mode=1777'], 'cap_drop': ['ALL'], 'cap_add': ['CHOWN', 'SETUID', 'SETGID'], 'security_opt': ['no-new-privileges:true'], 'mem_limit': '128m', 'cpus': 0.5, 'pids_limit': 64, 'networks': {'backend': {'aliases': [name + '.brokerrouter.internal']}}}
        secret('provider_key', 'provider.key')
        services['provider'] = {'image': identity.BROWSER, 'entrypoint': ['node', '-e', "process.env.FIXTURE_MODEL_KEY=require('fs').readFileSync('/run/secrets/provider_key','utf8');process.setgid(10001);process.setuid(10001);require('/fixture/provider.js');"], 'secrets': ['provider_key'], 'volumes': [str(HERE / 'provider.js') + ':/fixture/provider.js:ro'], 'read_only': True, 'cap_drop': ['ALL'], 'cap_add': ['SETUID', 'SETGID'], 'security_opt': ['no-new-privileges:true'], 'mem_limit': '128m', 'cpus': 0.5, 'pids_limit': 64, 'networks': ['backend']}
        # Agent receives only the runtime trust anchor. Assertions/virtual key are supplied on stdin, not argv/env/files.
        secret('runtime_ca', 'runtime-ca.crt')
        services['agent'] = {'image': identity.BROWSER, 'user': '10001:10001', 'entrypoint': ['node', '-e', 'setInterval(()=>{},10000)'], 'secrets': [{'source': 'runtime_ca', 'target': 'ca.crt'}], 'read_only': True, 'cap_drop': ['ALL'], 'security_opt': ['no-new-privileges:true'], 'mem_limit': '128m', 'cpus': 0.5, 'pids_limit': 64, 'networks': ['backend']}
        self.write('compose.json', json.dumps({'name': PROJECT, 'services': services, 'secrets': secrets_config, 'networks': {'backend': {'internal': True}}}, indent=2))

    def start_database(self):
        self.started = True
        self.compose('config', '--quiet')
        self.compose('up', '--detach', '--wait', '--wait-timeout', '60', 'postgres', timeout=90)
        self.cli(kind='migration')
        self.admin((SOURCE / 'deploy/permissions.sql').read_text())
        self.admin('REVOKE ALL ON DATABASE brokerrouter FROM PUBLIC; GRANT CONNECT ON DATABASE brokerrouter TO brokerrouter_migration;')
        for name in ['runtime', 'operator']:
            self.admin("CREATE ROLE brokerrouter_" + name + "_login LOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD '" + purpose.scram(self.passwords[name].encode()) + "'; GRANT brokerrouter_" + name + " TO brokerrouter_" + name + "_login; GRANT CONNECT ON DATABASE brokerrouter TO brokerrouter_" + name + "_login;")
        owner = self.call(['docker', 'run', '--rm', '--network', 'none', '--mount', 'type=bind,src=' + str(self.root / 'purpose-logins') + ',dst=/purpose,readonly', '--entrypoint', 'stat', identity.PG, '-c', '%u:%g', '/purpose']).decode().strip()
        uid, gid = owner.split(':')
        self.call(['docker', 'run', '--rm', '--network', PROJECT + '_backend', '--user', owner, '--read-only', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges:true', '--tmpfs', '/run/purpose-secrets:size=1m,mode=0700,uid=' + uid + ',gid=' + gid, '--tmpfs', '/tmp:size=8m,mode=1777', '--mount', 'type=bind,src=' + str(self.root / 'purpose-logins') + ',dst=/run/purpose-secrets/purpose-logins,readonly', '--mount', 'type=bind,src=' + str(self.root / '.purpose-logins.lock') + ',dst=/run/purpose-secrets/.purpose-logins.lock', '--mount', 'type=bind,src=' + str(self.root / 'purpose-admin.service') + ',dst=/run/secrets/purpose_admin_service,readonly', '--mount', 'type=bind,src=' + str(self.root / 'purpose-admin.passfile') + ',dst=/run/secrets/purpose_admin_passfile,readonly', '--mount', 'type=bind,src=' + str(self.root / 'pg-ca.crt') + ',dst=/run/secrets/pg_ca,readonly', OPS], timeout=60)
        require(int(self.admin('SELECT count(*) FROM brokerrouter_deployment.purpose_logins;')) == 20, 'all twenty canonical purpose LOGIN registrations')
        versions = self.admin('SELECT version FROM _sqlx_migrations WHERE success ORDER BY version;').decode().split()
        require(versions == [str(n) for n in range(1, 32)], 'actual database is exactly schema31')
        self.passed('actual release migration31 and canonical grants; isolated ops registered twenty purposes; measured Secret owner with strict TLS/SCRAM')

    def seed(self):
        self.tenant = self.cli('tenant-create', '--name', 'Synthetic final publication acceptance', '--budget-cny', '10').decode().strip()
        self.cli('model-put', '--tenant', self.tenant, '--id', 'fixture', '--name', 'Zero-cost actual HTTP model')
        self.cli('endpoint-add', '--tenant', self.tenant, '--model', 'fixture', '--name', 'Local fixture only', '--upstream-model', 'local-upstream', '--base-url', 'http://provider:8099/v1', '--credential-env', 'FIXTURE_MODEL_KEY', '--input-per-million', '0', '--output-per-million', '0', '--context-tokens', '1000', '--max-output-tokens', '100', '--allow-private')
        key = json.loads(self.cli('key-create', '--tenant', self.tenant, '--name', 'Agent virtual key only', '--budget-cny', '1', '--model', 'fixture'))
        self.key, self.keyid = key['key'], key['id']
        self.sensitive.append(self.key.encode())
        self.owner = self.cli('governance-user-add', '--tenant', self.tenant, '--issuer', ISSUER, '--subject', 'synthetic-owner').decode().strip()
        self.cli('runtime-bind', '--tenant', self.tenant, '--key', self.keyid, '--user', self.owner, '--runtime', 'final-text-fixture', '--agent', 'synthetic-agent')
        self.compose('up', '--detach', 'provider', 'agent')
        self.compose('up', '--detach', '--wait', '--wait-timeout', '60', 'gateway', 'control', 'approvals', timeout=90)
        self.compose('up', '--detach', 'runtime-tls', 'control-tls', 'approvals-tls')
        for name in ['runtime', 'control', 'approvals']:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                try:
                    response = self.http('https://' + name + '.brokerrouter.internal:8443/health/ready', ca=name + '-ca.crt')
                    if response['status'] == 204:
                        break
                except RuntimeError:
                    pass
                time.sleep(0.5)
            else:
                raise RuntimeError('actual HTTPS readiness deadline: ' + name)
        response = self.human_http('/v1/governance/turns', {'key_id': self.keyid, 'session_id': 'synthetic-final-session', 'message_id': 'synthetic-final-message', 'run_id': 'synthetic-final-run', 'generation': 1, 'ttl_seconds': 1800})
        require(response['status'] == 200, 'trusted controller creates owner-bound turn')
        self.turn = json.loads(response['body'])['id']
        self.passed('three independently ready runtime/control/approvals APIs behind separate verified HTTPS; controller created the bound synthetic owner turn')

    def http(self, url, body=None, headers=None, network='backend', ca='control-ca.crt', agent=False, lose_ack=False):
        script = "let a='';process.stdin.on('data',b=>a+=b);process.stdin.on('end',()=>{const fs=require('fs'),https=require('https'),x=JSON.parse(a);let done=false;const finish=v=>{if(!done){done=true;process.stdout.write(JSON.stringify(v));}};const u=new URL(x.url);if(x.agent&&u.hostname!=='runtime.brokerrouter.internal')throw Error('Agent destination rejected');const q=https.request(u,{method:x.body===null?'GET':'POST',ca:fs.readFileSync('/run/" + ('secrets/ca.crt' if agent else 'ca.crt') + "'),headers:x.headers,timeout:15000},r=>{if(x.lose){finish({status:r.statusCode,ack_lost:true,body_consumed:false});r.destroy();return;}let b='';r.on('data',c=>b+=c);r.on('end',()=>finish({status:r.statusCode,headers:r.headers,body:b}));});q.on('error',e=>{if(!done){process.stderr.write('HTTPS transport failed '+e.code);process.exitCode=2;}});q.on('timeout',()=>q.destroy());q.end(x.body);});"
        data = json.dumps({'url': url, 'body': None if body is None else json.dumps(body, separators=(',', ':')), 'headers': headers or {}, 'agent': agent, 'lose': lose_ack}).encode()
        if agent:
            command = ['docker', 'exec', '-i', self.container('agent'), 'node', '-e', script]
        else:
            command = ['docker', 'run', '--rm', '-i', '--network', PROJECT + '_' + network, '--memory', '128m', '--cpus', '0.5', '--read-only', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges:true', '--mount', 'type=bind,src=' + str(self.root / ca) + ',dst=/run/ca.crt,readonly', '--entrypoint', 'node', identity.BROWSER, '-e', script]
        return json.loads(self.call(command, data, timeout=30))

    def human_http(self, path, body, key='human', typ=None, aud=None, kid=None):
        raw = json.dumps(body, separators=(',', ':')).encode()
        now = int(time.time())
        control = key == 'human'
        claims = {'tenant_id': self.tenant, 'iss': ISSUER, 'sub': 'synthetic-owner', 'aud': aud or ('fixture-control' if control else 'fixture-approvals'), 'exp': now + 45, 'nbf': now - 1, 'iat': now, 'auth_time': now, 'jti': str(uuid.uuid4()), 'method': 'POST', 'path': path, 'body_sha256': hashlib.sha256(raw).hexdigest(), 'interactive': True}
        encode = lambda value: base64.urlsafe_b64encode(value).rstrip(b'=')
        unsigned = encode(json.dumps({'alg': 'RS256', 'typ': typ or ('br-human+jwt' if control else 'br-approvals+jwt'), 'kid': kid or key + '-v1'}, separators=(',', ':')).encode()) + b'.' + encode(json.dumps(claims, separators=(',', ':')).encode())
        signature = self.openssl('dgst', '-sha256', '-sign', self.write('sign-' + key + '.pem', self.keys[key]), data=unsigned)
        token = (unsigned + b'.' + encode(signature)).decode()
        self.sensitive.append(token.encode())
        role = 'control' if path.startswith('/v1/governance/') else 'approvals'
        return self.http('https://' + role + '.brokerrouter.internal:8443' + path, body, {'Content-Type': 'application/json', 'Authorization': 'Bearer ' + token}, ca=role + '-ca.crt')

    def agent(self, path, body, lose_ack=False, extra_headers=None):
        return self.http('https://runtime.brokerrouter.internal:8443' + path, body, {'Content-Type': 'application/json', 'Authorization': 'Bearer ' + self.key, **(extra_headers or {})}, agent=True, lose_ack=lose_ack)

    def policy(self, version):
        self.write('rail.json', json.dumps({'id': 'final-text-review', 'version': version, 'name': 'Synthetic explicit final text review', 'stage': 'post', 'rule': {'kind': 'always'}}))
        self.write('policy.json', json.dumps({'id': getattr(self, 'policy_id', str(uuid.uuid4())), 'guardrail_id': 'final-text-review', 'model_id': 'fixture', 'key_id': self.keyid, 'runtime_id': 'final-text-fixture', 'tool_name': None, 'active': True}))
        self.policy_id = json.loads((self.root / 'policy.json').read_text())['id']
        config = json.loads((self.root / 'compose.json').read_text())
        config['services']['operator']['volumes'] = [str(self.root / 'rail.json') + ':/fixture/rail.json:ro', str(self.root / 'policy.json') + ':/fixture/policy.json:ro']
        self.write('compose.json', json.dumps(config, indent=2))
        self.cli('guardrail-publish', '--tenant', self.tenant, '--file', '/fixture/rail.json')
        if version == 1:
            self.cli('policy-put', '--tenant', self.tenant, '--file', '/fixture/policy.json')

    def approve(self, pending, expired_review_probe=False):
        approval = pending['approval']['id']
        response = self.human_http('/approvals/v1/query', {'limit': 20, 'pending_only': True, 'turn_id': self.turn}, key='approvals')
        require(response['status'] == 200, 'owner workbench inbox accessible')
        inbox = json.loads(response['body'])
        require(any(row['id'] == approval and row['stage'] == 'post' for row in inbox['data']), 'pending final post approval visible to same owner')
        require(self.final_text not in response['body'], 'inbox never contains final plaintext')
        if expired_review_probe:
            # A fixture-only BEFORE INSERT hook creates a short real append-only review receipt.
            # Existing receipts and immutable source/publication facts are never edited.
            self.admin("CREATE FUNCTION fixture_short_review() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.expires_at=LEAST(NEW.expires_at,NEW.created_at+interval '2 seconds'); RETURN NEW; END; $$; CREATE TRIGGER fixture_short_review BEFORE INSERT ON approval_reviews FOR EACH ROW EXECUTE FUNCTION fixture_short_review();")
        response = self.human_http('/approvals/v1/review', {'approval_id': approval}, key='approvals')
        if expired_review_probe:
            self.admin('DROP TRIGGER fixture_short_review ON approval_reviews; DROP FUNCTION fixture_short_review();')
        require(response['status'] == 200, 'real protected workbench review succeeds')
        review = json.loads(response['body'])
        require(review['can_continue'] is True, 'server-recognized final publication is actually reviewable')
        payload = json.loads(review['payload_text'])
        require(payload['text'] == self.final_text and payload['publication_id'] == self.publication and payload['source_request_ids'] == [self.request], 'review contains complete exact final text and real source identity')
        require(payload['session_id'] == 'synthetic-final-session' and payload['message_id'] == 'synthetic-final-message' and payload['run_id'] == 'synthetic-final-run' and payload['generation'] == 1, 'review binds native owner turn facts')
        decision = {'approval_id': approval, 'review_id': review['review_id'], 'acknowledged_scope': True, 'decision': {'decision': 'continue_this_turn', 'approved_guardrail_ids': ['final-text-review'], 'feedback': None}}
        if expired_review_probe:
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                expired = self.admin("SELECT expires_at<=clock_timestamp() FROM approval_reviews WHERE id='" + str(uuid.UUID(review['review_id'])) + "';").decode().strip()
                if expired == 't':
                    break
                time.sleep(0.2)
            require(expired == 't', 'actual immutable review receipt crosses PostgreSQL clock expiry')
            expired = self.human_http('/approvals/v1/decide', decision, key='approvals')
            require(expired['status'] == 409, 'expired actual review cannot create a decision')
            require(self.admin("SELECT state,(SELECT count(*) FROM guardrail_grants WHERE approval_id=a.id) FROM approvals a WHERE id='" + str(uuid.UUID(approval)) + "';").decode().strip() == 'pending|0', 'expired review denial leaves approval pending with zero grants')
            response = self.human_http('/approvals/v1/review', {'approval_id': approval}, key='approvals')
            require(response['status'] == 200, 'new complete review after expired review succeeds')
            review = json.loads(response['body'])
            require(review['can_continue'] is True and json.loads(review['payload_text'])['text'] == self.final_text, 'fresh review returns complete exact text')
            decision['review_id'] = review['review_id']
            self.passed('actual expired review returns409 with pending approval and zero grants; fresh complete review is required')
        response = self.human_http('/approvals/v1/decide', decision, key='approvals')
        require(response['status'] == 200, 'review-bound explicit owner decision succeeds')
        repeat = self.human_http('/approvals/v1/decide', decision, key='approvals')
        require(repeat['status'] == 200, 'same explicit decision replay is idempotent')
        row = self.admin("SELECT state,decided_by::text,(SELECT count(*) FROM guardrail_grants WHERE approval_id=a.id) FROM approvals a WHERE id='" + str(uuid.UUID(approval)) + "';").decode().strip()
        require(row == 'approved|' + self.owner + '|1', 'one real owner decision and one exact guardrail grant')
        return approval

    def stats(self):
        output = self.call(['docker', 'exec', self.container('provider'), 'node', '-e', "require('http').get('http://127.0.0.1:8099/stats',r=>{let b='';r.on('data',c=>b+=c);r.on('end',()=>process.stdout.write(b));});"])
        return json.loads(output)

    def verify(self):
        for name, wrong in [('runtime', 'control'), ('control', 'approvals'), ('approvals', 'runtime')]:
            rejected = False
            try:
                self.http('https://' + name + '.brokerrouter.internal:8443/health/ready', ca=wrong + '-ca.crt')
            except RuntimeError:
                rejected = True
            require(rejected, 'independent HTTPS purpose CA must reject other purpose leaf: ' + name)
        self.passed('actual runtime/control/approvals HTTPS succeeds with its own CA and rejects another purpose CA')
        wrong_signer = self.human_http('/v1/governance/turns', {'key_id': self.keyid, 'session_id': 'forged-purpose', 'message_id': 'synthetic', 'run_id': 'synthetic', 'generation': 1, 'ttl_seconds': 600}, key='approvals', typ='br-human+jwt', aud='fixture-control', kid='human-v1')
        require(wrong_signer['status'] == 401, 'correct HUMAN header/claims signed by approvals key must be rejected')
        self.passed('separate approvals RSA key cannot mint HUMAN turns even with correct HUMAN kid/type/audience')
        mounted = json.loads(self.call(['docker', 'inspect', self.container('agent')]))[0]
        require(len(mounted['Mounts']) == 1 and mounted['Mounts'][0]['Destination'] == '/run/secrets/ca.crt', 'Agent has only the public runtime trust-anchor mount')
        require(all('DATABASE_URL=' not in item and 'BROKERROUTER_HUMAN' not in item and 'BROKERROUTER_BFF' not in item for item in mounted['Config']['Env']), 'Agent environment has no DB, HUMAN or BFF identity')
        require(mounted['HostConfig']['ReadonlyRootfs'] and mounted['HostConfig']['CapDrop'] == ['ALL'], 'Agent is read-only and has no capabilities')
        self.passed('actual Agent container mounts only runtime CA; no controller/HUMAN/BFF key, database credential or Docker socket')
        chat = {'model': 'fixture', 'messages': [{'role': 'user', 'content': 'Synthetic local model input'}], 'max_tokens': 20, 'stream': False}
        response = self.agent('/v1/chat/completions', chat, extra_headers={'Idempotency-Key': 'final-publication-source', 'x-brokerrouter-turn-id': self.turn})
        require(response['status'] == 200, 'real model chat HTTP succeeds')
        self.request = str(uuid.UUID(response['headers']['x-brokerrouter-request-id']))
        raw = json.loads(response['body'])
        require(raw['id'] != self.request and raw['choices'][0]['message']['content'] == 'raw synthetic provider answer', 'use actual gateway request header rather than provider id')
        self.final_text = '最终中文正文\n加工后的合成结果 🐎\nUTF-8 exact text'
        self.sensitive += [self.final_text.encode(), b'Synthetic local model input', b'raw synthetic provider answer']
        self.passed('one zero-cost real HTTP provider call creates succeeded chat source under the trusted turn; gateway UUID is distinct from provider id')
        record = {'turn_id': self.turn, 'model_id': 'fixture', 'source_request_ids': [self.request], 'text': self.final_text}
        lost = self.agent('/v1/governance/final-publications', record, lose_ack=True)
        require(lost == {'status': 200, 'ack_lost': True, 'body_consumed': False}, 'discard actual record response before consuming acknowledgement body')
        response = self.agent('/v1/governance/final-publications', record)
        require(response['status'] == 200, 'exact record retry recovers metadata')
        metadata = json.loads(response['body'])
        self.publication = str(uuid.UUID(metadata['id']))
        require('text' not in metadata and metadata['text_sha256'] == hashlib.sha256(self.final_text.encode()).hexdigest() and metadata['bytes'] == len(self.final_text.encode()), 'record retry exposes only exact immutable metadata')
        changed = self.agent('/v1/governance/final-publications', {**record, 'text': self.final_text + ' replacement'})
        require(changed['status'] == 409, 'lost acknowledgement cannot replace immutable final text')
        require(self.stats()['calls'] == 1, 'record retry never repeats model')
        self.passed('record acknowledgement body lost on verified HTTPS; exact retry returns one immutable metadata record; replacement conflicts and model remains single-call')
        self.policy(1)
        path = '/v1/governance/final-publications/' + self.publication + '/release'
        response = self.agent(path, {})
        require(response['status'] == 200, 'first gated final release succeeds as metadata')
        pending = json.loads(response['body'])
        require(pending['state'] == 'waiting_approval' and 'text' not in pending and self.final_text not in response['body'], 'unapproved transformed text never released')
        denied = self.http('https://approvals.brokerrouter.internal:8443/approvals/v1/query', {'limit': 20, 'pending_only': True}, {'Content-Type': 'application/json', 'Authorization': 'Bearer ' + self.key}, ca='approvals-ca.crt')
        require(denied['status'] == 401, 'Agent virtual key cannot act as approver')
        self.passed('current output policy blocks final plaintext; actual virtual key is rejected by separate approvals API')
        first = self.approve(pending, expired_review_probe=True)
        response = self.agent(path, {})
        require(response['status'] == 200, 'approved final release HTTP succeeds')
        ready = json.loads(response['body'])
        require(ready['state'] == 'ready' and ready['text'] == self.final_text, 'first approved release has exact UTF-8 text')
        self.passed('real workbench inbox→complete review→explicit acknowledged decision grants only this turn; release returns exact transformed text')
        self.policy(2)
        response = self.agent(path, {})
        require(response['status'] == 200, 'policy-upgraded final release checks current version')
        pending = json.loads(response['body'])
        require(pending['state'] == 'waiting_approval' and 'text' not in pending and pending['approval']['id'] != first, 'version2 blocks old grant and produces new approval without text')
        self.passed('real immutable policy version2 invalidates previous approval; final retry returns only a distinct current pending approval')
        second = self.approve(pending)
        before = json.loads(self.call(['docker', 'inspect', self.container('gateway')]))[0]['State']['StartedAt']
        self.compose('restart', '--timeout', '10', 'gateway', timeout=45)
        self.compose('up', '--detach', '--wait', '--wait-timeout', '60', 'gateway', timeout=90)
        after = json.loads(self.call(['docker', 'inspect', self.container('gateway')]))[0]['State']['StartedAt']
        require(before != after, 'actual gateway process restarted')
        response = self.agent(path, {})
        require(response['status'] == 200, 'final release resumes after actual restart')
        resumed = json.loads(response['body'])
        require(resumed['state'] == 'ready' and resumed['text'] == self.final_text and resumed['output_operation_id'] == ready['output_operation_id'], 'restart releases exact stored text from same output operation after current approval')
        require(self.stats()['calls'] == 1, 'real restart/recovery never repeats provider execution')
        counts = self.admin("SELECT (SELECT count(*) FROM requests),(SELECT count(*) FROM runtime_final_publications),(SELECT count(*) FROM approvals),(SELECT count(*) FROM guardrail_grants),(SELECT count(*) FROM ledger_entries WHERE request_id='" + self.request + "' AND kind='finalize');").decode().strip()
        require(counts == '1|1|2|2|1', 'one actual model request, one publication, two version-bound approvals/grants, one final settlement')
        self.passed('gateway process truly restarted; exact publication resumes with policy2 grant and same output operation; provider call/request/publication/settlement remain once')
        pools = self.admin("SELECT a.usename,count(*),bool_and(s.ssl) FROM pg_stat_activity a JOIN pg_stat_ssl s USING(pid) WHERE a.usename LIKE 'brokerrouter_%_login' GROUP BY a.usename ORDER BY a.usename;").decode().splitlines()
        require({line.split('|')[0] for line in pools} == {'brokerrouter_runtime_login', 'brokerrouter_control_login', 'brokerrouter_approvals_login'}, 'only three intended application DB identities are active')
        require(all(int(line.split('|')[1]) <= 2 and line.split('|')[2] == 't' for line in pools), 'every actual workload connection uses TLS and pool<=2')
        self.passed('three purpose-specific runtime/control/approvals LOGIN pools stay <=2 and use actual PostgreSQL TLS')
        self.logs_safe()
        return {'gateway_request_id': self.request, 'publication_id': self.publication, 'approval_ids': [first, second], 'provider_calls': 1, 'counts': {'requests': 1, 'publications': 1, 'approvals': 2, 'grants': 2, 'finalize_ledger_entries': 1}, 'text_sha256': hashlib.sha256(self.final_text.encode()).hexdigest(), 'text_bytes': len(self.final_text.encode())}

    def logs_safe(self):
        logs = self.compose('logs', '--no-color', timeout=30)
        for value in self.sensitive:
            require(value not in logs, 'generated secret or synthetic request/final body appeared in service logs')
        (EVIDENCE / 'r1-final-publications-services.log').write_bytes(logs)
        self.passed('service logs omit generated credentials, signed assertions, request input and provider/final plaintext')


def main():
    global SOURCE, EVIDENCE, purpose, OPS
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', required=True, help='immutable fixed schema31 release image ID')
    parser.add_argument('--source', required=True, type=Path, help='matching frozen migrations/canonical permissions/production helper source')
    parser.add_argument('--evidence-dir', required=True, type=Path)
    parser.add_argument('--provisioner-image', default=OPS)
    args = parser.parse_args()
    os.umask(0o077)
    SOURCE = args.source.resolve()
    EVIDENCE = args.evidence_dir.resolve()
    require(not EVIDENCE.is_relative_to(REPO), 'evidence must be external to repository')
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    require(not any(EVIDENCE.glob('r1-final-publications-*')), 'use a fresh evidence directory; previous red/green evidence must remain immutable')
    for reference in [args.image, args.provisioner_image, identity.PG, identity.BROWSER, identity.NGINX]:
        require(re.fullmatch(r'(?:[A-Za-z0-9._/:-]+@)?sha256:[0-9a-f]{64}', reference), 'immutable image reference required')
    require(len(list((SOURCE / 'migrations').glob('*.sql'))) == 31, 'matching source must contain exactly31 migrations')
    required = ['deploy/permissions.sql', 'deploy/production/purpose-logins.py', 'deploy/production/create-purpose-secrets.sh', 'deploy/production/prepare-purpose-admin.py', 'deploy/production/pg_hba.conf', 'deploy/production/control-plane-nginx.conf']
    for name in required:
        require((SOURCE / name).is_file(), 'missing matching frozen deployment input: ' + name)
    identity.PROJECT = PROJECT
    OPS = args.provisioner_image
    module = importlib.util.spec_from_file_location('purpose', SOURCE / 'deploy/production/purpose-logins.py')
    purpose = importlib.util.module_from_spec(module)
    module.loader.exec_module(purpose)
    existing = subprocess.run(['docker', 'ps', '-aq', '--filter', 'label=com.docker.compose.project=' + PROJECT], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)
    require(not existing.stdout.strip(), 'refusing to reuse existing project')
    root = Path(tempfile.mkdtemp(prefix='brokerrouter-final-text-secrets-', dir='/private/tmp')).resolve()
    root.rmdir()
    fixture = Fixture(args.image, root)
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
            (EVIDENCE / 'r1-final-publications-failure-services.log').write_bytes(logs)
        raise
    finally:
        fixture.close()
    remaining = subprocess.run(['docker', 'ps', '-aq', '--filter', 'label=com.docker.compose.project=' + PROJECT], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)
    require(not remaining.stdout.strip() and not root.exists(), 'own project and generated credentials cleaned')
    fixture.passed('all own test containers/networks and generated private credentials removed; evidence and frozen source preserved')
    images = {}
    for name, reference in [('core', args.image), ('provisioner', OPS), ('postgres', identity.PG), ('node_probe', identity.BROWSER), ('nginx', identity.NGINX)]:
        raw = json.loads(fixture.call(['docker', 'image', 'inspect', reference]))[0]
        images[name] = {'reference': reference, 'id': raw['Id'], 'revision': (raw['Config'].get('Labels') or {}).get('org.opencontainers.image.revision')}
    paths = sorted([*[(SOURCE / name) for name in required], *(SOURCE / 'migrations').glob('*.sql')])
    source_hashes = {str(path.relative_to(SOURCE)): hashlib.sha256(path.read_bytes()).hexdigest() for path in paths}
    own_hashes = {name: hashlib.sha256((HERE / name).read_bytes()).hexdigest() for name in ['acceptance.py', 'provider.js']}
    own_hashes['../identity/acceptance.py'] = hashlib.sha256((HERE.parent / 'identity/acceptance.py').read_bytes()).hexdigest()
    report = {'result': 'passed', 'groups': len(fixture.facts), 'facts': fixture.facts, 'schema_version': 31, 'purpose_registry_count': 20, 'synthetic_identity': True, 'real_external_idp_verified': False, 'native_hermes_history_verified': False, 'full_control_plane_verified': False, 'model_provider': 'isolated_zero_cost_real_HTTP_fixture', 'images': images, 'source_sha256': source_hashes, 'test_source_sha256': own_hashes, 'roundtrip': result, 'own_cleanup_verified': True, 'duration_seconds': round(time.monotonic() - started, 2), 'logs_sha256': {'r1-final-publications-services.log': hashlib.sha256((EVIDENCE / 'r1-final-publications-services.log').read_bytes()).hexdigest()}}
    (EVIDENCE / 'r1-final-publications-manifest.json').write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
    print('FINAL: ' + json.dumps({'result': 'passed', 'groups': len(fixture.facts), 'schema_version': 31, 'provider_calls': 1, 'own_cleanup_verified': True}), flush=True)


if __name__ == '__main__':
    main()
