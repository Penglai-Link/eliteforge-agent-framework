export const name = "eliteforge-dsh-model-catalog-probe";
export const inject = ["llm"];
export function apply(ctx) {
  (async () => {
    await ctx.get("loader")?.await();
    const models = [];
    const errors = [];
    await Promise.all(ctx.llm.listProviders().map(async provider => {
      try {
        const entries = await ctx.llm.listModels(provider.id);
        await Promise.all(entries.map(async entry => {
          let info = entry;
          try { info = await ctx.llm.resolveModelInfo(provider.id, entry.id); }
          catch (error) { errors.push(`${provider.id}/${entry.id}: ${error.message}`); }
          models.push({provider: provider.id, model: entry.id, displayName: info.name,
            contextWindowTokens: info.context?.contextWindow,
            reasoningEfforts: info.reasoning?.efforts.map(e => e.id) ?? []});
        }));
      } catch (error) { errors.push(`${provider.id}: ${error.message}`); }
    }));
    process.stdout.write(JSON.stringify({models, errors}) + "\n");
    ctx.appExit(0);
  })().catch(error => { process.stderr.write(String(error)); ctx.appExit(1); });
}
