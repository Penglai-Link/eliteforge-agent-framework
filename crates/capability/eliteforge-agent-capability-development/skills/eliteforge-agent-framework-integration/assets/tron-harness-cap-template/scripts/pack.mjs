import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { pathToFileURL } from 'node:url';
import { archive } from './archive.mjs';

export function assertIdentity(id) {
  if (typeof id !== 'string' || id.length > 48 || !/^[a-z][a-z0-9]*(?:[.-][a-z0-9]+)*$/.test(id)) {
    throw new Error('包 ID 需为 1–48 位小写字母、数字、点或连字符，以字母开头，分隔符不连续');
  }
}
export function assertVersion(version) {
  const number = '(?:0|[1-9][0-9]*)';
  if (typeof version !== 'string' || !new RegExp(`^${number}\\.${number}\\.${number}(?:-[0-9A-Za-z-]+(?:\\.[0-9A-Za-z-]+)*)?(?:\\+[0-9A-Za-z-]+(?:\\.[0-9A-Za-z-]+)*)?$`).test(version)) throw new Error('版本必须符合 SemVer');
  for (const part of (version.split('+')[0].split('-').slice(1).join('-') || '').split('.')) {
    if (/^0[0-9]+$/.test(part)) throw new Error('SemVer 预发布数字不能有前导零');
  }
}
export function safeFile(root, relative, directory = false) {
  if (typeof relative !== 'string' || !relative || /[\\:\x00-\x1f]/.test(relative)) throw new Error(`非法路径：${relative}`);
  const parts = relative.split('/');
  let current = root;
  for (const part of parts) {
    if (!part || part === '.' || part === '..' || /[. ]$/.test(part) || /^(con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)/i.test(part)) throw new Error(`非法路径：${relative}`);
    current = path.join(current, part);
    if (fs.lstatSync(current).isSymbolicLink()) throw new Error(`不能包含符号链接：${relative}`);
  }
  const stat = fs.statSync(current);
  if (directory ? !stat.isDirectory() : !stat.isFile()) throw new Error(`路径类型不匹配：${relative}`);
  return current;
}
const readJson = (root, name) => JSON.parse(fs.readFileSync(safeFile(root, name), 'utf8'));
export function collectDirectory(root, relative, files) {
  const directory = safeFile(root, relative, true);
  for (const name of fs.readdirSync(directory).sort()) {
    const item = `${relative}/${name}`;
    const stat = fs.lstatSync(path.join(root, item));
    if (stat.isDirectory()) collectDirectory(root, item, files);
    else files.set(item, fs.readFileSync(safeFile(root, item)));
  }
}
export function run(command, args, cwd, capture = false) {
  const result = spawnSync(command, args, { cwd, encoding: 'utf8', stdio: capture ? ['ignore', 'pipe', 'inherit'] : 'inherit', maxBuffer: 16 * 1024 * 1024 });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} 执行失败（${result.status ?? result.signal}）`);
  return result.stdout;
}
function checkNavigation(navigation, pageIds) {
  const domains = new Set(), menus = new Map();
  for (const [kind, entries] of [['domains', navigation.domains], ['menus', navigation.menus]]) {
    if (!Array.isArray(entries)) throw new Error(`navigation.${kind} 必须是数组`);
    for (const entry of entries) {
      const codes = kind === 'domains' ? domains : menus;
      if (typeof entry.code !== 'string' || !/^[A-Z0-9_]{1,64}$/.test(entry.code) || codes.has(entry.code)) throw new Error(`导航编码非法或重复：${entry.code}`);
      for (const locale of ['zh-CN', 'en-US']) if (typeof entry.name?.[locale] !== 'string' || !entry.name[locale].trim()) throw new Error(`导航名称缺少 ${locale}：${entry.code}`);
      if (kind === 'domains') domains.add(entry.code);
      else menus.set(entry.code, entry);
    }
  }
  for (const menu of menus.values()) if (menu.menuType === 'PackPage' && !pageIds.has(menu.pageCode)) throw new Error(`菜单引用未知页面：${menu.pageCode}`);
  if (!navigation.layouts || Array.isArray(navigation.layouts) || typeof navigation.layouts !== 'object') throw new Error('navigation.layouts 必须是对象');
  for (const [domain, layout] of Object.entries(navigation.layouts)) {
    if (!domains.has(domain) || !Array.isArray(layout)) throw new Error(`布局领域不存在或布局格式错误：${domain}`);
    const items = new Map(), positions = new Set();
    for (const item of layout) {
      if (!menus.has(item.menuCode) || items.has(item.menuCode)) throw new Error(`布局菜单不存在或重复：${item.menuCode}`);
      if (!Number.isSafeInteger(item.position) || item.position < 0) throw new Error('菜单 position 必须为非负整数');
      const position = JSON.stringify([item.parentMenuCode ?? null, item.position]);
      if (positions.has(position)) throw new Error('同一父节点下的菜单 position 不能重复');
      positions.add(position);
      items.set(item.menuCode, item);
    }
    for (const item of items.values()) {
      const visited = new Set([item.menuCode]);
      let parent = item.parentMenuCode;
      while (parent !== undefined && parent !== null) {
        if (visited.has(parent) || !items.has(parent) || menus.get(parent)?.menuType !== 'Group') throw new Error(`父菜单不存在、不是 Group 或形成循环：${parent}`);
        visited.add(parent);
        parent = items.get(parent).parentMenuCode;
      }
    }
  }
}
export function checkProject(root) {
  const manifest = readJson(root, 'manifest.json');
  assertIdentity(manifest.id);
  assertVersion(manifest.version);
  if (manifest.manifest_version !== 1 || manifest.framework !== '^1.0.0') throw new Error('模板需要 manifest_version=1、framework=^1.0.0');
  if (typeof manifest.name !== 'string' || !manifest.name.trim()) throw new Error('缺少包名称');
  if (!Array.isArray(manifest.resources) || !Array.isArray(manifest.skills)) throw new Error('resources 和 skills 必须是数组');
  const files = new Map();
  for (const resource of manifest.resources) {
    if (['manifest.json', 'checksums.json', 'signature.json', 'receipt.json'].includes(resource)) throw new Error(`保留路径不能作为资源：${resource}`);
    files.set(resource, fs.readFileSync(safeFile(root, resource)));
  }
  for (const directory of manifest.skills) {
    collectDirectory(root, directory, files);
    const skills = [...files].filter(([name]) => name.startsWith(`${directory}/`) && name.endsWith('/SKILL.md'));
    if (!skills.length) throw new Error(`技能目录缺少 SKILL.md：${directory}`);
    for (const [name, data] of skills) {
      const frontmatter = data.toString().match(/^---\r?\n([\s\S]+?)\r?\n---(?:\r?\n|$)/)?.[1];
      if (!frontmatter || !/^name:\s*\S.*$/m.test(frontmatter) || !/^description:\s*\S.*$/m.test(frontmatter)) throw new Error(`技能缺少 name/description：${name}`);
    }
  }
  const mcp = readJson(root, 'mcp/servers.json');
  if (!Array.isArray(mcp)) throw new Error('mcp/servers.json 必须是数组');
  const ids = new Set();
  for (const server of mcp) {
    if (typeof server.id !== 'string' || !server.id.startsWith(`${manifest.id}.`) || ids.has(server.id)) throw new Error('MCP ID 必须唯一并以包 ID 加点开头');
    ids.add(server.id);
    if (server.transport === 'http') {
      const url = new URL(server.url);
      if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password) throw new Error('MCP 必须使用无内嵌凭据的 HTTP(S) URL');
    } else if (server.transport === 'stdio') {
      if (!files.has(server.program)) throw new Error('stdio MCP 程序必须列入 resources');
      safeFile(root, server.program);
      if (server.args !== undefined && (!Array.isArray(server.args) || server.args.some(value => typeof value !== 'string'))) throw new Error('MCP args 必须是字符串数组');
    } else throw new Error('MCP transport 只支持 http 或 stdio');
  }
  if (manifest.mcp?.length) throw new Error('请统一在 mcp/servers.json 维护 MCP 声明，manifest.mcp 保持空数组');
  manifest.mcp = mcp;
  const pages = readJson(root, 'tron/pages.json');
  const navigation = readJson(root, 'tron/navigation.json');
  for (const file of ['tron/pages.json', 'tron/navigation.json']) if (!files.has(file)) throw new Error(`${file} 必须列入 resources`);
  if (pages.version !== 1 || navigation.version !== 1 || !Array.isArray(pages.pages) || pages.pages.length > 32) throw new Error('页面/导航声明版本或数量不符合约定');
  const pageIds = new Set();
  for (const page of pages.pages) {
    if (!/^[A-Za-z0-9_-]{1,64}$/.test(page.id) || pageIds.has(page.id)) throw new Error('页面 ID 非法或重复');
    pageIds.add(page.id);
    const entry = `${page.root}/${page.entry}`;
    safeFile(root, entry);
    if (!files.has(entry)) throw new Error(`页面入口必须列入 resources：${entry}`);
    const pageFiles = [...files].filter(([name]) => name.startsWith(`${page.root}/`));
    if (pageFiles.length > 256 || pageFiles.reduce((total, [, data]) => total + data.length, 0) > 16 * 1024 * 1024) throw new Error('页面超过 256 文件或 16 MiB');
    const html = files.get(entry).toString();
    if ((html.match(/<script\b/gi) ?? []).length > 1 || /<(iframe|form)\b|type\s*=\s*["']module["']|\bsrcset\s*=/i.test(html)) throw new Error('页面包含不支持的元素或脚本');
    for (const [name, data] of pageFiles.filter(([name]) => /\.(html|css)$/.test(name))) {
      const source = data.toString();
      if (/\.css$/.test(name) && /@import/i.test(source)) throw new Error('页面 CSS 不支持 @import');
      const references = name.endsWith('.html') ? [...source.matchAll(/\b(?:src|href)\s*=\s*["']([^"']+)["']/gi)].map(match => match[1]) : [...source.matchAll(/url\(\s*["']?([^\s"')]+)["']?\s*\)/gi)].map(match => match[1]);
      for (const reference of references) {
        if (/^[/.]|[:?#\\]/.test(reference)) throw new Error(`页面资源须为包内相对路径：${reference}`);
        const target = path.posix.join(path.posix.dirname(name), reference);
        if (!files.has(target)) throw new Error(`页面资源未声明：${target}`);
      }
    }
  }
  checkNavigation(navigation, pageIds);
  if (manifest.backend) {
    const cargo = fs.readFileSync(safeFile(root, 'native/Cargo.toml'), 'utf8');
    if (cargo.match(/^version\s*=\s*"([^"]+)"/m)?.[1] !== manifest.version) throw new Error('native/Cargo.toml 与 manifest.json 版本不一致');
    safeFile(root, 'native/src/main.rs');
    if (!manifest.implementations?.length) throw new Error('原生入口需要 implementations');
  } else if (manifest.implementations?.length) throw new Error('implementations 需要 backend');
  return { manifest, files };
}
export function buildProject(root, requestedVersion = '') {
  const { manifest, files } = checkProject(root);
  if (requestedVersion && requestedVersion !== manifest.version) throw new Error('VERSION 必须与 manifest.json 一致');
  let suffix = 'universal';
  if (manifest.backend) {
    const cwd = path.join(root, 'native');
    if (!fs.existsSync(path.join(cwd, 'Cargo.lock'))) run('cargo', ['generate-lockfile'], cwd);
    const target = run('rustc', ['-vV'], cwd, true).match(/^host: (.+)$/m)?.[1];
    if (!target) throw new Error('无法读取 rustc host');
    const output = run('cargo', ['build', '--locked', '--release', '--target', target, '--message-format=json-render-diagnostics'], cwd, true);
    const artifact = output.split('\n').filter(Boolean).map(line => JSON.parse(line)).findLast(message => message.reason === 'compiler-artifact' && message.target?.name === 'capability-backend' && message.executable)?.executable;
    if (!artifact) throw new Error('Cargo 未返回 capability-backend 程序');
    const os = { darwin: 'macos', linux: 'linux', win32: 'windows' }[process.platform];
    const arch = { arm64: 'aarch64', x64: 'x86_64' }[process.arch];
    if (!os || !arch || !target.startsWith(arch) || (os === 'windows' ? !target.includes('windows') : os === 'macos' ? !target.includes('darwin') : !target.includes('linux'))) throw new Error('Rust host 与 Node 平台不匹配，请在目标平台原生构建');
    const executable = `bin/backend${os === 'windows' ? '.exe' : ''}`;
    manifest.backend.executables = { [`${arch}-${os}`]: executable };
    manifest.tools = JSON.parse(run(artifact, ['catalog'], root, true));
    if (!Array.isArray(manifest.tools)) throw new Error('catalog 必须返回工具数组');
    files.set(executable, fs.readFileSync(artifact));
    suffix = `${os}-${arch}`;
  }
  files.set('manifest.json', Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`));
  const bytes = archive(files);
  const output = path.join(root, 'output');
  if (fs.existsSync(output) && fs.lstatSync(output).isSymbolicLink()) throw new Error('output 不能是符号链接');
  fs.mkdirSync(output, { recursive: true });
  const destination = path.join(output, `${manifest.id}-${manifest.version}-${suffix}.zip`);
  // Exclusive creation also refuses a symlink destination; versioned outputs are never overwritten.
  fs.writeFileSync(destination, bytes, { flag: 'wx' });
  console.log(`已生成 ${destination}`);
  return destination;
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  try {
    const [command, ...extra] = process.argv.slice(2);
    if (extra.length) throw new Error('不支持额外参数');
    if (command === 'check') { checkProject(process.cwd()); console.log('能力包静态校验通过'); }
    else if (command === 'build') buildProject(process.cwd());
    else throw new Error('usage: node scripts/pack.mjs check|build');
  } catch (error) { console.error(`错误：${error.message}`); process.exitCode = 1; }
}
