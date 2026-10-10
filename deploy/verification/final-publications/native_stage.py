"""Runtime-only full fixed native acceptance; never embeds a human or BFF secret."""
from __future__ import annotations
import argparse
import asyncio
import hashlib
import json
import logging
import os
import re
from pathlib import Path
import sys
import traceback

parser=argparse.ArgumentParser()
parser.add_argument("phase",choices=["prepare-session","initial-run","resume"])
parser.add_argument("--state-dir",required=True)
parser.add_argument("--host-source",required=True)
parser.add_argument("--config")
parser.add_argument("--observation",required=True)
args=parser.parse_args()
home=Path(args.state_dir)
home.mkdir(parents=True,exist_ok=True)
os.environ.clear()
os.environ.update(PATH="/opt/hermes/.venv/bin:/usr/local/bin:/usr/bin:/bin",HOME=str(home),HERMES_HOME=str(home),
                  BROKERROUTER_HERMES_GOVERNANCE="off",PYTHONDONTWRITEBYTECODE="1")
sys.path.insert(0,args.host_source)
logging.disable(logging.CRITICAL)
from gateway.run import GatewayRunner
from gateway.config import GatewayConfig,Platform
from gateway.session import SessionSource
from gateway.platforms.base import BasePlatformAdapter,PlatformConfig,SendResult
from gateway.platforms.event import MessageEvent

if not (home/"config.yaml").exists():
    (home/"config.yaml").write_text("model:\n  default: acceptance-placeholder\n  provider: openai\n  context_length: 32768\ncompression:\n  enabled: false\napprovals:\n  timeout: 1800\n")
runner=GatewayRunner(GatewayConfig())
original_error_reply=runner._hmwa_agent_error_reply
async def observed_error_reply(error,*positional,**named):
    observation["native_exception_type"]=type(error).__name__
    observation["native_exception_frames"]=[{"file":Path(frame.filename).name,"line":frame.lineno,"function":frame.name}
        for frame in traceback.extract_tb(error.__traceback__)]
    observation["native_exception_causes"]=[]
    current=error
    seen=set()
    while current is not None and id(current) not in seen and len(seen)<16:
        seen.add(id(current))
        item={"type":type(current).__name__}
        if type(current).__name__=="GovernanceHalt":
            code=getattr(current,"code",None)
            if isinstance(code,str) and re.fullmatch(r"[a-z0-9_]{1,80}",code):
                item["code"]=code
        observation["native_exception_causes"].append(item)
        current=current.__cause__ or current.__context__
    return await original_error_reply(error,*positional,**named)
runner._hmwa_agent_error_reply=observed_error_reply
source=SessionSource(platform=Platform.LOCAL,chat_id="brokerrouter-native-final-acceptance")
entry=runner.session_store.get_or_create_session(source)
observation={"session_id":entry.session_id,"session_key":entry.session_key,"phase":args.phase}

def save():
    destination=Path(args.observation)
    temporary=destination.with_suffix(".tmp")
    temporary.write_text(json.dumps(observation,sort_keys=True))
    os.replace(temporary,destination)

if args.phase=="prepare-session":
    save()
    runner.session_store.close_all_db_handles()
    raise SystemExit(0)

config=json.loads(Path(args.config).read_text())
if config["session_id"]!=entry.session_id:
    raise RuntimeError("acceptance routing identity changed")
os.environ.update({
    "BROKERROUTER_HERMES_GOVERNANCE":"required",
    "BROKERROUTER_BASE_URL":config["runtime_base_url"],
    "BROKERROUTER_VIRTUAL_KEY":config["virtual_key"],
    "BROKERROUTER_HERMES_APPROVALS_URL":config["approvals_url"],
    "BROKERROUTER_HERMES_GATEWAY_CA_FILE":config["runtime_ca_path"],
    "BROKERROUTER_HERMES_STATE_DB":str(home/"gate.db"),
    "BROKERROUTER_HERMES_STATE_KEY":config["state_key_hex"],
    "BROKERROUTER_HERMES_APPROVAL_WAIT_SECONDS":"30",
    "BROKERROUTER_HERMES_APPROVAL_POLL_SECONDS":"0.25",
})
from brokerrouter_hermes.gate import GovernanceHalt
from brokerrouter_hermes import final_publication
from gateway import delivery_ledger
sends=[]

class Sink(BasePlatformAdapter):
    name="isolated-zero-fee-native-publication"
    async def connect(self): return True
    async def disconnect(self): return None
    async def get_chat_info(self,_chat): return {}
    async def send(self,chat_id,content,**kwargs):
        metadata=kwargs.get("metadata") or {}
        approval=metadata.get("brokerrouter_approval_id")
        if approval:
            observation.update(approval_id=approval,state="waiting_approval")
            save()
            return SendResult(success=True,message_id="isolated-notice")
        digest=hashlib.sha256(content.encode("utf-8")).hexdigest()
        sends.append(digest)
        # This is the actual isolated local platform receiver, not an external
        # platform certification. Its accepted effect is durable before ACK.
        accepted=home/"platform-accepted.json"
        prior=json.loads(accepted.read_text()) if accepted.exists() else []
        prior.append(digest)
        accepted.write_text(json.dumps(prior))
        return SendResult(success=True,message_id="isolated-ack-"+str(len(prior)))

adapter=Sink(PlatformConfig(),Platform.LOCAL)
adapter._session_store=runner.session_store
runner.adapters[Platform.LOCAL]=adapter
runner._session_state(entry.session_key).conversation.model_override={
    "model":config["model_id"],"provider":"openai","requested_provider":"openai",
    "api_key":config["virtual_key"],"base_url":config["runtime_base_url"]+"/v1","api_mode":"chat_completions",
}
generation=runner._begin_session_run_generation(entry.session_key)
if type(config["generation"]) is not int or generation!=config["generation"]:
    raise RuntimeError("acceptance actual native generation changed")
event=MessageEvent(text="zero fee native final publication acceptance",source=source,message_id=config["message_id"],
                   metadata={"brokerrouter_turn":{"turn_id":config["turn_id"],"run_id":config["run_id"]}})

async def run():
    try:
        response=await runner._handle_message_with_agent(event,source,entry.session_key,generation)
        if getattr(event,"_brokerrouter_final_agent",None) is None:
            observation.update(state="held",code="no_released_native_controller")
            return
        result,_=await adapter.send_final_ledgered(event,entry.session_key,response,{},reply_to=None)
        observation.update(state="delivered" if result.success else "held",send_success=result.success,
                           error=result.error,text_sha256=hashlib.sha256(response.encode("utf-8")).hexdigest())
    except GovernanceHalt as halt:
        observation.update(state="held",code=halt.code)
    except Exception as error:
        observation.update(state="held",code="native_outer_failed",native_exception_type=type(error).__name__)
        observation["native_exception_frames"]=[{"file":Path(frame.filename).name,"line":frame.lineno,"function":frame.name}
            for frame in traceback.extract_tb(error.__traceback__)]
    finally:
        db=runner.session_store._db_for_session_id(entry.session_id)
        rows=db.get_messages(entry.session_id)
        observation.update(ordinary_rows=len(rows),ordinary_assistant_rows=sum(row["role"]=="assistant" for row in rows),
                           platform_calls=len(sends))
        with final_publication._transaction() as connection:
            row=connection.execute("""SELECT publication_id,output_operation_id,text_sha256,bytes,sources
                FROM brokerrouter_final_candidates LIMIT 1""").fetchone()
        if row:
            observation.update(publication_id=row[0],output_operation_id=row[1],candidate_text_sha256=row[2],
                               candidate_bytes=row[3],source_request_ids=json.loads(row[4]) if row[4] else [])
        save()

asyncio.run(run())
runner.session_store.close_all_db_handles()
