'use strict';
const {test}=require('node:test');
const assert=require('node:assert/strict');
const Report=require('./assets/work-report.js');
test('only successful Write and Edit events count as modified files',()=>{
  let s=Report.fresh();
  s=Report.apply(s,{type:'message',message:{role:'user'}});
  s=Report.apply(s,{type:'tool_call',call:{id:'1',function:{name:'Edit',arguments:JSON.stringify({file_path:'src/app.rs'})}}});
  s=Report.apply(s,{type:'tool_result',call_id:'1',name:'Edit',ok:false});
  s=Report.apply(s,{type:'tool_call',call:{id:'2',function:{name:'Write',arguments:JSON.stringify({path:'src/ok.rs'})}}});
  s=Report.apply(s,{type:'tool_result',call_id:'2',name:'Write',ok:true});
  assert.equal(s.failed,1);assert.equal(s.done,2);
  assert.deepEqual(Report.view(s).modified,['src/ok.rs']);
});
test('new user turn resets work summary and flags failed test command',()=>{
  let s=Report.fresh();
  s=Report.apply(s,{type:'tool_start',name:'Bash',call_id:'a',args:{command:'cargo test --workspace'}});
  s=Report.apply(s,{type:'tool_done',name:'Bash',call_id:'a',ok:false});
  assert.equal(s.tests,1);assert.equal(s.testFails,1);
  s=Report.apply(s,{type:'user_message',content:[]});
  assert.equal(s.calls,0);assert.equal(s.failed,0);assert.equal(s.tests,0);
});
test('summary is replay-derived and cannot be forged by assistant prose',()=>{
  let s=Report.fresh();
  s=Report.apply(s,{type:'message',message:{role:'assistant',content:'I changed 50 files'}});
  assert.equal(s.calls,0);assert.equal(s.modified.size,0);
});

test('task progress mirrors durable TodoWrite facts across turns and compaction',()=>{
  let s=Report.fresh();
  s=Report.apply(s,{type:'todos',items:[
    {content:'Inspect sources',status:'done'},
    {content:'Build and test',status:'in_progress'},
    {content:'Ship',status:'pending'}]});
  assert.equal(Report.view(s).todos.length,3);
  s=Report.apply(s,{type:'message',message:{role:'user'}});
  assert.equal(Report.view(s).todos[1].status,'in_progress');
  s=Report.apply(s,{type:'compacted',summary:'checkpoint'});
  assert.equal(Report.view(s).todos[0].status,'done');
  s=Report.apply(s,{type:'todos',items:[{content:'Ship',status:'done'}]});
  assert.equal(Report.view(s).todos.length,1);
  s=Report.apply(s,{type:'todos',items:[{content:'bad',status:'unsafe'}]});
  assert.equal(Report.view(s).todos.length,0);
});
