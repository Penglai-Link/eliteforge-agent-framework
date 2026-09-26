// Real browser acceptance against the Rust host and a packaged Rust backend.
import assert from 'node:assert/strict';
import {mkdtemp,cp,mkdir,readFile,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {resolve,join,dirname} from 'node:path';
import {fileURLToPath} from 'node:url';
import {spawn,spawnSync} from 'node:child_process';
import {createServer} from 'node:net';
import {chromium} from 'playwright';
const root=resolve(dirname(fileURLToPath(import.meta.url)),'../../..');
const temporary=await mkdtemp(join(tmpdir(),'capability-browser-'));
const listener=createServer();await new Promise(done=>listener.listen(0,'127.0.0.1',done));const port=listener.address().port;await new Promise(done=>listener.close(done));
const executable=name=>join(root,'target/debug',`${name}${process.platform==='win32'?'.exe':''}`);
const server=spawn(executable('capability-host'),['--development','--dev-refresh','--port',String(port),'--store',join(temporary,'store')],{cwd:root,stdio:['ignore','pipe','pipe']});
let log='';server.stdout.on('data',b=>log+=b);server.stderr.on('data',b=>log+=b);
const base=`http://127.0.0.1:${port}`;
const wait=ms=>new Promise(done=>setTimeout(done,ms));
async function api(path,body,type='application/json'){
 const response=await fetch(base+path,{method:body===undefined?'GET':'POST',headers:{'Content-Type':type},body:body===undefined?undefined:type==='application/json'?JSON.stringify(body):body});
 assert.ok(response.ok,`${path}: ${response.status} ${await response.clone().text()}`);return response.json();
}
let browser;let passed=false;
try {
 for(let attempt=0;;attempt++){try{await api('/api/catalog');break;}catch(error){if(attempt>100||server.exitCode!==null)throw new Error(`${error}\n${log}`);await wait(100);}}
 async function install(version){
  const source=join(temporary,version);await cp(join(root,'examples/capability-pack'),source,{recursive:true});
  await mkdir(join(source,'bin'),{recursive:true});await cp(executable('capability-backend'),join(source,'bin/backend'+(process.platform==='win32'?'.exe':'')));
  await mkdir(join(source,'ui/sdk'),{recursive:true});await cp(join(root,'sdk/page/dist'),join(source,'ui/sdk'),{recursive:true});
  const manifest=JSON.parse(await readFile(join(source,'manifest.json'),'utf8'));manifest.version=version;
  const arch={arm64:'aarch64',x64:'x86_64'}[process.arch]??process.arch;const os={darwin:'macos',win32:'windows',linux:'linux'}[process.platform];
  manifest.backend.executables={[`${arch}-${os}`]:'bin/backend'+(process.platform==='win32'?'.exe':'')};await writeFile(join(source,'manifest.json'),JSON.stringify(manifest));
  const archive=join(temporary,`${version}.zip`);const packaged=spawnSync(executable('capability-host'),['pack',source,archive],{cwd:root,encoding:'utf8'});assert.equal(packaged.status,0,packaged.stderr);await api('/api/import',await readFile(archive),'application/octet-stream');
 }
 await install('1.0.0');await install('2.0.0');
 const implementations=['demo.chat','demo.state','demo.interactions','demo.conversations','demo.middleware','demo.custom'];
 const build=version=>({package:'demo.pack',version,implementations});
 await api('/api/build',build('1.0.0'));
 browser=await chromium.launch({headless:true});const page=await browser.newPage();const errors=[];page.on('pageerror',error=>errors.push(String(error)));
 await page.goto(base);await page.getByRole('button',{name:'挂载插件独立页面'}).click();
 const frame=page.frameLocator('iframe');await frame.locator('#context').filter({hasText:'light / zh-CN'}).waitFor();await frame.getByRole('button',{name:'调用本插件后端'}).click();
 await frame.locator('#result').filter({hasText:'page-only data'}).waitFor().catch(async error=>{throw new Error(`${error}\nPage result: ${await frame.locator('#result').textContent()}\n${log}`)});await frame.locator('#topic').filter({hasText:'page-only'}).waitFor();
 await page.getByRole('button',{name:'切换主题'}).click();await page.getByRole('button',{name:'切换语言'}).click();
 await frame.locator('#context').filter({hasText:'dark / en'}).waitFor();
 const before=await api('/api/catalog');const instance=before.instances[0];
 // A formal switch must reject the live page and keep the original instance.
 const busy=await fetch(base+'/api/build',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(build('2.0.0'))});assert.ok(!busy.ok);assert.match(await busy.text(),/busy/i);
 const oldSession=new URL(await page.locator('iframe').getAttribute('src'),base).pathname.split('/')[2];
 const oldFrame=await page.locator('iframe').elementHandle();
 await api('/api/dev/update',build('2.0.0'));
 await page.waitForFunction(()=>document.querySelector('iframe')?.getAttribute('src')?.includes('/resources/')&&document.querySelector('#status').textContent==='页面已挂载');
 // Poll by identity: a dev refresh closes the old iframe and mounts a new session.
 for(let attempt=0;;attempt++){const src=await page.locator('iframe').getAttribute('src').catch(()=>null);if(src&&!src.includes(oldSession))break;if(attempt>100)throw new Error('Page did not remount');await wait(100);}
 assert.equal(await oldFrame.evaluate(frame=>frame.isConnected),false);
 const late=await fetch(base+`/api/pages/${oldSession}/call`,{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({instance,method:'echo',params:'late'})});assert.ok(!late.ok);
 await frame.getByRole('button',{name:'调用本插件后端'}).click();await frame.locator('#topic').filter({hasText:'page-only'}).waitFor();
 await page.getByRole('button',{name:'关闭页面'}).click();await page.locator('iframe').waitFor({state:'detached'});
 for(let attempt=0;;attempt++){const catalog=await api('/api/catalog');if(!catalog.usage.some(item=>item.kind==='page'))break;if(attempt>100)throw new Error('Page lease remained');await wait(100);}
 await page.getByRole('button',{name:'扩展点',exact:true}).click();await page.getByRole('cell',{name:'framework.chat-model',exact:true}).waitFor();
 await mkdir(join(root,'output'),{recursive:true});await page.screenshot({path:join(root,'output/capability-browser.png'),fullPage:true});
 await page.getByRole('button',{name:'挂载插件独立页面'}).click();await frame.locator('#context').filter({hasText:'dark / en'}).waitFor();
 assert.deepEqual(errors,[]);passed=true;
 console.log('Browser acceptance passed: iframe RPC/topics, theme/language, Busy switch, dev remount, late-session isolation, close cleanup and extension catalog.');
} finally {
 server.kill('SIGINT');let forced=false;
 if(server.exitCode===null)await Promise.race([new Promise(done=>server.once('exit',done)),wait(10000).then(()=>{if(server.exitCode===null){forced=true;server.kill('SIGKILL');}})]);
 await browser?.close();await rm(temporary,{recursive:true,force:true});
 if(passed)assert.equal(forced,false,'Host shutdown with a live SSE page must finish without forced termination');
}
