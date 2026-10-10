'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const UI = require('./assets/native-ui.js');
const upsert = (id,type,parent='root',props={}) => JSON.stringify({op:'upsert',id,type,parent,props});

test('markdown mixed with UI fences preserves chunk sequence', () => {
  const chunks = UI.parts('Before\n```sunmao-ui\n'+upsert('card','card')+'\n```\nAfter');
  assert.deepEqual(chunks.map(x=>x.kind), ['md','ui','md']);
  assert.equal(chunks[0].body, 'Before\n');
  assert.equal(chunks[2].body, 'After');
});
test('incomplete JSONL line is not interpreted until next chunk', () => {
  const c = new UI.Compiler();
  const line = upsert('a','heading','root',{text:'你好'})+'\n';
  assert.equal(c.push(line.slice(0, 12)), false);
  assert.equal(c.nodes.size, 0);
  assert.equal(c.push(line), true);
  assert.equal(c.nodes.get('a').props.text, '你好');
});
test('final line commits when closing fence arrives', () => {
  const c = new UI.Compiler(); const line = upsert('a','card');
  assert.equal(c.push(line), false);
  assert.equal(c.push(line, true), true);
  assert.equal(c.nodes.size, 1);
});
test('state expressions update reactively with no eval', () => {
  const c = new UI.Compiler();
  c.patch({op:'state',key:'batch',value:8});
  assert.equal(UI.value({op:'mul',args:[{var:'batch'},5]},c.state), 40);
  c.state.batch = 12;
  assert.equal(UI.value({op:'mul',args:[{var:'batch'},5]},c.state), 60);
  assert.equal(UI.value({op:'exec',args:[10]},c.state),null);
});
test('reject invalid node types, cycles, unsafe keys', () => {
  const c = new UI.Compiler();
  assert.equal(c.patch({op:'upsert',id:'bad',type:'script'}),false);
  assert.equal(c.patch({op:'upsert',id:'ok',type:'card'}),true);
  assert.equal(c.patch({op:'upsert',id:'child',type:'text',parent:'ok'}),true);
  assert.equal(c.patch({op:'upsert',id:'ok',type:'card',parent:'child'}),false);
  assert.equal(c.patch(JSON.parse('{"op":"set","id":"ok","props":{"__proto__":{"polluted":true}}}')),false);
  assert.equal({}.polluted,undefined);
});
test('long text append rejected without mutating stored state', () => {
  const c = new UI.Compiler();
  c.patch({op:'upsert',id:'a',type:'text',props:{text:'A'.repeat(16380)}});
  assert.equal(c.patch({op:'append_text',id:'a',text:'12345'}),false);
  assert.equal(c.nodes.get('a').props.text.length,16380);
});
test('bounded source rejects oversized streamed payload', () => {
  const c = new UI.Compiler();
  assert.equal(c.push('x'.repeat(131073)), false);
  assert.equal(c.invalid,1);
});
test('remove cascades children', () => {
  const c = new UI.Compiler();
  c.patch({op:'upsert',id:'rootA',type:'card'});
  c.patch({op:'upsert',id:'child',type:'text',parent:'rootA'});
  assert.equal(c.patch({op:'remove',id:'rootA'}),true);
  assert.equal(c.nodes.size,0);
});

test('live user-controlled state survives later model state patches', () => {
  const c = new UI.Compiler();
  c.patch({op:'state',key:'batch',value:8});
  assert.equal(c.touch('batch',24),true);
  c.patch({op:'state',key:'batch',value:64});
  assert.equal(c.state.batch,24);
  assert.equal(c.defaults.batch,8);
  c.resetControls();
  assert.equal(c.state.batch,8);
});
test('rewrite or retry of a stream resets previous patch tree', () => {
  const c = new UI.Compiler();
  c.push(upsert('old','card')+'\n');
  assert.equal(c.nodes.has('old'),true);
  c.push(upsert('fresh','card')+'\n');
  assert.equal(c.nodes.has('old'),false);
  assert.equal(c.nodes.has('fresh'),true);
});
test('partial fence stays markdown until complete delimiter', () => {
  assert.deepEqual(UI.parts('start\n```sunmao-ui').map(s=>s.kind),['md']);
  assert.deepEqual(UI.parts('start\n```sunmao-ui\n').map(s=>s.kind),['md','ui']);
});
test('non-container cannot gain children and children prevent parent mutation', () => {
  const c = new UI.Compiler();
  assert.equal(c.patch({op:'upsert',id:'p',type:'heading'}),true);
  assert.equal(c.patch({op:'upsert',id:'c',type:'text',parent:'p'}),false);
  assert.equal(c.patch({op:'upsert',id:'a',type:'card'}),true);
  assert.equal(c.patch({op:'upsert',id:'c',type:'text',parent:'a'}),true);
  assert.equal(c.patch({op:'upsert',id:'a',type:'heading'}),false);
});
test('missing forward parent resolves without accepting a cycle', () => {
  const c = new UI.Compiler();
  assert.equal(c.patch({op:'upsert',id:'child',type:'text',parent:'future'}),true);
  assert.equal(c.patch({op:'upsert',id:'future',type:'card',parent:'child'}),false);
  assert.equal(c.patch({op:'upsert',id:'future',type:'card'}),true);
});
test('state values and expression operations are constrained', () => {
  const c = new UI.Compiler();
  assert.equal(c.patch({op:'state',key:'bad',value:Infinity}),false);
  assert.equal(c.patch({op:'state',key:'bad',value:'x'.repeat(257)}),false);
  assert.equal(c.patch({op:'state',key:'ok',value:10}),true);
  assert.equal(UI.value({op:'gt',args:[{var:'ok'},5]},c.state),true);
  assert.equal(UI.value({op:'if',args:[{op:'gte',args:[{var:'ok'},10]},'pass','fail']},c.state),'pass');
  assert.equal(UI.value({op:'div',args:[5,0]},c.state),null);
  assert.equal(UI.value({op:'add',args:[{var:'missing'},1]},c.state),null);
});
test('hard node cap and invalid patch lines cannot crash a compiler', () => {
  const c = new UI.Compiler();
  for(let i=0;i<96;i++) assert.equal(c.patch({op:'upsert',id:'n'+i,type:'text'}),true);
  assert.equal(c.patch({op:'upsert',id:'overflow',type:'text'}),false);
  assert.equal(c.nodes.size,96);
  const raw = 'not JSON\n'+upsert('fresh','card')+'\n';
  const another = new UI.Compiler();
  assert.equal(another.push(raw),true);
  assert.equal(another.invalid,1);
  assert.equal(another.nodes.size,1);
});
test('a user gesture can only affect a valid scalar key', () => {
  const c = new UI.Compiler();
  assert.equal(c.touch('bad.name',42),false);
  assert.equal(c.touch('ok',Number.NaN),false);
  assert.equal(c.touch('ok','a'.repeat(257)),false);
  assert.equal(c.touch('ok','good'),true);
  assert.equal(c.state.ok,'good');
});

test('arbitrary UTF-8 character-level stream boundaries match full replay', () => {
  const lines = [
    JSON.stringify({op:'state',key:'length',value:8}),
    upsert('panel','card'),
    upsert('title','heading','root',{text:'中文 / emoji 🧪 / μs'}),
    JSON.stringify({op:'append_text',id:'title',text:'...more'})
  ];
  const body = lines.join('\n');
  const full = new UI.Compiler();
  assert.equal(full.push(body,true),true);
  const incremental = new UI.Compiler();
  for (let i=1;i<body.length;i++) incremental.push(body.slice(0,i));
  incremental.push(body,true);
  assert.deepEqual([...incremental.nodes.entries()],[...full.nodes.entries()]);
  assert.deepEqual({...incremental.state},{...full.state});
  assert.equal(incremental.invalid,0);
});
test('closing an unfinished final JSON line does not produce an executable patch', () => {
  const c = new UI.Compiler();
  c.push(upsert('good','card')+'\n{"op":"upsert","id":"broken"');
  assert.equal(c.nodes.size,1);
  assert.equal(c.push(upsert('good','card')+'\n{"op":"upsert","id":"broken"',true),false);
  assert.equal(c.nodes.size,1);
  assert.equal(c.invalid,1);
});

test('source limits are UTF-8 byte limits, not JS UTF-16 units', () => {
  const c = new UI.Compiler();
  const body = ('中文').repeat(24000) + '\n';
  assert.equal(body.length < 131072,true);
  assert.equal(c.push(body),false);
  assert.equal(c.overLimit,true);
});
test('changing a component kind cannot inherit previous action props', () => {
  const c = new UI.Compiler();
  c.patch({op:'upsert',id:'a',type:'button',props:{action:'ask',text:'sensitive message'}});
  c.patch({op:'upsert',id:'a',type:'text',props:{text:'safe'}});
  assert.equal(c.nodes.get('a').props.action,undefined);
  assert.equal(c.nodes.get('a').props.text,'safe');
});

test('UTF-8 limit counts a surrogate pair split across streamed updates once', () => {
  const c = new UI.Compiler();
  const body = JSON.stringify({op:'upsert',id:'e',type:'text',props:{text:'🧪'}})+'\n';
  const idx = body.indexOf('🧪');
  c.push(body.slice(0,idx+1)); // high surrogate alone
  c.push(body.slice(0,idx+2)); // complete pair
  c.push(body);
  assert.equal(c.invalid,0);
  assert.equal(c.byteCount,new TextEncoder().encode(body).length);
});

test('reset controls preserves the reference held by live DOM listeners', () => {
  const c = new UI.Compiler();
  c.patch({op:'state',key:'batch',value:8});
  const original = c.state;
  c.touch('batch',24);
  c.resetControls();
  assert.equal(c.state,original);
  assert.equal(original.batch,8);
  c.touch('batch',32);
  assert.equal(original.batch,32);
});

test('serve native UI guide has a valid runtime example', () => {
  const { readFileSync } = require('node:fs');
  const { join } = require('node:path');
  const guide = readFileSync(join(__dirname,'assets/native-ui.md'),'utf8').replace(/\r\n/g,'\n');
  assert.match(guide,/Decide on your own/);
  assert.match(guide,/Use ordinary/);
  const begin = guide.indexOf('~~~sunmao-ui\n');
  const end = guide.indexOf('\n~~~',begin);
  assert.ok(begin >= 0 && end > begin);
  const example = guide.slice(begin + '~~~sunmao-ui\n'.length,end);
  const compiler = new UI.Compiler();
  assert.equal(compiler.push(example,true),true);
  assert.equal(compiler.nodes.get('slider').type,'slider');
  assert.equal(compiler.state.batch,8);
  assert.equal(compiler.invalid,0);
});

test('scoped patches address a stable panel and reject rewrites', () => {
  const first='{"op":"surface","id":"bench"}\n{"op":"set","id":"metric","props":{"value":5}}\n';
  const scope=UI.surfaceHeader(first);
  assert.equal(scope.id,'bench');
  assert.equal(UI.scopedDelta('',scope.body),scope.body);
  assert.equal(UI.scopedDelta(scope.body,scope.body + '{"op":"remove","id":"metric"}\n'),
    '{"op":"remove","id":"metric"}\n');
  assert.equal(UI.scopedDelta(scope.body,'different'),null);
  assert.equal(UI.surfaceHeader('{"op":"surface","id":"../escape"}\n'),null);
});
test('across messages, patches preserve user input in a reused compiler', () => {
  const c=new UI.Compiler();
  const initial='{"op":"state","key":"batch","value":8}\n' +
    '{"op":"upsert","id":"v","type":"metric","props":{"value":8}}\n';
  c.push(initial,true);
  c.touch('batch',24);
  c.push(initial + '{"op":"state","key":"batch","value":64}\n' +
    '{"op":"set","id":"v","props":{"value":{"var":"batch"}}}\n',true);
  assert.equal(c.state.batch,24);
  assert.deepEqual(c.nodes.get('v').props.value,{var:'batch'});
});

test('durable scalar UI state is validated and can arrive before a panel',()=>{
  UI.resetSurfaces();
  assert.equal(UI.applyState('bench','batch',24),true);
  assert.equal(UI.applyState('bench','x',{bad:1}),false);
  assert.equal(UI.applyState('../no','batch',8),false);
  UI.resetSurfaces();
});
