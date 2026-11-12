import fs from 'node:fs';
import path from 'node:path';
import { buildProject, checkProject, run } from './pack.mjs';

const root = process.cwd();
const command = process.argv[2];
try {
  if (Number(process.versions.node.split('.')[0]) < 22) throw new Error('需要 Node.js >= 22');
  if (!['true', 'false'].includes(process.env.CAP_SKIP_TESTS ?? 'false')) throw new Error('SKIP_TESTS 只接受 true 或 false');
  const version = process.env.CAP_VERSION ?? '';
  const native = path.join(root, 'native');
  switch (command) {
    case 'help':
      console.log('make help/doctor/install/clean/format/lint/test/build/dev/deploy\ndoctor 检查环境；install 拉取 Rust 依赖；lint 静态及编译检查；build 生成 ZIP；format 格式化 Rust；clean 清理构建产物。\n本模板没有自动测试、常驻开发服务或发布仓库；test 跳过，dev/deploy 明确报错。\nVERSION 留空或与 manifest.json 相同；SKIP_TESTS=true/false（构建不隐含运行测试）。');
      break;
    case 'doctor':
      checkProject(root);
      run('node', ['--version'], root);
      run('cargo', ['--version'], root);
      run('rustc', ['--version'], root);
      console.log('警告：尚未配置远程制品发布；模板静态校验通过。');
      break;
    case 'install':
      if (!fs.existsSync(path.join(native, 'Cargo.lock'))) run('cargo', ['generate-lockfile'], native);
      run('cargo', ['fetch', '--locked'], native);
      break;
    case 'format': run('cargo', ['fmt'], native); break;
    case 'lint':
      checkProject(root);
      for (const script of ['pack.mjs', 'archive.mjs', 'project.mjs']) run('node', ['--check', `scripts/${script}`], root);
      run('cargo', ['fmt', '--', '--check'], native);
      if (!fs.existsSync(path.join(native, 'Cargo.lock'))) throw new Error('请先执行 make install 生成并保留 native/Cargo.lock');
      run('cargo', ['check', '--locked'], native);
      break;
    case 'build': buildProject(root, version); break;
    case 'clean':
      run('cargo', ['clean'], native);
      fs.rmSync(path.join(root, 'output'), { recursive: true, force: true });
      break;
    case 'test': console.log('跳过：未配置自动测试。'); break;
    case 'dev': throw new Error('本模板没有常驻开发服务，请构建后导入 Tron 调试');
    case 'deploy': throw new Error('尚未配置发布仓库，请按用户指示通过 Tron 能力中心发布 ZIP');
    default: throw new Error('未知命令，请执行 make help');
  }
} catch (error) { console.error(`错误：${error.message}`); process.exitCode = 1; }
