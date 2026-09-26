#!/usr/bin/env node
// Builds complete artifacts before publishing; uses the same archive and runtime
// protocol as installed packages. All build commands go through RTK.
import {spawn} from 'node:child_process';
import {readFile,writeFile,readdir,stat,mkdir,cp,rm} from 'node:fs/promises';
import {resolve,join,dirname} from 'node:path';
import {fileURLToPath} from 'node:url';
const root=resolve(dirname(fileURLToPath(import.meta.url)),'..');
const host=process.env.CAPABILITY_DEV_HOST??'http://127.0.0.1:4317';
const once=process.argv.includes('--once');
const source=resolve(root,process.env.CAPABILITY_DEV_SOURCE??'examples/capability-pack');
const staging=join(root,'.capability-dev','build');
const run=(args,cwd=root)=>new Promise((ok,fail)=>{
  const child=spawn('rtk',args,{cwd,stdio:'inherit',env:{...process.env,CARGO_INCREMENTAL:'0',CARGO_PROFILE_DEV_DEBUG:'0'}});
  child.once('error',fail);child.once('exit',(code,signal)=>code===0?ok():fail(new Error(`Build failed (${code??signal}): ${args.join(' ')}`)));
});
async function files(path){const result=[];for(const entry of await readdir(path,{withFileTypes:true})){if(['target','node_modules','dist','.git'].includes(entry.name))continue;const child=join(path,entry.name);if(entry.isDirectory())result.push(...await files(child));else if(entry.isFile()){const info=await stat(child);result.push(`${child}:${info.mtimeMs}:${info.size}`);}}return result;}
async function fingerprint(){const entries=(await Promise.all(['crates','sdk/page',source].map(path=>files(resolve(root,path))))).flat();return entries.sort().join('\n');}
async function request(path,body,type='application/json'){
  const response=await fetch(`${host}${path}`,{method:'POST',headers:{'Content-Type':type},body:type==='application/json'?JSON.stringify(body):body});
  if(!response.ok)throw new Error(await response.text());return response.json();
}
async function publish(){
  const catalog=await fetch(`${host}/api/catalog`).then(r=>r.json());
  if(!catalog.dev_refresh)throw new Error('Start the host with --development --dev-refresh and an isolated --store');
  await run(['cargo','build','-p','eliteforge-agent-capability-dev-host',...JSON.parse(process.env.CAPABILITY_CARGO_ARGS??'[]')]);
  await run(['pnpm','--dir','sdk/page','build']);
  // Neither a failed Rust compilation nor a failed frontend build changes the
  // installed version, current process or mounted page.
  await rm(staging,{recursive:true,force:true});await mkdir(staging,{recursive:true});
  await cp(source,staging,{recursive:true});
  const manifest=JSON.parse(await readFile(join(staging,'manifest.json'),'utf8'));
  manifest.version=`${manifest.version.split('-')[0]}-dev.${Date.now()}`;
  if(manifest.backend){
    const arch={arm64:'aarch64',x64:'x86_64'}[process.arch]??process.arch;
    const os={darwin:'macos',win32:'windows',linux:'linux'}[process.platform];
    if(!os)throw new Error('Unsupported development platform');
    const name=process.platform==='win32'?'backend.exe':'backend';
    manifest.backend.executables={[`${arch}-${os}`]:`bin/${name}`};
    await mkdir(join(staging,'bin'),{recursive:true});
    await cp(join(root,'target','debug',`capability-backend${process.platform==='win32'?'.exe':''}`),join(staging,'bin',name));
  }
  await mkdir(join(staging,'ui','sdk'),{recursive:true});await cp(join(root,'sdk/page/dist'),join(staging,'ui/sdk'),{recursive:true});
  await writeFile(join(staging,'manifest.json'),JSON.stringify(manifest,null,2));
  const archive=join(root,'.capability-dev','latest.zip');
  await run([join(root,'target','debug',`capability-host${process.platform==='win32'?'.exe':''}`),'pack',staging,archive]);
  await request('/api/import',await readFile(archive),'application/octet-stream');
  const implementations=(process.env.CAPABILITY_DEV_IMPLEMENTATIONS??'demo.chat,demo.state,demo.interactions,demo.conversations,demo.middleware,demo.custom').split(',').filter(Boolean);
  for(let attempt=0;;attempt++){
    try{await request('/api/dev/update',{package:manifest.id,version:manifest.version,implementations});break;}
    catch(error){if(attempt>=60||!/busy/i.test(String(error)))throw error;await new Promise(done=>setTimeout(done,1000));}
  }
  process.stdout.write(`Ready: ${manifest.id}@${manifest.version}\n`);
}
let previous='';
do{
  const current=await fingerprint();
  if(current!==previous){previous=current;try{await publish();}catch(error){process.stderr.write(`${error}\n`);if(once)process.exitCode=1;}}
  if(!once)await new Promise(done=>setTimeout(done,1000));
}while(!once);
