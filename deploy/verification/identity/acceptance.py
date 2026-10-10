#!/usr/bin/env python3
"""Isolated actual OIDC/BFF/Console own-approval acceptance; no production identities."""
import argparse, base64, hashlib, importlib.util, json, os, re, secrets, shlex, subprocess, tempfile, time, uuid
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
FROZEN = None
HERE = Path(__file__).resolve().parent
EVIDENCE = None
NODE_MODULES = None
PROJECT = 'brokerrouter-identity-test-' + secrets.token_hex(6)
PG = 'postgres@sha256:67f41722b7a8cbdb868a44a4995c846eddfdc2973bccb291ce937dce88ad5675'
KC = 'brokerrouter-local-keycloak@sha256:ab348b253b741ff2d289067b5bf05225eded24f578e113d4c036eb16819fe592'
BROWSER = 'brokerrouter-local-identity-browser@sha256:595490e76f3a0db1125d1846f84253c506ac219140c0fb82f00e8cc5582854a6'
CONSOLE = 'brokerrouter-console@sha256:4634ef080af44903ca89c0e16cd277c0201d2305772f2219ba08d46e2af3c332'
NGINX = 'nginx@sha256:dc5069ad14f19660b141b21236140b91656bf89bbc3e2417c70ae650cd66104c'
OPS = 'brokerrouter-provisioner@sha256:9e1e315101845437d5b589795d60cfc8449ca3a06110b8f3e2e83a85c3624a06'
PUBLIC = 'https://console.fixture.invalid'
ISSUER = 'https://identity.fixture.invalid/realms/brokerrouter-fixture'
purpose = None

class Fixture:
    def __init__(self, image, secret_dir):
        self.image=image; self.project=PROJECT; self.root=secret_dir; self.root.mkdir(mode=0o700)
        self.transcript=[]; self.started=False; self.sensitive=[]; self.check=None
        self.users={name: {'id':str(uuid.uuid4()), 'username': 'synthetic-'+name, 'password':secrets.token_urlsafe(30)} for name in ['owner','other','unbound']}
        self.client_secret=secrets.token_hex(32); self.sensitive.append(self.client_secret.encode())
        self.passwords={n:secrets.token_hex(32) for n in ['migration','runtime','operator','idp-admin','idp']}
        self.sensitive += [v.encode() for v in self.passwords.values()] + [u['password'].encode() for u in self.users.values()]
    def save(self,stage):
        names=['project','image','users','client_secret','passwords','keys','pubs','result','bffkey','tenant','other_tenant','bindings','owners','key','keyid','turn','check','approval']
        self.write('fixture-state.json',json.dumps({'stage':stage,'state':{n:getattr(self,n) for n in names if hasattr(self,n)}}))
    @classmethod
    def restore(cls,root,image):
        f=object.__new__(cls); f.root=root; f.transcript=[]; f.started=True; f.sensitive=[]
        raw=json.loads((root/'fixture-state.json').read_text())
        for n,v in raw['state'].items(): setattr(f,n,v)
        global PROJECT
        PROJECT=f.project
        f.image=image
        f.sensitive += [v.encode() for v in f.passwords.values()]+[u['password'].encode() for u in f.users.values()]+[v.encode() for v in f.keys.values()]+[f.client_secret.encode(),f.result.encode(),f.bffkey.encode()]
        if hasattr(f,'key'): f.sensitive.append(f.key.encode())
        return f,raw['stage']
    def call(self, args, data=None, expect=0, timeout=90):
        p=subprocess.run([str(v) for v in args], input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
        self.transcript += [p.stdout,p.stderr]
        if p.returncode != expect:
            if any(str(a).startswith('IDENTITY_MODE=') for a in args):
                safe=p.stdout+p.stderr
                for v in self.sensitive: safe=safe.replace(v,b'[REDACTED]')
                print(safe.decode(errors='replace'),end='',flush=True)
            states=re.findall(rb'SQLSTATE=([A-Z0-9]{5})',p.stderr)
            raise RuntimeError('command failed: '+str(args[:3])+', rc='+str(p.returncode)+', SQLSTATE='+(states[-1].decode() if states else 'unavailable'))
        return p.stdout
    def write(self,name,value,mode=0o600):
        p=self.root/name; p.parent.mkdir(mode=0o700,parents=True,exist_ok=True)
        p.write_bytes(value if isinstance(value,bytes) else value.encode()); p.chmod(mode); return p
    def openssl(self,*args,data=None): return self.call(['openssl',*args],data)
    def ca(self,name):
        self.openssl('req','-x509','-newkey','rsa:2048','-nodes','-days','14','-subj','/CN=Task-only '+name+' CA','-addext','basicConstraints=critical,CA:TRUE','-keyout',self.root/(name+'.key'),'-out',self.root/(name+'.crt'))
        (self.root/(name+'.key')).chmod(0o600); (self.root/(name+'.crt')).chmod(0o444)
    def leaf(self,name,ca,hosts):
        self.openssl('req','-new','-newkey','rsa:2048','-nodes','-subj','/CN='+hosts[0],'-keyout',self.root/(name+'.key'),'-out',self.root/(name+'.csr'))
        self.write(name+'.ext','subjectAltName='+','.join('DNS:'+v for v in hosts)+'\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n')
        self.openssl('x509','-req','-days','14','-in',self.root/(name+'.csr'),'-CA',self.root/(ca+'.crt'),'-CAkey',self.root/(ca+'.key'),'-CAcreateserial','-extfile',self.root/(name+'.ext'),'-out',self.root/(name+'.crt'))
        (self.root/(name+'.key')).chmod(0o600); (self.root/(name+'.crt')).chmod(0o444)
    def envfile(self,name,values):
        return self.write(name, ''.join(k+'='+shlex.quote(v)+'\n' for k,v in values.items()))
    def compose(self,*args,data=None,expect=0,timeout=90):
        return self.call(['docker','compose','--project-name',PROJECT,'--file',self.root/'compose.json',*args],data,expect,timeout)
    def container(self,service): return self.compose('ps','--quiet',service).decode().strip()
    def admin(self,sql,idp=False):
        return self.call(['docker','exec','-i',self.container('idp-postgres' if idp else 'postgres'),'psql','-U','keycloak_bootstrap' if idp else 'brokerrouter_migration','-d','keycloak' if idp else 'brokerrouter','-X','-q','-A','-t','--set=ON_ERROR_STOP=1'],sql.encode())
    def cli(self,*args,kind='operator'):
        return self.compose('run','--rm','--no-deps',kind,*args)
    def setup(self):
        for name in ['pg-ca','idp-pg-ca','web-ca','control-ca']: self.ca(name)
        for name,ca,hosts in [('pg-server','pg-ca',['postgres']),('idp-pg-server','idp-pg-ca',['idp-postgres']),('idp','web-ca',['identity.fixture.invalid']),('console','web-ca',['console.fixture.invalid']),('control','control-ca',['control.brokerrouter.internal','console.brokerrouter.internal','approvals.brokerrouter.internal','runtime.brokerrouter.internal'])]: self.leaf(name,ca,hosts)
        self.write('bff-ca.crt',(self.root/'web-ca.crt').read_bytes()+(self.root/'control-ca.crt').read_bytes(),0o444)
        self.keys={n:self.openssl('genpkey','-algorithm','RSA','-pkeyopt','rsa_keygen_bits:2048').decode() for n in ['human','console','approvals']}
        self.pubs={n:self.openssl('pkey','-pubout',data=v.encode()).decode() for n,v in self.keys.items()}
        self.write('human.private.pem',self.keys['human'])
        self.sensitive += [k.encode() for k in self.keys.values()]
        self.result=secrets.token_hex(32); self.bffkey=secrets.token_hex(32); self.sensitive += [self.result.encode(),self.bffkey.encode()]
        common={'BROKERROUTER_ACTIVE_RESULT_KEY':'result-v1','BROKERROUTER_RESULT_KEYS':json.dumps({'result-v1':self.result})}
        def dsn(user,pw): return 'postgres://'+user+':'+pw+'@postgres:5432/brokerrouter?sslmode=verify-full&sslrootcert=/run/secrets/pg_ca'
        for n in ['migration','runtime','operator']:
            self.write('postgres-'+n+'-password',self.passwords[n])
            self.envfile(n+'.env', {'DATABASE_URL':dsn('brokerrouter_migration' if n=='migration' else 'brokerrouter_'+n+'_login',self.passwords[n]),**(common if n=='runtime' else {})})
        self.call(['sh',FROZEN/'deploy/production/create-purpose-secrets.sh',self.root])
        self.call(['python3',FROZEN/'deploy/production/prepare-purpose-admin.py',self.root])
        for n in ['control','console','approvals','bff']:
            pw=(self.root/'purpose-logins'/(n+'.password')).read_text(); self.sensitive.append(pw.encode())
            values={'DATABASE_URL':dsn('brokerrouter_'+n+'_login',pw)}
            if n=='control': values.update(common, BROKERROUTER_HUMAN_ISSUER=PUBLIC,BROKERROUTER_HUMAN_AUDIENCE='fixture-human',BROKERROUTER_HUMAN_PUBLIC_KEYS=json.dumps({'human-v1':self.pubs['human']}))
            if n in ['console','approvals']:
                prefix='BROKERROUTER_'+n.upper(); values.update({prefix+'_ISSUER':PUBLIC,prefix+'_AUDIENCE':'fixture-'+n,prefix+'_PUBLIC_KEYS':json.dumps({n+'-v1':self.pubs[n]})})
                if n=='approvals': values.update(common)
            if n=='bff': values.update(BROKERROUTER_BFF_ORIGIN=PUBLIC,BROKERROUTER_OIDC_ISSUER=ISSUER,BROKERROUTER_OIDC_ALLOWED_ORIGINS=json.dumps(['https://identity.fixture.invalid']),BROKERROUTER_OIDC_CLIENT_ID='brokerrouter-fixture',BROKERROUTER_OIDC_CLIENT_SECRET=self.client_secret,BROKERROUTER_BFF_CONSOLE_ORIGIN='https://console.brokerrouter.internal:8443',BROKERROUTER_CONSOLE_AUDIENCE='fixture-console',BROKERROUTER_BFF_SIGNING_KID='console-v1',BROKERROUTER_BFF_SIGNING_PRIVATE_KEY=self.keys['console'],BROKERROUTER_BFF_KEYS=json.dumps({'bff-v1':self.bffkey}),BROKERROUTER_BFF_ACTIVE_KEY='bff-v1',BROKERROUTER_BFF_APPROVALS_ORIGIN='https://approvals.brokerrouter.internal:8443',BROKERROUTER_APPROVALS_AUDIENCE='fixture-approvals',BROKERROUTER_BFF_APPROVALS_SIGNING_KID='approvals-v1',BROKERROUTER_BFF_APPROVALS_SIGNING_PRIVATE_KEY=self.keys['approvals'])
            self.envfile(n+'.env',values)
        realm={'realm':'brokerrouter-fixture','enabled':True,'sslRequired':'all','registrationAllowed':False,'resetPasswordAllowed':False,'rememberMe':False,'loginWithEmailAllowed':False,'duplicateEmailsAllowed':False,'defaultSignatureAlgorithm':'RS256','clients':[{'clientId':'brokerrouter-fixture','enabled':True,'publicClient':False,'secret':self.client_secret,'protocol':'openid-connect','standardFlowEnabled':True,'directAccessGrantsEnabled':False,'implicitFlowEnabled':False,'serviceAccountsEnabled':False,'defaultClientScopes':['basic','profile','email','roles','web-origins','acr'],'optionalClientScopes':[],'redirectUris':[PUBLIC+'/bff/v1/callback'],'webOrigins':[PUBLIC],'attributes':{'pkce.code.challenge.method':'S256','id.token.signed.response.alg':'RS256'}}], 'users':[{'id':u['id'],'username':u['username'],'enabled':True,'emailVerified':True,'email':u['username']+'@fixture.invalid','firstName':'Synthetic','lastName':name,'credentials':[{'type':'password','value':u['password'],'temporary':False}]} for name,u in self.users.items()]}
        self.write('realm.json',json.dumps(realm))
        self.write('idp-admin-password',self.passwords['idp-admin'])
        self.write('keycloak.conf','db=postgres\ndb-url=jdbc:postgresql://idp-postgres:5432/keycloak?sslmode=verify-full&sslrootcert=/work/idp-pg-ca.crt\ndb-username=keycloak_login\ndb-pool-initial-size=2\ndb-pool-min-size=2\ndb-pool-max-size=4\ndb-password='+self.passwords['idp']+'\nhostname=https://identity.fixture.invalid\nhostname-strict=true\nhttps-port=443\nhttp-enabled=false\nhttps-certificate-file=/work/idp.crt\nhttps-certificate-key-file=/work/idp.key\nhealth-enabled=true\ncache=local\nlog-level=warn\n')
        self.write('pg_hba.conf',(REPO/'deploy/production/pg_hba.conf').read_bytes(),0o444)
        self.nginx()
        self.configuration()
        self.save('setup')
        print('PASS: protected new fixture Secret set, four independent CAs, three distinct purpose RSA keys, actual confidential OIDC realm',flush=True)
    def nginx(self):
        self.call(['python3',REPO/'deploy/production/render-control-edge.py',PUBLIC,'https://review.fixture-media.invalid',self.root])
        config=(self.root/'control-edge/console-nginx.conf').read_text().replace('listen 8443','listen 443')
        self.write('console-nginx.conf',config,0o444)
        src=(REPO/'deploy/production/control-plane-nginx.conf').read_text()
        # Preserve exact TLS/host/proxy controls; deploy only the four APIs in this fixture.
        head=src[:src.index('    server { listen 8443 ssl default_server;')]
        servers='    server { listen 8443 ssl default_server; ssl_reject_handshake on; return 421; }\n'
        for purpose_name,port in [('control',8081),('console',8086),('approvals',8090),('runtime',8080)]:
            servers += '    server { listen 8443 ssl; server_name '+purpose_name+'.brokerrouter.internal; if ($host != '+purpose_name+'.brokerrouter.internal) { return 421; } error_log /dev/null; location / { set $fixed_upstream '+('gateway' if purpose_name=='runtime' else purpose_name)+':'+str(port)+'; proxy_pass http://$fixed_upstream; } }\n'
        self.write('control-nginx.conf',head+servers+'}\n',0o444)
    def configuration(self):
        sec={}
        def secret(name,path): sec[name]={'file':str(self.root/path)}; return name
        for n in ['pg-ca','pg-server','idp-pg-ca','idp-pg-server','idp','console','control']:
            secret(n+'_cert',n+'.crt')
            if n not in ['pg-ca','idp-pg-ca']: secret(n+'_key',n+'.key')
        for name,path in [('migration-password','postgres-migration-password'),('idp-admin-password','idp-admin-password'),('kc-config','keycloak.conf'),('realm','realm.json'),('bff-ca','bff-ca.crt'),('pg_ca','pg-ca.crt'),('purpose-admin-service','purpose-admin.service'),('purpose-admin-passfile','purpose-admin.passfile')]: secret(name,path)
        services={}
        for name in ['postgres','idp-postgres']:
            idp=name=='idp-postgres'; prefix='idp-pg' if idp else 'pg'
            services[name]={'image':PG,'hostname':name,'environment':{'POSTGRES_USER':'keycloak_bootstrap' if idp else 'brokerrouter_migration','POSTGRES_DB':'keycloak' if idp else 'brokerrouter','POSTGRES_PASSWORD_FILE':'/run/secrets/admin_password','POSTGRES_INITDB_ARGS':'--auth-host=scram-sha-256 --auth-local=trust'},'entrypoint':['sh','-ec','mkdir -p /tls; cp /run/secrets/server_key /tls/server.key; cp /run/secrets/server_cert /tls/server.crt; chmod 600 /tls/server.key; chown 999:999 /tls/*; exec /usr/local/bin/docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/tls/server.crt -c ssl_key_file=/tls/server.key -c hba_file=/fixture/pg_hba.conf -c password_encryption=scram-sha-256 -c log_statement=none -c log_min_duration_statement=-1 -c log_min_error_statement=panic -c max_connections=80'], 'secrets':[{'source':'idp-admin-password' if idp else 'migration-password','target':'admin_password'},{'source':prefix+'-server_cert','target':'server_cert'},{'source':prefix+'-server_key','target':'server_key'}], 'volumes':[str(self.root/'pg_hba.conf')+':/fixture/pg_hba.conf:ro'],'tmpfs':['/var/lib/postgresql/data:size=384m','/var/run/postgresql:size=8m','/tls:size=8m','/tmp:size=16m'],'read_only':True,'networks':['idp-data' if idp else 'backend'],'healthcheck':{'test':['CMD-SHELL','pg_isready -U '+('keycloak_bootstrap -d keycloak' if idp else 'brokerrouter_migration -d brokerrouter')],'interval':'2s','timeout':'2s','retries':30},'mem_limit':'512m','cpus':1,'pids_limit':128,'security_opt':['no-new-privileges:true']}
        apps={'migration':('migration',['migrate']), 'operator':('operator',['--help']), 'gateway':('runtime',['serve','--bind','0.0.0.0:8080']), 'control':('control',['serve-control','--bind','0.0.0.0:8081']), 'console':('console',['serve-console','--bind','0.0.0.0:8086']), 'approvals':('approvals',['serve-approvals','--bind','0.0.0.0:8090']), 'bff':('bff',['serve-bff','--bind','0.0.0.0:8087'])}
        for name,(purpose_name,cmd) in apps.items():
            secret(name+'_env',purpose_name+'.env')
            item={'image':self.image,'command':cmd,'environment':{'BROKERROUTER_ENV_FILE':'/run/secrets/application_env','BROKERROUTER_DATABASE_MAX_CONNECTIONS':'2','RUST_LOG':'brokerrouter=warn'},'secrets':['pg_ca',{'source':name+'_env','target':'application_env'}], 'networks':['backend'],'read_only':True,'cap_drop':['ALL'],'cap_add':['SETUID','SETGID'],'security_opt':['no-new-privileges:true'],'tmpfs':['/tmp:size=16m,mode=1777'],'mem_limit':'256m','cpus':1,'pids_limit':128,'volumes':[str(self.root)+':/fixture:ro'] if name=='operator' else []}
            # Operator mounts synthetic seed files only; remove broad Secret tree before operations.
            if name=='operator': item['volumes']=[]
            if name in ['migration','operator']: item['profiles']=['ops']
            else:
                port=int(cmd[-1].split(':')[-1]); item['healthcheck']={'test':['CMD','curl','-fsS','http://127.0.0.1:'+str(port)+'/health/ready'],'interval':'2s','timeout':'2s','retries':30}
            if name=='bff':
                item['secrets'].append({'source':'bff-ca','target':'bff_ca'}); item['environment']['BROKERROUTER_BFF_CA_FILE']='/run/secrets/bff_ca'; item['networks'].append('identity')
            services[name]=item
        kc_start='mkdir -p /work /opt/keycloak/data/import; cp /run/secrets/kc_config /work/keycloak.conf; cp /run/secrets/idp_key /work/idp.key; cp /run/secrets/idp_cert /work/idp.crt; cp /run/secrets/idp_pg_ca /work/idp-pg-ca.crt; cp /run/secrets/realm /opt/keycloak/data/import/brokerrouter-fixture-realm.json; chmod 600 /work/* /opt/keycloak/data/import/*; chown -R 1000:0 /work /opt/keycloak/data; exec /usr/sbin/chroot --userspec=1000:0 / /opt/keycloak/bin/kc.sh --config-file /work/keycloak.conf start --optimized --import-realm'
        services['keycloak']={'image':KC,'user':'0:0','entrypoint':['/bin/bash','-ec',kc_start],'secrets':[{'source':'kc-config','target':'kc_config'},{'source':'realm','target':'realm'},{'source':'idp_key','target':'idp_key'},{'source':'idp_cert','target':'idp_cert'},{'source':'idp-pg-ca_cert','target':'idp_pg_ca'}], 'read_only':True,'cap_drop':['ALL'],'cap_add':['CHOWN','SETUID','SETGID','SYS_CHROOT'],'security_opt':['no-new-privileges:true'],'tmpfs':['/work:size=8m,mode=0700','/opt/keycloak/data:size=64m,uid=1000,gid=0','/tmp:size=64m,mode=1777'],'environment':{'JAVA_OPTS_KC_HEAP':'-Xms128m -Xmx512m'},'networks':{'identity':{'aliases':['identity.fixture.invalid']},'idp-data':{}},'mem_limit':'1g','cpus':1,'pids_limit':256}
        for name in ['control-tls','console-edge']:
            public=name=='console-edge'; prefix='console' if public else 'control'
            services[name]={'image':CONSOLE if public else NGINX,'entrypoint':['nginx','-g','daemon off;'],'secrets':[{'source':prefix+'_cert','target':'tls_fullchain'},{'source':prefix+'_key','target':'tls_private_key'}], 'volumes':[str(self.root/(prefix+'-nginx.conf'))+':/etc/nginx/nginx.conf:ro'], 'read_only':True,'tmpfs':['/tmp:size=16m,mode=1777'],'cap_drop':['ALL'],'cap_add':['CHOWN','SETUID','SETGID'],'security_opt':['no-new-privileges:true'],'mem_limit':'128m','cpus':0.5,'pids_limit':64,'networks':{'backend':{},**({'identity':{'aliases':['console.fixture.invalid']}} if public else {'backend':{'aliases':[n+'.brokerrouter.internal' for n in ['control','console','approvals','runtime']]}})}}
        # Fixture JSON contains paths only; values and credentials stay in external 0600 Secrets.
        self.write('compose.json',json.dumps({'name':PROJECT,'services':services,'secrets':sec,'networks':{'backend':{'internal':True},'identity':{},'idp-data':{'internal':True}}},indent=2))
    def start_idp(self):
        self.started=True; self.compose('up','--detach','--wait','--wait-timeout','60','idp-postgres',timeout=90)
        verifier=purpose.scram(self.passwords['idp'].encode())
        self.admin("CREATE ROLE keycloak_login LOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS CONNECTION LIMIT 10 PASSWORD '"+verifier+"'; REVOKE ALL ON DATABASE keycloak FROM PUBLIC; GRANT CONNECT ON DATABASE keycloak TO keycloak_login; ALTER DATABASE keycloak OWNER TO keycloak_login; REVOKE CREATE ON SCHEMA public FROM PUBLIC; GRANT USAGE,CREATE ON SCHEMA public TO keycloak_login;",idp=True)
        self.compose('up','--detach','keycloak')
        deadline=time.monotonic()+90
        while time.monotonic()<deadline:
            try:
                r=self.http(ISSUER+'/.well-known/openid-configuration',network='identity',ca='web-ca.crt')
                if r['status']==200:
                    doc=json.loads(r['body']); assert doc['issuer']==ISSUER and doc['token_endpoint'].startswith(ISSUER+'/'); break
            except (RuntimeError,ValueError): pass
            time.sleep(1)
        else: raise RuntimeError('production-mode Keycloak HTTPS discovery readiness deadline')
        self.save('idp')
        print('PASS: Keycloak actual HTTPS discovery, optimized production mode, isolated non-superuser IdP DB TLS/SCRAM',flush=True)
    def http(self,url,body=None,headers=None,network='backend',ca='control-ca.crt'):
        script="let a='';process.stdin.on('data',b=>a+=b);process.stdin.on('end',()=>{const fs=require('fs'),https=require('https');const x=JSON.parse(a),u=new URL(x.url);let q=https.request(u,{method:x.body===null?'GET':'POST',ca:fs.readFileSync('/run/ca.crt'),headers:x.headers||{},timeout:15000},r=>{let b='';r.on('data',c=>b+=c);r.on('end',()=>process.stdout.write(JSON.stringify({status:r.statusCode,body:b})));});q.on('error',e=>{process.stderr.write('HTTPS transport failed '+e.code);process.exitCode=2;});q.on('timeout',()=>q.destroy());q.end(x.body);});"
        data=json.dumps({'url':url,'body':None if body is None else json.dumps(body,separators=(',',':')),'headers':headers or {}}).encode()
        return json.loads(self.call(['docker','run','--rm','-i','--network',PROJECT+'_'+network,'--memory','128m','--cpus','0.5','--read-only','--cap-drop','ALL','--mount','type=bind,src='+str(self.root/ca)+',dst=/run/ca.crt,readonly','--entrypoint','node',BROWSER,'-e',script],data,timeout=30))
    def start_apps(self):
        self.compose('up','--detach','--wait','--wait-timeout','60','postgres',timeout=90)
        self.cli(kind='migration')
        self.admin((FROZEN/'deploy/permissions.sql').read_text())
        self.admin('REVOKE ALL ON DATABASE brokerrouter FROM PUBLIC; GRANT CONNECT ON DATABASE brokerrouter TO brokerrouter_migration;')
        for name in ['runtime','operator']:
            self.admin("CREATE ROLE brokerrouter_"+name+"_login LOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD '"+purpose.scram(self.passwords[name].encode())+"'; GRANT brokerrouter_"+name+" TO brokerrouter_"+name+"_login; GRANT CONNECT ON DATABASE brokerrouter TO brokerrouter_"+name+"_login;")
        # Detect the actual Docker Desktop mount UID rather than assuming host UID translation.
        stat=self.call(['docker','run','--rm','--network','none','--mount','type=bind,src='+str(self.root/'purpose-logins')+',dst=/purpose,readonly','--entrypoint','stat',PG,'-c','%u:%g','/purpose']).decode().strip()
        uid,gid=stat.split(':')
        self.call(['docker','run','--rm','--network',PROJECT+'_backend','--user',stat,'--read-only','--cap-drop','ALL','--security-opt','no-new-privileges:true','--tmpfs','/run/purpose-secrets:size=1m,mode=0700,uid='+uid+',gid='+gid,'--tmpfs','/tmp:size=8m,mode=1777','--mount','type=bind,src='+str(self.root/'purpose-logins')+',dst=/run/purpose-secrets/purpose-logins,readonly','--mount','type=bind,src='+str(self.root/'.purpose-logins.lock')+',dst=/run/purpose-secrets/.purpose-logins.lock','--mount','type=bind,src='+str(self.root/'purpose-admin.service')+',dst=/run/secrets/purpose_admin_service,readonly','--mount','type=bind,src='+str(self.root/'purpose-admin.passfile')+',dst=/run/secrets/purpose_admin_passfile,readonly','--mount','type=bind,src='+str(self.root/'pg-ca.crt')+',dst=/run/secrets/pg_ca,readonly',OPS],timeout=45)
        assert int(self.admin('SELECT count(*) FROM brokerrouter_deployment.purpose_logins;'))==19
        self.compose('up','--detach','--wait','--wait-timeout','60','gateway','control','console','approvals','bff',timeout=90)
        self.compose('up','--detach','control-tls','console-edge')
        self.save('apps')
        print('PASS: actual migrate, canonical grants, 19 purpose LOGIN ops registration, five independently ready runtime/control purposes',flush=True)
    def seed(self):
        self.tenant=self.cli('tenant-create','--name','Synthetic OIDC acceptance','--budget-cny','10').decode().strip()
        self.other_tenant=self.cli('tenant-create','--name','Synthetic different tenant','--budget-cny','1').decode().strip()
        self.cli('model-put','--tenant',self.tenant,'--id','fixture','--name','No paid vendor')
        key=json.loads(self.cli('key-create','--tenant',self.tenant,'--name','Synthetic runtime only','--budget-cny','1','--model','fixture')); self.key=key['key']; self.keyid=key['id']; self.sensitive.append(self.key.encode())
        self.bindings={}; self.owners={}
        for name in ['owner','other']:
            user=self.cli('governance-user-add','--tenant',self.tenant,'--issuer',PUBLIC,'--subject','internal-'+name).decode().strip(); self.owners[name]=user
            binding=self.cli('bff-identity-bind','--tenant',self.tenant,'--user',user,'--issuer',ISSUER,'--subject',self.users[name]['id']).decode().strip(); self.bindings[name]=binding
            self.cli('console-permission-set','--tenant',self.tenant,'--user',user,'--permission','usage_read','--enabled','true','--reason','Synthetic local acceptance')
        self.cli('runtime-bind','--tenant',self.tenant,'--key',self.keyid,'--user',self.owners['owner'],'--runtime','fixture-runtime','--agent','fixture-agent')
        # Operator receives only these two non-secret input files as dedicated binds.
        self.write('rail.json',json.dumps({'id':'identity-review','version':1,'name':'Synthetic identity approval','stage':'tool_input','rule':{'kind':'always'}}))
        self.write('policy.json',json.dumps({'id':str(uuid.uuid4()),'guardrail_id':'identity-review','model_id':None,'key_id':self.keyid,'runtime_id':'fixture-runtime','tool_name':'fixture-tool','active':True}))
        config=json.loads((self.root/'compose.json').read_text()); config['services']['operator']['volumes']=[str(self.root/'rail.json')+':/fixture/rail.json:ro',str(self.root/'policy.json')+':/fixture/policy.json:ro']; self.write('compose.json',json.dumps(config,indent=2))
        self.cli('guardrail-publish','--tenant',self.tenant,'--file','/fixture/rail.json'); self.cli('policy-put','--tenant',self.tenant,'--file','/fixture/policy.json')
        turn={'key_id':self.keyid,'session_id':'synthetic-identity-session','message_id':'synthetic-message','run_id':'synthetic-run','generation':1,'ttl_seconds':1800}
        response=self.human_http('/v1/governance/turns',turn); assert response['status']==200,response['status']; self.turn=json.loads(response['body'])['id']
        self.check={'turn_id':self.turn,'operation_key':'synthetic-own-approval','stage':'tool_input','model_id':None,'tool_name':'fixture-tool','payload':{'instruction':'synthetic approval fixture; no tool execution'}}
        r=self.http('https://runtime.brokerrouter.internal:8443/v1/governance/checks',self.check,{'Content-Type':'application/json','Authorization':'Bearer '+self.key}); assert r['status']==200,r['status']; gate=json.loads(r['body']); assert gate['state']=='waiting_approval',gate
        self.approval=gate['approval']['id'] if 'approval' in gate else gate['approval_id']
        self.write('browser-storage.json','{}')
        self.write('browser-input.json',json.dumps({'tenant':self.tenant,'other_tenant':self.other_tenant,'users':self.users,'owner_id':self.owners['owner'],'approval':self.approval,'origin':PUBLIC}))
        self.save('seed')
        print('PASS: real operator-managed immutable OIDC bindings; distinct HUMAN creates same-owner turn; Agent virtual key stops at pending approval',flush=True)
    def human_http(self,path,body,key='human',typ='br-human+jwt',aud='fixture-human',kid=None):
        raw=json.dumps(body,separators=(',',':')).encode(); now=int(time.time())
        claims={'tenant_id':self.tenant,'iss':PUBLIC,'sub':'internal-owner','aud':aud,'exp':now+45,'nbf':now-1,'iat':now,'auth_time':now,'jti':str(uuid.uuid4()),'method':'POST','path':path,'body_sha256':hashlib.sha256(raw).hexdigest(),'interactive':True}
        encode=lambda b:base64.urlsafe_b64encode(b).rstrip(b'=')
        unsigned=encode(json.dumps({'alg':'RS256','typ':typ,'kid':kid or key+'-v1'},separators=(',',':')).encode())+b'.'+encode(json.dumps(claims,separators=(',',':')).encode())
        sig=self.openssl('dgst','-sha256','-sign',self.write('sign-'+key+'.pem',self.keys[key]),data=unsigned)
        token=(unsigned+b'.'+encode(sig)).decode(); self.sensitive.append(token.encode())
        return self.http('https://control.brokerrouter.internal:8443'+path,body,{'Content-Type':'application/json','Authorization':'Bearer '+token})
    def browser(self,mode):
        return self.call(['docker','run','--rm','--network',PROJECT+'_identity','--memory','768m','--cpus','1.5','--shm-size','1g','--cap-drop','ALL','--security-opt','no-new-privileges:true','--mount','type=bind,src='+str(self.root/'web-ca.crt')+',dst=/run/web-ca.crt,readonly','--mount','type=bind,src='+str(self.root/'browser-input.json')+',dst=/run/browser-input.json,readonly','--mount','type=bind,src='+str(self.root/'browser-storage.json')+',dst=/run/browser-storage.json','--mount','type=bind,src='+str(HERE/'browser.js')+',dst=/run/browser.js,readonly','--mount','type=bind,src='+str(NODE_MODULES)+',dst=/workspace/node_modules,readonly','--env','NODE_PATH=/workspace/node_modules','--env','NODE_EXTRA_CA_CERTS=/run/web-ca.crt','--env','IDENTITY_MODE='+mode,'--entrypoint','sh',BROWSER,'-ec','mkdir -p /root/.pki/nssdb; certutil -N -d sql:/root/.pki/nssdb --empty-password; certutil -A -d sql:/root/.pki/nssdb -n task-only-web-ca -t C,, -i /run/web-ca.crt; node /run/browser.js'],timeout=150)
    def verify(self):
        print(self.browser('approval').decode(),end='',flush=True)
        r=self.http('https://runtime.brokerrouter.internal:8443/v1/governance/checks',self.check,{'Content-Type':'application/json','Authorization':'Bearer '+self.key}); assert r['status']==200; gate=json.loads(r['body']); assert gate['state']=='ready',gate
        assert self.admin("SELECT state FROM approvals WHERE id='"+self.approval+"';").decode().strip()=='approved'
        print('PASS: browser decision recorded once; original Agent runtime re-check is ready without executing any tool or vendor',flush=True)
        bad=self.human_http('/v1/governance/turns',{'key_id':self.keyid,'session_id':'wrong-purpose','message_id':'synthetic-message','run_id':'synthetic-run','generation':1,'ttl_seconds':600},key='approvals',typ='br-human+jwt',aud='fixture-human',kid='human-v1'); assert bad['status']==401,bad['status']
        print('PASS: BFF approvals-purpose signing key cannot act as HUMAN turn issuer',flush=True)
        denied=self.http('https://approvals.brokerrouter.internal:8443/approvals/v1/query',{'limit':20,'pending_only':True},{'Content-Type':'application/json','Authorization':'Bearer '+self.key})
        assert denied['status']==401
        print('PASS: actual Agent virtual key cannot authenticate to the independent approvals control',flush=True)
        print(self.browser('revocation-login').decode(),end='',flush=True)
        # Revocation probe runs within browser process after a coordinating marker; separate mode tests callback binding denial after operator revocation.
        self.cli('bff-identity-set','--tenant',self.tenant,'--id',self.bindings['owner'],'--active','false','--reason','Synthetic revocation verification')
        print(self.browser('revocation-denied').decode(),end='',flush=True)
        rows=self.admin("SELECT a.usename,count(*),bool_and(s.ssl) FROM pg_stat_activity a JOIN pg_stat_ssl s USING(pid) WHERE a.usename LIKE 'brokerrouter_%_login' GROUP BY a.usename ORDER BY a.usename;").decode().strip()
        expected={'brokerrouter_runtime_login','brokerrouter_control_login','brokerrouter_console_login','brokerrouter_approvals_login','brokerrouter_bff_login'}
        actual={line.split('|')[0] for line in rows.splitlines()}; assert actual==expected,(actual,expected)
        for line in rows.splitlines(): role,count,tls=line.split('|'); assert int(count)<=2 and tls=='t'
        print('PASS: five actual workload LOGIN pools <=2; every workload PostgreSQL connection uses TLS',flush=True)
        idp=self.admin("SELECT count(*),bool_and(s.ssl) FROM pg_stat_activity a JOIN pg_stat_ssl s USING(pid) WHERE a.usename='keycloak_login';",idp=True).decode().strip().split('|')
        assert 2<=int(idp[0])<=4 and idp[1]=='t'
        assert self.admin("SELECT rolsuper,rolcreatedb,rolcreaterole,rolreplication,rolbypassrls,rolconnlimit FROM pg_roles WHERE rolname='keycloak_login';",idp=True).decode().strip()=='f|f|f|f|f|10'
        uid=self.call(['docker','exec',self.container('keycloak'),'cat','/proc/1/status']).decode()
        assert re.search(r'^Uid:\s+1000\s+1000\s+1000\s+1000$',uid,re.M)
        assert int(self.admin('SELECT count(*) FROM requests;'))==0
        assert int(self.admin('SELECT count(*) FROM ledger_entries;'))==0
        row=self.admin("SELECT state,decided_by::text,(SELECT count(*) FROM guardrail_grants WHERE approval_id=a.id) FROM approvals a WHERE id='"+self.approval+"';").decode().strip()
        assert row=='approved|'+self.owners['owner']+'|1'
        print('PASS: actual IdP UID1000, non-superuser isolated LOGIN and pool2..4 TLS; one owner decision/one grant and zero tool/provider request or ledger entry',flush=True)
        self.logs_safe()
    def logs_safe(self):
        logs=self.compose('logs','--no-color',timeout=30)
        for value in self.sensitive: assert value not in logs,'generated secret appeared in application logs'
        assert b'code=' not in logs and b'access_token' not in logs and b'refresh_token' not in logs and b'id_token' not in logs
        EVIDENCE.mkdir(exist_ok=True); (EVIDENCE/'r1-local-identity-services.log').write_bytes(logs)
        print('PASS: deployed service logs contain no generated credential, HUMAN assertion, virtual key or OAuth code/token',flush=True)
    def close(self):
        if self.started:
            self.compose('down','--remove-orphans','--timeout','5',timeout=90)
        # Preserve source/evidence, remove only this fixture-generated private credentials.
        import shutil; shutil.rmtree(self.root)

if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image',required=True)
    parser.add_argument('--source',required=True,type=Path,help='immutable schema/grant/helper source matching the release image')
    parser.add_argument('--node-modules',required=True,type=Path,help='existing Linux npm-ci cache; never copied')
    parser.add_argument('--evidence-dir',required=True,type=Path)
    for flag,default in [('keycloak-image',KC),('browser-image',BROWSER),('console-image',CONSOLE),('nginx-image',NGINX),('provisioner-image',OPS)]: parser.add_argument('--'+flag,default=default)
    parser.add_argument('--idp-only',action='store_true')
    parser.add_argument('--keep',action='store_true',help='keep this fixture only for diagnosis; private state is not evidence')
    parser.add_argument('--resume',type=Path)
    args=parser.parse_args()
    FROZEN=args.source.resolve(); NODE_MODULES=args.node_modules.resolve(); EVIDENCE=args.evidence_dir.resolve()
    assert EVIDENCE.is_absolute() and not EVIDENCE.is_relative_to(REPO)
    EVIDENCE.mkdir(parents=True,exist_ok=True)
    assert (FROZEN/'deploy/permissions.sql').is_file()
    assert json.loads((NODE_MODULES/'@playwright/test/package.json').read_text())['version']=='1.63.0'
    for reference in [args.image,args.keycloak_image,args.browser_image,args.console_image,args.nginx_image,args.provisioner_image]:
        assert re.fullmatch(r'(?:[A-Za-z0-9._/:-]+@)?sha256:[0-9a-f]{64}',reference),'immutable image reference required'
    KC=args.keycloak_image; BROWSER=args.browser_image; CONSOLE=args.console_image; NGINX=args.nginx_image; OPS=args.provisioner_image
    spec=importlib.util.spec_from_file_location('purpose',FROZEN/'deploy/production/purpose-logins.py')
    purpose=importlib.util.module_from_spec(spec); spec.loader.exec_module(purpose)
    if args.resume:
        root=args.resume.resolve(); assert root.parent==Path('/private/tmp') and root.name.startswith('brokerrouter-identity-secrets-')
        f,stage=Fixture.restore(root,args.image)
    else:
        check=subprocess.run(['docker','ps','-aq','--filter','label=com.docker.compose.project='+PROJECT],stdout=subprocess.PIPE,stderr=subprocess.PIPE,check=True)
        assert not check.stdout.strip(),'refusing to reuse any existing project'
        root=Path(tempfile.mkdtemp(prefix='brokerrouter-identity-secrets-',dir='/private/tmp')).resolve(); root.rmdir(); f=Fixture(args.image,root); stage='new'
    try:
        if stage=='new': f.setup(); f.start_idp(); stage='idp'
        if not args.idp_only:
            if stage=='idp': f.start_apps(); stage='apps'
            if stage=='apps': f.seed()
            f.verify()
    except Exception as e:
        print('FAIL: '+str(e),flush=True)
        if f.started:
            raw=f.compose('logs','--no-color',timeout=30)
            for value in f.sensitive: raw=raw.replace(value,b'[REDACTED]')
            (EVIDENCE/'r1-local-identity-failure-services.log').write_bytes(raw)
        raise
    finally:
        if not args.keep: f.close()
        else: print('PRIVATE_FIXTURE_ROOT='+str(root),flush=True)
