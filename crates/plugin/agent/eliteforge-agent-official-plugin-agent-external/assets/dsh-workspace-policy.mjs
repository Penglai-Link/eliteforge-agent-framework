import { loadDsh } from "./dsh-workspace-state.mjs";
const { default: SandboxPolicy } = await loadDsh("@deepseek-ai/dsh-sandbox-policy");

// Keep the user's standing mode, per-session overrides and escalation semantics.
// The grouping directory must never become the standing write boundary.
export default class ExternalWorkspacePolicy extends SandboxPolicy {
  static inject = ["sessionProjections", "loader"];
  constructor(ctx, config) {
    // In dsh patches, `name` asserts a plugin's name; it cannot replace one.
    // The overlay disables the native entry and mounts this subclass instead.
    // Read that entry's effective config, evaluating expressions in its context,
    // so user permission modes survive without copying profile configuration.
    const entry = [...ctx.loader.entries()].find(entry => entry.options.id === "sandbox-policy");
    if (!entry) throw new Error("dsh native sandbox-policy configuration is unavailable");
    const raw = entry.options.config?.mode;
    const mode = (raw && typeof raw === "object" && typeof raw.__jsExpr === "string"
      ? entry.evaluate(raw.__jsExpr) : raw) ?? config.mode;
    if (!["read-only", "workspace-write", "danger-full-access"].includes(mode)) {
      throw new Error("dsh native sandbox mode is invalid");
    }
    super(ctx, { ...config, mode, workspaceRoot: process.env.ELITEFORGE_DSH_WORKING_DIRECTORY });
  }
  resolve(request = {}) {
    const policy = super.resolve(request);
    if (policy.workspaceRoot === process.env.ELITEFORGE_DSH_WORKSPACE_ROOT) {
      policy.workspaceRoot = process.env.ELITEFORGE_DSH_WORKING_DIRECTORY;
    }
    return policy;
  }
}
