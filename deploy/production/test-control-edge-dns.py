#!/usr/bin/env python3
"""Real container IP replacement regression for fixed production edge targets."""
import http.client
import ipaddress
import json
import os
from pathlib import Path
import secrets
import socket
import ssl
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[2]
NGINX = 'nginx@sha256:dc5069ad14f19660b141b21236140b91656bf89bbc3e2417c70ae650cd66104c'
PYTHON = 'brokerrouter-provisioner@sha256:9e1e315101845437d5b589795d60cfc8449ca3a06110b8f3e2e83a85c3624a06'
CONSOLE = os.environ.get('BROKERROUTER_CONSOLE_TEST_IMAGE','brokerrouter-console:r1-local-verification')


def command(*args, data=None):
    p = subprocess.run(args, input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
    if p.returncode:
        raise RuntimeError('task-only edge DNS fixture command failed: ' + p.stderr.decode())
    return p.stdout.decode().strip()


class DynamicDnsTest(unittest.TestCase):
    def test_fixed_targets_follow_recreated_backend_ip_without_edge_restart(self):
        with tempfile.TemporaryDirectory(prefix='brokerrouter-edge-dns-') as directory:
            root = Path(directory).resolve(); root.chmod(0o700)
            ns = 'brokerrouter-edge-dns-' + secrets.token_hex(6)
            names = []; network = ns + '-network'
            # Select a new, explicit subnet so the old address can be held by a sentinel.
            network_ids = command('docker','network','ls','--quiet').splitlines()
            used = [ipaddress.ip_network(v['Subnet']) for n in json.loads(command('docker','network','inspect',*network_ids)) for v in (n['IPAM']['Config'] or []) if v.get('Subnet')]
            available = [ipaddress.ip_network('10.247.'+str(i)+'.0/24') for i in range(1,255) if not any(ipaddress.ip_network('10.247.'+str(i)+'.0/24').overlaps(n) for n in used if n.version==4)]
            self.assertTrue(available,'no non-overlapping fixture subnet available')
            subnet = secrets.choice(available)
            command('openssl','req','-x509','-newkey','rsa:2048','-nodes','-days','2','-subj','/CN=task-only edge DNS CA','-keyout',str(root/'ca.key'),'-out',str(root/'ca.crt'))
            hosts = ['console.fixture.invalid'] + [n+'.brokerrouter.internal' for n in ['control','assets','media-api','output-scan','output-delivery','console','catalog','settings','approvals','media-review']]
            command('openssl','req','-new','-newkey','rsa:2048','-nodes','-subj','/CN=console.fixture.invalid','-keyout',str(root/'server.key'),'-out',str(root/'server.csr'))
            (root/'ext').write_text('subjectAltName='+','.join('DNS:'+n for n in hosts)+'\n')
            command('openssl','x509','-req','-days','2','-in',str(root/'server.csr'),'-CA',str(root/'ca.crt'),'-CAkey',str(root/'ca.key'),'-CAcreateserial','-extfile',str(root/'ext'),'-out',str(root/'server.crt'))
            command('python3',str(ROOT/'deploy/production/render-control-edge.py'),'https://console.fixture.invalid','https://review.fixture-media.invalid',str(root))
            script = '''import http.server,json,os,threading,time
class H(http.server.BaseHTTPRequestHandler):
 def log_message(self,*a): pass
 def do_POST(self):
  raw=self.rfile.read(int(self.headers.get('Content-Length','0')))
  body=json.dumps({'generation':os.environ['GENERATION'],'path':self.path,'body':raw.decode(),'authorization':self.headers.get('Authorization'),'cookie':self.headers.get('Cookie'),'csrf':self.headers.get('X-BR-CSRF'),'origin':self.headers.get('Origin'),'port':self.server.server_port}).encode()
  self.send_response(200); self.send_header('Content-Type','application/json'); self.send_header('Content-Length',str(len(body))); self.end_headers(); self.wfile.write(body)
for p in range(8081,8092): threading.Thread(target=http.server.ThreadingHTTPServer(('0.0.0.0',p),H).serve_forever,daemon=True).start()
while True: time.sleep(1)
'''
            aliases = ['bff','control','assets','media-api','output-scan','output-delivery','console','catalog','settings','approvals','media-review']
            def backend(label, address, generation, alias=True):
                name=ns+'-'+label
                args=['docker','run','--detach','--name',name,'--network',network,'--ip',str(address),'--read-only','--cap-drop','ALL','--user','10001:10001','--memory','128m','--cpus','0.5','--pids-limit','64','--env','GENERATION='+generation]
                if alias:
                    for a in aliases: args += ['--network-alias',a]
                command(*args,'--entrypoint','python3',PYTHON,'-c',script); names.append(name); return name
            def edge(label,image,config):
                name=ns+'-'+label
                command('docker','run','--detach','--name',name,'--network',network,'--publish','127.0.0.1::8443','--read-only','--cap-drop','ALL','--cap-add','CHOWN','--cap-add','SETUID','--cap-add','SETGID','--security-opt','no-new-privileges:true','--memory','128m','--cpus','0.5','--pids-limit','64','--tmpfs','/tmp:size=16m,mode=1777','--mount','type=bind,src='+str(config)+',dst=/etc/nginx/nginx.conf,readonly','--mount','type=bind,src='+str(root/'server.crt')+',dst=/run/secrets/tls_fullchain,readonly','--mount','type=bind,src='+str(root/'server.key')+',dst=/run/secrets/tls_private_key,readonly','--entrypoint','nginx',image,'-g','daemon off;')
                names.append(name); return name
            trust=ssl.create_default_context(cafile=str(root/'ca.crt'))
            payload=b'{"synthetic":"fixed target, exact body"}'
            token='SYNTHETIC_DNS_RECREATE_NEVER_LOG'
            headers={'Content-Type':'application/json','Authorization':token,'Cookie':token,'X-BR-CSRF':token,'Origin':'https://console.fixture.invalid'}
            def request(edge_name,host,path):
                port=int(command('docker','port',edge_name,'8443/tcp').rsplit(':',1)[1]); c=http.client.HTTPSConnection('127.0.0.1',port,context=trust,timeout=5)
                c.sock=trust.wrap_socket(socket.create_connection(('127.0.0.1',port),5),server_hostname=host)
                try:
                    c.request('POST',path,payload,{'Host':host,**headers}); r=c.getresponse(); body=r.read(); return r.status,json.loads(body) if r.status==200 else None
                finally: c.close()
            try:
                command('docker','network','create','--subnet',str(subnet),network)
                old_ip=subnet.network_address+10; new_ip=subnet.network_address+11
                original=backend('original',old_ip,'original')
                main=edge('console',CONSOLE,root/'control-edge/console-nginx.conf'); control=edge('control',NGINX,ROOT/'deploy/production/control-plane-nginx.conf')
                time.sleep(0.5)
                probes=[(main,'console.fixture.invalid','/bff/v1/approvals/query?target=http%3A%2F%2Fattacker.invalid'),(control,'approvals.brokerrouter.internal','/approvals/v1/query?target=http%3A%2F%2Fattacker.invalid')]
                for e,h,p in probes:
                    status,value=request(e,h,p); self.assertEqual(status,200); self.assertEqual(value['generation'],'original')
                edge_ids={n:command('docker','inspect','--format','{{.Id}} {{.State.StartedAt}}',n) for n in [main,control]}
                command('docker','rm','--force',original); names.remove(original)
                backend('old-ip-sentinel',old_ip,'old-ip-sentinel',False)
                replacement=backend('replacement',new_ip,'replacement')
                self.assertNotEqual(command('docker','inspect','--format','{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}',replacement),str(old_ip))
                # The production DNS cache has a fixed five-second lifetime. This is a
                # bounded DNS-expiry wait, never retries a failed assertion or request.
                time.sleep(6)
                after=[(e,h,p,*request(e,h,p)) for e,h,p in probes]
                self.assertEqual({h:v['generation'] if status==200 else 'status-'+str(status) for _,h,_,status,v in after},
                                 {h:'replacement' for _,h,_ in probes},'edges kept the removed backend IP after container recreation')
                for e,h,p,status,value in after:
                    self.assertEqual(status,200)
                    self.assertEqual(value['path'],p); self.assertEqual(value['body'],payload.decode())
                    for k in ['authorization','cookie','csrf']: self.assertEqual(value[k],token)
                    self.assertEqual(value['origin'],headers['Origin'])
                self.assertEqual({n:command('docker','inspect','--format','{{.Id}} {{.State.StartedAt}}',n) for n in [main,control]},edge_ids)
                print('PASS: actual backend IP changes; both live edges resolve replacement without reload/restart or old-IP traffic',flush=True)
                print('PASS: constant BFF/approvals targets preserve exact JSON body, original path/query and signed/cookie/CSRF/origin headers',flush=True)
                for n in names: self.assertNotIn(token,command('docker','logs',n))
                print('PASS: dynamic edge lookup never takes a target from URI/headers and does not log synthetic credentials',flush=True)
            finally:
                for n in reversed(names): subprocess.run(['docker','rm','--force',n],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
                subprocess.run(['docker','network','rm',network],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)


if __name__=='__main__': unittest.main()
