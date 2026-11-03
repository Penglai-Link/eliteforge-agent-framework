import { realpath, stat } from "node:fs/promises";
import { bindingOf, bindingType, loadDsh } from "./dsh-workspace-state.mjs";
const { appendPluginRecord } = await loadDsh("@deepseek-ai/dsh-session");
export const name = "eliteforge-dsh-workspace";
export const inject = ["workspaceRegistry", "workingDirectory", "sandboxPolicy", "agents", "sessions", "headlessStartup"];

async function existingDirectory(path) {
  if (!(await stat(path)).isDirectory()) throw new Error(`dsh working directory is unavailable: ${path}`);
  return realpath(path);
}

export function apply(ctx, config) {
  let bound = false;
  async function assertTaskDirectory(agent) {
    const directory = ctx.workingDirectory.get(agent.session);
    if (directory === config.root && config.directory !== config.root) {
      throw new Error("dsh task directory was replaced by the workspace grouping directory");
    }
    await existingDirectory(directory);
  }
  // agent/created is awaited by the native headless factory, unlike session/created.
  // No user task or model request is submitted until this hook succeeds.
  ctx.on("agent/created", async ({ agent, signal }) => {
    if (bound || !ctx.agents.roots().includes(agent)) return;
    if (config.sessionId && agent.session.id !== config.sessionId) return;
    const session = agent.session;
    if (session.header.cwd !== config.root) throw new Error("dsh workspace initial directory does not match");
    const binding = bindingOf(session.snapshotEvents());
    if (config.sessionId) {
      if (binding?.version !== 1 || binding.root !== config.root || binding.directory !== config.directory) {
        throw new Error("dsh session belongs to a different task directory or workspace");
      }
      // Do not let native ensure() silently restore the shared grouping directory.
      await assertTaskDirectory(agent);
    } else {
      await ctx.workingDirectory.set(agent, config.directory, signal);
      appendPluginRecord(session, bindingType, { version: 1, root: config.root, directory: config.directory });
    }
    const policyRoot = ctx.sandboxPolicy.resolve({ session }).workspaceRoot;
    if (policyRoot !== config.directory) {
      throw new Error(`dsh sandbox write boundary mismatch: expected ${config.directory}, got ${policyRoot}`);
    }
    await ctx.sessions.flush(session);
    const workspace = await ctx.workspaceRegistry.create(config.root, config.title);
    if (workspace.title !== config.title) await workspace.setTitle(config.title);
    await workspace.attachSession(session.id);
    bound = true;
    process.stdout.write(JSON.stringify({
      type: "eliteforge_workspace_ready", sessionId: session.id,
      root: config.root, directory: config.directory, title: config.title,
    }) + "\n");
  });
  // A removed directory during a later step must also fail instead of falling
  // back to the shared parent and loading another session's instructions.
  ctx.on("system-prompt/assemble", async (_assembly, context, next) => {
    if (context.agent?.session.header.cwd !== config.root) return next();
    await assertTaskDirectory(context.agent);
    const assembly = await next();
    // Native ensure() can observe a deletion between our precheck and its own
    // stat. Reject its fallback before the assembled context reaches a model.
    await assertTaskDirectory(context.agent);
    return assembly;
  }, { prepend: true });
}
