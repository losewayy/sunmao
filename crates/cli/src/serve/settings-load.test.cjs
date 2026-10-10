'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const vm = require('node:vm');
const source = readFileSync(join(__dirname,'assets/settings.js'),'utf8');
const from = source.indexOf('const SETTINGS_LOAD =');
const to = source.indexOf('/* providers page',from);
assert.ok(from >= 0 && to > from,'loading helpers present');
function setup() {
  const host = {innerHTML:'',retry:null,querySelector() {
    return {addEventListener:(_event,fn) => { this.retry=fn; }};
  }};
  const ctx = {sessionId:'one',setPage:'mcp',view:'settings',
    GRANTS:null,HOOKS:null,MCPS:null,PLUGIN_CAT:null,MODELS:null,SHELL:null,
    head:x=>'<h1>'+x+'</h1>',esc:x=>String(x),
    t:(msg,params)=>msg.replace('{msg}',params?.msg || ''),
    $:()=>host};
  vm.createContext(ctx);
  vm.runInContext(source.slice(from,to),ctx);
  return {ctx,host};
}
test('a failed settings request is terminal until explicit retry',async () => {
  const {ctx,host}=setup(); let count=0;
  ctx.request=async()=>{count++;throw Error('offline');};
  const run=()=>vm.runInContext("loadSettingsSection('mcp',request,data=>{MCPS=data},()=>{})",ctx);
  await run();
  assert.equal(vm.runInContext('SETTINGS_LOAD.mcp.status',ctx),'error');
  vm.runInContext("settingsFetchState('mcp','MCP',request)",ctx);
  assert.equal(count,1,'rendering an error must not re-fetch');
  assert.match(host.innerHTML,/offline/);
  assert.ok(host.retry,'retry button must have a callback');
});
test('a session switch prevents stale settings data from landing',async () => {
  const {ctx}=setup(); let deliver;
  ctx.request=()=>new Promise(resolve=>{deliver=resolve;});
  const pending=vm.runInContext("loadSettingsSection('mcp',request,data=>{MCPS=data},()=>{})",ctx);
  vm.runInContext("sessionId='two';resetSessionSettingsLoads()",ctx);
  deliver(['stale']); await pending;
  assert.equal(ctx.MCPS,null);
  assert.equal(vm.runInContext('SETTINGS_LOAD.mcp.status',ctx),'idle');
});
