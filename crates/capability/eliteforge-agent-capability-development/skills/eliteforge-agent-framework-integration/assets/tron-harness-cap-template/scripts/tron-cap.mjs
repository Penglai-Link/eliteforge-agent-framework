#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { checkProject, assertIdentity, assertVersion } from './pack.mjs';

const scripts = path.dirname(fileURLToPath(import.meta.url));
const template = path.resolve(scripts, '../template');
const usage = `Tron 能力包完整模板
  tron-cap create --id com.example.assistant --name "示例助手" --out ./example-assistant [--version 0.1.0]
预建 skills、native、ui、tron、mcp、resources、scripts 目录；拒绝覆盖已有目录。
需要 Node.js >= 22，构建原生工具另需 Rust/Cargo。`;

function create(options) {
  const id = options.id;
  const name = options.name ?? id;
  const version = options.version ?? '0.1.0';
  assertIdentity(id);
  assertVersion(version);
  if (!options.out) throw new Error('必须指定 --out');
  if (typeof name !== 'string' || !name.trim() || name.length > 120 || /[\x00-\x1f\x7f]/.test(name)) {
    throw new Error('名称必须是 1–120 个字符的单行文字');
  }
  const slug = id.replaceAll('.', '-');
  const values = { PACK_ID: id, PACK_SLUG: slug, PACK_VERSION: version, TOOL_NAME: `${id.replaceAll(/[.-]/g, '_')}_echo` };
  const render = text => text.replace(/__([A-Z_]+)__/g, (match, key) => {
    if (!(key in values)) throw new Error(`未知模板参数：${match}`);
    return values[key];
  });
  const files = new Map();
  function collect(directory, relative = '') {
    for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
      if (entry.isSymbolicLink()) throw new Error('模板不能包含符号链接');
      const source = path.join(directory, entry.name);
      const target = path.posix.join(relative, render(entry.name).replace(/\.tpl$/, ''));
      if (entry.isDirectory()) collect(source, target);
      else files.set(target, render(fs.readFileSync(source, 'utf8')));
    }
  }
  collect(template);
  for (const file of ['pack.mjs', 'archive.mjs', 'project.mjs']) files.set(`scripts/${file}`, fs.readFileSync(path.join(scripts, file)));
  const json = value => `${JSON.stringify(value, null, 2)}\n`;
  files.set('manifest.json', json({
    manifest_version: 1, id, name, version, framework: '^1.0.0',
    skills: ['skills'],
    backend: { executables: { native: 'bin/backend' }, args: [] },
    implementations: [{ id: `${id}.tools`, extension: 'framework.tool-provider', contract: '^1.0.0' }],
    mcp: [], plugins: [], extension_points: [], permissions: [], dependencies: [],
    resources: ['README.md', 'resources/guide.md', 'tron/pages.json', 'tron/navigation.json', 'ui/index.html', 'ui/page.css', 'ui/page.js'],
  }));
  files.set('tron/pages.json', json({ version: 1, pages: [{ id: 'overview', title: { 'zh-CN': name, 'en-US': name }, root: 'ui', entry: 'index.html' }] }));
  const domain = `${id.replaceAll(/[.-]/g, '_').toUpperCase()}_DOMAIN`;
  const menu = `${id.replaceAll(/[.-]/g, '_').toUpperCase()}_OVERVIEW`;
  files.set('tron/navigation.json', json({
    version: 1, domains: [{ code: domain, name: { 'zh-CN': name, 'en-US': name } }],
    menus: [{ code: menu, name: { 'zh-CN': '概览', 'en-US': 'Overview' }, menuType: 'PackPage', pageCode: 'overview' }],
    layouts: { [domain]: [{ menuCode: menu, position: 0 }] },
  }));
  const destination = path.resolve(options.out);
  fs.mkdirSync(path.dirname(destination), { recursive: true });
  // Non-recursive mkdir provides exclusive ownership; never write into an existing directory.
  fs.mkdirSync(destination);
  try {
    for (const [relative, content] of files) {
      const output = path.join(destination, relative);
      fs.mkdirSync(path.dirname(output), { recursive: true });
      fs.writeFileSync(output, content, { flag: 'wx' });
    }
    checkProject(destination);
  } catch (error) {
    fs.rmSync(destination, { recursive: true, force: true });
    throw error;
  }
  console.log(`已生成完整能力包项目：${destination}\n接下来填写业务实现，再执行 make install、make lint 和 make build。`);
}

try {
  if (Number(process.versions.node.split('.')[0]) < 22) throw new Error('需要 Node.js >= 22');
  const { positionals, values } = parseArgs({ allowPositionals: true, options: {
    id: { type: 'string' }, name: { type: 'string' }, out: { type: 'string' }, version: { type: 'string' }, help: { type: 'boolean' },
  } });
  if (values.help || positionals.length === 0 || positionals[0] === 'help') console.log(usage);
  else if (positionals.length === 1 && positionals[0] === 'create') create(values);
  else throw new Error(usage);
} catch (error) {
  console.error(`错误：${error.message}`);
  process.exitCode = 1;
}
