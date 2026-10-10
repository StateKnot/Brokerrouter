#!/usr/bin/env python3
"""Extend the frozen native acceptance with current policy2 and a real core restart."""
import hashlib
import json
from pathlib import Path
import time
import uuid

import native_acceptance as native


class RecoveryFixture(native.NativeFixture):
    def verify(self):
        result = super().verify()
        first = self.approval
        publication = self.ack['publication_id']
        operation = result['delivered']['output_operation_id']
        request = self.request
        accepted = json.loads((self.agent_root / 'platform-accepted.json').read_text())
        path = '/v1/governance/final-publications/' + publication + '/release'
        self.policy(2)
        response = self.agent(path, {})
        native.require(response['status'] == 200, 'current policy2 release responds with metadata')
        pending = json.loads(response['body'])
        native.require(pending['state'] == 'waiting_approval' and 'text' not in pending
                       and pending['approval']['id'] != first,
                       'policy2 denies old grant and never returns plaintext before a distinct approval')
        second = str(uuid.UUID(pending['approval']['id']))
        facts = json.loads(self.admin(
            "SELECT jsonb_build_object('pending',a.state='pending','guardrails',a.guardrails,"
            "'version',g.version,'current_identity',b.active AND u.active AND k.revoked_at IS NULL "
            "AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp()),"
            "'live_deadlines',a.expires_at>clock_timestamp() AND t.expires_at>clock_timestamp() "
            "AND p.expires_at>clock_timestamp()) FROM approvals a "
            "JOIN governance_turns t ON t.tenant_id=a.tenant_id AND t.id=a.turn_id "
            "JOIN runtime_bindings b ON b.tenant_id=t.tenant_id AND b.key_id=t.key_id AND b.user_id=t.user_id "
            "JOIN governance_users u ON u.tenant_id=t.tenant_id AND u.id=t.user_id "
            "JOIN virtual_keys k ON k.tenant_id=t.tenant_id AND k.id=t.key_id "
            "JOIN runtime_final_publications p ON p.tenant_id=t.tenant_id AND p.turn_id=t.id "
            "JOIN guardrails g ON g.tenant_id=t.tenant_id AND g.id='final-text-review' "
            "WHERE a.id='" + second + "' AND p.id='" + publication + "';"))
        native.require(facts['pending'] and facts['version'] == 2 and facts['current_identity']
                       and facts['live_deadlines'] and len(facts['guardrails']) == 1
                       and facts['guardrails'][0]['version'] == 2,
                       'actual policy2 approval has current version, live identity and live deadlines')
        self.passed('same schema32 policy2 invalidates the actual old grant; distinct current approval metadata has no plaintext and retains live identity/deadlines')
        self.approval = self.approve_native({'approval_id': second})
        native.require(self.request == request, 'policy2 review still refers to the same real model source')
        deadlines = self.admin(
            "SELECT count(*),bool_and(g.expires_at>clock_timestamp() AND "
            "g.expires_at<=LEAST(t.expires_at,p.expires_at)) FROM guardrail_grants g "
            "JOIN governance_turns t ON t.tenant_id=g.tenant_id AND t.id=g.turn_id "
            "JOIN runtime_final_publications p ON p.tenant_id=t.tenant_id AND p.turn_id=t.id "
            "WHERE g.turn_id='" + self.turn + "' AND p.id='" + publication + "';").decode().strip()
        native.require(deadlines == '2|t', 'both actual version grants retain bounded live source/turn TTL')
        before = json.loads(self.call(['docker', 'inspect', self.container('gateway')]))[0]
        self.compose('restart', '--timeout', '10', 'gateway', timeout=45)
        self.compose('up', '--detach', '--wait', '--wait-timeout', '60', 'gateway', timeout=90)
        after = json.loads(self.call(['docker', 'inspect', self.container('gateway')]))[0]
        native.require(before['Id'] == after['Id'] and before['State']['StartedAt'] != after['State']['StartedAt'],
                       'actual fixed core process restarted in the same container')
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            try:
                if self.http('https://runtime.brokerrouter.internal:8443/health/ready', ca='runtime-ca.crt')['status'] == 204:
                    break
            except RuntimeError:
                pass
            time.sleep(0.5)
        else:
            raise AssertionError('restarted core actual HTTPS readiness deadline')
        response = self.agent(path, {})
        native.require(response['status'] == 200, 'actual core restart resumes final release')
        ready = json.loads(response['body'])
        self.sensitive.append(ready['text'].encode())
        native.require(ready['state'] == 'ready' and ready['output_operation_id'] == operation
                       and hashlib.sha256(ready['text'].encode()).hexdigest() == self.ack['text_sha256']
                       and len(ready['text'].encode()) == self.ack['text_bytes'],
                       'current version2 release after real restart is exact immutable text from the same operation')
        self.passed('real fixed core process restart resumes the same exact publication and output operation only after actual version2 workbench review/decision')
        replay = self.native('resume')
        native.require(replay['state'] == 'delivered' and replay['ordinary_rows'] == 2
                       and replay['ordinary_assistant_rows'] == 1 and replay['platform_calls'] == 0
                       and replay['output_operation_id'] == operation
                       and json.loads((self.agent_root / 'platform-accepted.json').read_text()) == accepted
                       and self.stats()['calls'] == 1 and self.stats()['streaming_calls'] == 1,
                       'fresh complete native process after policy2/core restart duplicates no model/history/send')
        counts = self.admin("SELECT (SELECT count(*) FROM requests),(SELECT count(*) FROM runtime_final_publications),"
                            "(SELECT count(*) FROM approvals),(SELECT count(*) FROM guardrail_grants),"
                            "(SELECT count(*) FROM ledger_entries WHERE request_id='" + request + "' AND kind='finalize');").decode().strip()
        native.require(counts == '1|1|2|2|1', 'actual final counts stay one source/publication/settlement and two version grants')
        self.passed('fresh complete native restart after current policy2 and core restart adds zero model calls, history rows or sender effects; database1/1/2/2/1')
        for data in self.native_logs:
            for value in self.sensitive:
                native.require(value not in data, 'native recovery log disclosed a generated credential or plaintext')
        (native.final.EVIDENCE / 'native-processes.log').write_bytes(b''.join(self.native_logs))
        self.logs_safe()
        result.update(counts=dict(zip(['requests', 'publications', 'approvals', 'grants', 'finalize_ledger_entries'], [1, 1, 2, 2, 1])),
                      policy2_approval_ids=[first, second], policy2_current_identity=True,
                      policy2_live_deadlines=True, policy2_bounded_grant_deadlines=True,
                      actual_core_restart=True, post_core_restart_native=replay)
        return result


if __name__ == '__main__':
    native.NativeFixture = RecoveryFixture
    native.main()
    path = native.final.EVIDENCE / 'native-manifest.json'
    report = json.loads(path.read_text())
    report.update(scope='full_native_policy2_and_actual_core_restart',
                  extended_verification_source_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest())
    path.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
