'use strict';
// Optional integration smoke test: real browser DOM, focus, SVG and user gestures.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const CHROME = process.env.CHROME || ['chromium','google-chrome','chrome'].find(cmd =>
  spawnSync('sh',['-c',`command -v ${cmd}`],{encoding:'utf8'}).status === 0);

test('native UI preserves live DOM controls and sends actions only on click', {skip:!CHROME || process.env.SUNMAO_BROWSER_TEST !== '1'}, () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(),'sunmao-browser-'));
  try {
    const js = 'file://' + path.resolve(__dirname,'assets/native-ui.js');
    const fixture = `<!doctype html><html><meta charset="utf-8"><body><div id="host"></div><pre id="results"></pre>
<script src="${js}"></script><script>
let sent = [];
function wsSend(msg) { sent.push(msg); }
function check(ok,why) { if (!ok) throw new Error(why); }
try {
  const host = document.getElementById('host');
  const patch = p => JSON.stringify(p) + '\\n';
  const lines = [
    {op:'state',key:'batch',value:8},
    {op:'state',key:'name',value:'initial'},
    {op:'upsert',id:'panel',type:'card'},
    {op:'upsert',id:'range',type:'slider',parent:'panel',props:{bind:'batch',label:'Batch',min:1,max:128}},
    {op:'upsert',id:'nameInput',type:'input',parent:'panel',props:{bind:'name',label:'Name'}},
    {op:'upsert',id:'metric',type:'metric',parent:'panel',props:{label:'Double',value:{op:'mul',args:[{var:'batch'},2]}}},
    {op:'upsert',id:'run',type:'button',parent:'panel',props:{label:'Run',action:'ask',text:'Analyze {batch} {name}'}},
    {op:'upsert',id:'graph',type:'chart',parent:'panel',props:{label:'Throughput',series:[{key:'a',label:'Base'},{key:'b',label:'New'}],data:[{x:'A',a:3,b:5},{x:'B',a:6,b:8}]}},
    {op:'upsert',id:'table1',type:'table',parent:'panel',props:{columns:[{key:'x',label:'Batch'},{key:'y',label:'Value'}],rows:[{x:1,y:3},{x:2,y:6}]}},
    {op:'upsert',id:'inject',type:'text',parent:'panel',props:{text:'<img src=x onerror=alert(1)>'}}
  ];
  let source = 'Intro\\n\\x60\\x60\\x60sunmao-ui\\n' + lines.map(patch).join('');
  const markdown = txt => '<p>' + txt.replaceAll('<','&lt;') + '</p>';
  NativeUI.render(host,source,markdown);
  let slider = host.querySelector('.native-slider input');
  let input = host.querySelector('.native-input input');
  check(slider && input,'controls missing');
  check(host.querySelectorAll('.native-chart polyline').length === 2,'multiseries chart');
  check(host.querySelectorAll('.native-table-data tbody tr').length === 2,'table rows');
  check(!host.querySelector('.native-text img'),'text injection!');
  check(sent.length === 0,'asked without a gesture');
  slider.value='24'; slider.dispatchEvent(new Event('input',{bubbles:true}));
  check(host.querySelector('.native-value').textContent === '48','reactive metric');
  input.focus(); input.value = 'typed'; input.dispatchEvent(new Event('input',{bubbles:true}));
  check(document.activeElement === input,'lost input focus on local state update');
  const addition = [
    {op:'state',key:'batch',value:64},
    {op:'upsert',id:'late',type:'badge',parent:'panel',props:{text:'Later streamed'}}
  ];
  source += addition.map(patch).join('');
  NativeUI.render(host,source,markdown);
  check(document.activeElement === input,'lost input focus on new stream updates');
  check(host.querySelector('.native-slider input') === slider,'slider DOM replaced');
  check(host.querySelector('.native-input input') === input,'input DOM replaced');
  check(slider.value === '24','later model patch stole local input');
  check(host.querySelector('.native-badge').textContent === 'Later streamed','late badge missing');
  check(sent.length === 0,'stream update triggered an agent action');
  host.querySelector('.native-button button').click();
  check(sent.length === 1 && sent[0].text === 'Analyze 24 typed','button action payload');
  document.getElementById('results').textContent = JSON.stringify({ok:true,controls:2,polylineCount:2,tables:1,agentMessages:sent.length});
} catch(e) { document.getElementById('results').textContent = JSON.stringify({ok:false,error:String(e.stack||e)}); }
</script></body></html>`;
    const file = path.join(dir,'index.html');fs.writeFileSync(file,fixture);
    const proc = spawnSync(CHROME,['--headless','--no-sandbox','--disable-gpu','--disable-dev-shm-usage',
      '--allow-file-access-from-files','--virtual-time-budget=2500','--dump-dom','file://'+file],
      {encoding:'utf8',timeout:18000,maxBuffer:1024*1024});
    assert.equal(proc.status,0,proc.stderr.slice(-2000));
    const match = proc.stdout.match(/<pre id="results">([^<]+)<\/pre>/);
    assert.ok(match,'missing smoke test JSON: '+proc.stdout.slice(-500));
    const data = JSON.parse(match[1].replaceAll('&quot;','"').replaceAll('&amp;','&'));
    assert.equal(data.ok,true,data.error||'browser smoke');
    assert.equal(data.agentMessages,1);
  } finally { fs.rmSync(dir,{recursive:true,force:true}); }
});
