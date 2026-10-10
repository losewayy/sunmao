'use strict';
const {test}=require('node:test'), assert=require('node:assert/strict');
const {readFileSync}=require('node:fs'),{join}=require('node:path'),vm=require('node:vm');
const src=readFileSync(join(__dirname,'assets/approvals.js'),'utf8');
function makeContext(send){
  const toasts=[];
  const ctx={Map,WeakMap,wsSend:send,toast:(...a)=>toasts.push(a),
    t:s=>s,esc:s=>String(s),ic:s=>s,renderRail:()=>{},
    syncWait:()=>{},sessionId:'s',stIcon:()=>'',logEv:()=>{},
    $:()=>({hidden:false}),motion:{height:()=>Promise.resolve()}};
  vm.createContext(ctx);vm.runInContext(src,ctx);
  return {ctx,toasts};
}
test('approval label explains risks without assuming shell command intent',()=>{
  const {ctx}=makeContext(()=>false);
  assert.match(vm.runInContext("approvalImpact('Bash')",ctx),/命令/);
  assert.match(vm.runInContext("approvalImpact('Edit')",ctx),/文件/);
  assert.match(vm.runInContext("approvalImpact('mcp__srv__write')",ctx),/MCP/);
});
test('failed delivery keeps approval pending and does not show success',()=>{
  const {ctx,toasts}=makeContext(()=>false);
  const card={classList:{toggle(){},add(){}},innerHTML:'approve',querySelector:()=>null};
  ctx.card=card;
  vm.runInContext('pendingApprovals.set(2,card)',ctx);
  vm.runInContext("decide('once',2)",ctx);
  assert.equal(vm.runInContext('pendingApprovals.has(2)',ctx),true);
  assert.equal(toasts.length,1);
  assert.equal(card.innerHTML,'approve');
});
