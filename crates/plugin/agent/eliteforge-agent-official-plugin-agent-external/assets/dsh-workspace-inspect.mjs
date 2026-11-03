import { bindingOf } from "./dsh-workspace-state.mjs";
export const name = "eliteforge-dsh-workspace-inspect";
export const inject = ["sessionQuery"];
export function apply(ctx, config) {
  (async () => {
    await ctx.get("loader")?.await();
    const observation = await ctx.sessionQuery.observeSession(config.sessionId);
    try {
      process.stdout.write(JSON.stringify({
        cwd: observation.header.cwd,
        binding: bindingOf(observation.events) ?? null,
      }) + "\n");
    } finally { observation[Symbol.dispose](); }
    ctx.appExit(0);
  })().catch(error => { process.stderr.write(String(error)); ctx.appExit(1); });
}
