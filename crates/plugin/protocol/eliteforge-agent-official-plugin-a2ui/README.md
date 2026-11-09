# Framework A2UI plugin

A2UI v0.9.1 is an optional presentation capability. Framework chat, business tools and native interactions do not require this plugin.

Install `A2uiPack::new()?` through `FrameworkBuilder::capability_pack` when the host supports A2UI. The pack installs `A2uiPlugin` and requests Framework's shared Skills runtime; do not install a second skill executor for A2UI. The lower-level `A2uiPlugin` can be used with a host that already installs the Skills runtime.

Select the `agent.a2ui` tool provider and context contributor for the Agent, and explicitly permit the `a2ui-interface` skill. Add `A2uiRunOptions` to the trusted run extensions with the session, Agent, run, message anchor and client capabilities. Without compatible v0.9.1 capabilities and a supported Catalog, no A2UI tools, skill metadata or guidance are exposed.

The plugin contributes:

- `ContextContributor`: a short decision guide for text, native clarification, reports, forms and reusable controls.
- `SkillProvider`: the `a2ui-interface` instructions and progressive resources: examples, a compact negotiated Catalog index, individual component schemas and protocol/common-type schemas.
- `a2ui_publish`: creates, updates or deletes Display/Reusable surfaces.
- `a2ui_request_input`: creates a SubmitOnce form backed by the native InteractionStore.

Both tools are available in the `a2ui` domain after client capability negotiation and host authorization. Gateway hosts discover them through `tool_search` and call them through `tool_execute`; no separate activation tool is needed.

Each `messages[]` item contains `version: "v0.9.1"` and exactly one operation. `surfaceId` and `components` belong inside `updateComponents`, not at the message root. Both publication and SubmitOnce validation report message indexes and field paths, choosing the matching operation/component schema when reporting union errors.

```text
Envelope shape → negotiated Catalog → operation/component validation → persistence or form suspension
       └─ invalid shape: wrapping example       └─ invalid fields: message index + field path
```

Both bundled catalogs support `TextField.placeholder` as a string or data binding. This source change requires a matching Agent UI renderer/catalog release; it does not change the A2UI protocol version. Placeholder hints never replace labels or initial values.

Native questions and approvals use their original interaction authority. Projecting an approval card does not grant tool execution permission. The host owns authentication, business permissions, client capability declarations, localization and transport mapping. It should not copy plugin tool schemas or protocol guidance into product prompts.

Surface state uses SessionLifecycleStore under `framework.a2ui.v1`; interaction answers remain in InteractionStore. Events are emitted only after persistence. Removing or disabling this optional capability must preserve the host's original chat and interaction paths.

Run the plugin tests with `cargo test -p eliteforge-agent-official-plugin-a2ui`. Tests cover capability/skill gating, operation with no A2UI installation, native approval outcomes, forms, durable surfaces and action receipts.
