const fs = require('fs');
const { chromium, expect } = require('@playwright/test');
const input = JSON.parse(fs.readFileSync('/run/browser-input.json', 'utf8'));
const mode = process.env.IDENTITY_MODE;
const sensitive = new Set(Object.values(input.users).map(u=>u.password));
let browser;
function pass(message) { process.stdout.write('PASS: '+message+'\n'); }
async function api(page, path, body, csrf = true) {
  return page.evaluate(async ({path,body,csrf}) => {
    const value=document.cookie.split(';').map(v=>v.trim()).find(v=>v.startsWith('__Host-br-csrf='))?.slice('__Host-br-csrf='.length);
    const response=await fetch(path,{method:body===null?'GET':'POST',headers:body===null?{}:{'Content-Type':'application/json',...(csrf?{'X-BR-CSRF':value||''}:{})},body:body===null?undefined:JSON.stringify(body)});
    let data=null; try { data=await response.json(); } catch {}
    return {status:response.status,data};
  },{path,body,csrf});
}
async function login(name, expectLogin = true, tenant = input.tenant) {
  const context=await browser.newContext();
  const page=await context.newPage(); page.setDefaultTimeout(30000);
  let callback;
  page.on('request',request=>{if(request.url().startsWith(input.origin+'/bff/v1/callback?')) callback=request.url();});
  await page.goto(input.origin);
  await page.getByLabel('组织 ID',{exact:true}).fill(tenant);
  await page.getByRole('button',{name:'使用组织身份登录',exact:true}).click();
  await expect(page.locator('#username')).toBeVisible();
  const destination=new URL(page.url());
  expect(destination.origin).toBe('https://identity.fixture.invalid');
  expect(destination.searchParams.get('code_challenge_method')).toBe('S256');
  expect(destination.searchParams.get('max_age')).toBe('0');
  expect(destination.searchParams.get('nonce')).toBeTruthy();
  expect(destination.searchParams.get('state')).toBeTruthy();
  expect(destination.searchParams.get('redirect_uri')).toBe(input.origin+'/bff/v1/callback');
  await page.locator('#username').fill(input.users[name].username);
  await page.locator('#password').fill(input.users[name].password);
  await page.locator('#kc-login').click();
  await Promise.race([page.waitForURL(url=>url.origin===input.origin),page.locator('input[name="email"]').waitFor({state:'visible'})]);
  if(await page.locator('input[name="email"]').isVisible()) {
    await page.locator('input[name="email"]').fill(input.users[name].username+'@fixture.invalid');
    await page.getByRole('button',{name:'Submit',exact:true}).click();
  }
  await page.waitForURL(url=>url.origin===input.origin);
  const session=await api(page,'/bff/v1/session',null);
  if(expectLogin) {
    expect(session.status).toBe(200);
    expect(session.data.tenant_id).toBe(input.tenant);
    if(name==='owner') expect(session.data.user_id).toBe(input.owner_id);
    const cookies=await context.cookies(input.origin);
    const opaque=cookies.find(c=>c.name==='__Host-br-session');
    const csrf=cookies.find(c=>c.name==='__Host-br-csrf');
    expect(opaque.secure).toBe(true); expect(opaque.httpOnly).toBe(true); expect(opaque.sameSite).toBe('Lax'); expect(opaque.value).toMatch(/^[0-9a-f]{64}$/);
    expect(csrf.secure).toBe(true); expect(csrf.httpOnly).toBe(false);
    cookies.forEach(c=>sensitive.add(c.value));
    expect(await page.evaluate(()=>document.cookie.includes('__Host-br-session='))).toBe(false);
    expect(await page.evaluate(()=>Object.keys(localStorage))).toEqual([]);
    expect(await page.evaluate(()=>Object.keys(sessionStorage))).toEqual([]);
    expect(Object.keys(session.data).some(k=>/token/i.test(k))).toBe(false);
  } else expect(session.status).toBe(401);
  return {context,page,callback};
}
(async()=>{
  browser=await chromium.launch({headless:true});
  if(mode==='approval') {
    const unbound=await login('unbound',false); await unbound.context.close();
    pass('real unbound Keycloak user completes OIDC but receives no BFF session');
    const crossTenant=await login('owner',false,input.other_tenant); await crossTenant.context.close();
    pass('real bound IdP owner cannot log in to a different active tenant without an explicit binding');
    const other=await login('other');
    const inbox=await api(other.page,'/bff/v1/approvals/query',{limit:20,pending_only:true}); expect(inbox.status).toBe(200); expect(inbox.data.data).toEqual([]);
    expect((await api(other.page,'/bff/v1/approvals/review',{approval_id:input.approval})).status).toBe(404);
    expect((await api(other.page,'/bff/v1/approvals/decide',{approval_id:input.approval,review_id:'00000000-0000-0000-0000-000000000000',acknowledged_scope:true,decision:{decision:'continue_this_turn',approved_guardrail_ids:['identity-review'],feedback:null}})).status).toBe(404);
    await other.context.close(); pass('different bound user cannot list, review or decide the owner approval');
    const owner=await login('owner');
    const denied=await api(owner.page,'/bff/v1/approvals/query',{limit:20,pending_only:true},false); expect(denied.status).toBe(403);
    const crossOrigin=await owner.context.request.post(input.origin+'/bff/v1/approvals/query',{headers:{Origin:'https://wrong.fixture.invalid','Content-Type':'application/json'},data:{limit:20,pending_only:true}}); expect(crossOrigin.status()).toBe(403);
    const audit=await api(owner.page,'/bff/v1/audit/query',{start:new Date(Date.now()-3600000).toISOString(),end:new Date().toISOString(),limit:20}); expect(audit.status).toBe(403);
    pass('actual opaque Secure HttpOnly session; no browser upstream tokens/storage; CSRF, wrong origin and ungranted console permission denied');
    const replay=await owner.context.request.get(owner.callback,{headers:{Accept:'text/html'},maxRedirects:0}); expect(replay.status()).toBe(303); expect(replay.headers().location).toBe('/?login=failed'); expect(replay.headers()['set-cookie']||'').not.toContain('__Host-br-session=');
    pass('consumed real authorization callback cannot mint a second session on replay');
    await owner.page.getByRole('link',{name:'本人审批',exact:true}).click();
    await owner.page.getByRole('button',{name:'审阅风险原文',exact:true}).click();
    await expect(owner.page.getByLabel('风险审阅',{exact:true})).toBeVisible();
    await expect(owner.page.locator('.approval-payload').first()).toContainText('synthetic approval fixture; no tool execution');
    await owner.page.locator('input[name="rail"]').check();
    await owner.page.locator('select[name="decision"]').selectOption('continue_this_turn');
    await owner.page.getByRole('checkbox',{name:'我已核对操作与风险，并理解同轮后续动作也可能跳过所选版本护栏；审批不等于执行完成。',exact:true}).check();
    await owner.page.getByRole('button',{name:'核对本次决定',exact:true}).click();
    await expect(owner.page.getByRole('heading',{name:'确认提交这个决定',exact:true})).toBeVisible();
    await owner.page.getByRole('button',{name:'确认并提交决定',exact:true}).click();
    await expect(owner.page.getByRole('status').filter({hasText:'决定已记录'})).toBeVisible();
    pass('real deployed Console owner reviews source, selects exact rail, acknowledges scope and explicitly confirms approval');
    await owner.page.getByRole('button',{name:'退出登录',exact:true}).click();
    await expect(owner.page.getByRole('heading',{name:'登录控制台',exact:true})).toBeVisible();
    expect((await api(owner.page,'/bff/v1/session',null)).status).toBe(401);
    pass('real Console logout invalidates the opaque session');
    await owner.context.close();
  } else if(mode==='revocation-login') {
    const owner=await login('owner');
    await owner.context.storageState({path:'/run/browser-storage.json'});
    fs.chmodSync('/run/browser-storage.json',0o600);
    await owner.context.close();
    pass('existing owner session saved only to protected fixture state for revocation check');
  } else if(mode==='revocation-denied') {
    const context=await browser.newContext({storageState:'/run/browser-storage.json'});
    const page=await context.newPage(); await page.goto(input.origin);
    expect((await api(page,'/bff/v1/session',null)).status).toBe(401);
    await context.close();
    const refused=await login('owner',false); await refused.context.close();
    pass('operator-disabled immutable identity binding invalidates old session and denies a fresh real OIDC login');
  } else throw Error('unknown verifier mode');
})().catch(async error=>{
  if(browser) for(const context of browser.contexts()) try {
    for(const c of await context.cookies()) sensitive.add(c.value);
    for(const page of context.pages()) {
      const location=new URL(page.url());
      process.stdout.write('DIAGNOSTIC: '+location.origin+location.pathname+' title='+await page.title()+' headings='+JSON.stringify(await page.locator('h1,h2,h3').allTextContents())+' fields='+JSON.stringify(await page.locator('input').evaluateAll(inputs=>inputs.map(i=>({name:i.name,type:i.type}))))+'\n');
    }
  } catch {}
  let message=String(error.message||error);
  for(const value of sensitive) if(value) message=message.split(value).join('[REDACTED]');
  message=message.replace(/([?&](?:code|state|session_state|nonce)=)[^&\s'"<>]+/g,'$1[REDACTED]');
  process.stderr.write('IDENTITY VERIFICATION FAILED: '+message+'\n'); process.exitCode=1;
}).finally(async()=>{if(browser) await browser.close();});
