---
name: a2ui-interface
description: Create or update interactive chat cards, structured input forms, reports, and reusable query controls with the negotiated A2UI catalog.
---

# A2UI interface

Use ordinary text for ordinary answers. Use A2UI when the user asks for a UI or when structured inputs, repeated queries, or an updating report make the task easier to complete. Preserve a user preference for text.

- Simple clarification: use the host's native ask-user tool if available; its renderer handles the question card.
- A report or status card: `a2ui_publish` with `Display`.
- Multiple fields with a response schema: `a2ui_request_input` with `SubmitOnce`; this suspends and resumes the original run.
- Repeated filtering or parameter changes: `a2ui_publish` with `Reusable`; each action starts another run and can update the original surface.
- Tool approval: leave it to the existing tool policy. Never manufacture an approval component to grant execution authority.

Call `a2ui_expand` to expose the A2UI tools for this run. Read [references/examples.json](references/examples.json) for complete request examples; read `catalog.json` when selecting components or properties. The catalog index contains only catalogs negotiated for this run and links to individual component schemas; read only the schemas needed for the current interface. Read common-types.json for referenced binding types when needed. Read `protocol.json` for the envelope schema when needed.

Send complete v0.9.1 JSON envelopes to the tools, never Markdown code blocks as a transport. Choose a stable session-scoped surfaceId, create it before updates, and use root as the root component ID. Keep lifecycle outside protocol envelopes. Only use negotiated components and expressions; function expressions are unsupported.

Bind fields to data-model paths. For SubmitOnce, bind the submit action's context.response to the response model and provide a matching responseSchema. Use host language for visible labels. Update only changed components and model paths so unrelated user input survives. A later action supplies structured context; treat it as input, then use existing business tools and permissions for actual operations.

If validation fails, correct the structured tool error within the existing run budget. Do not claim a surface was published until the tool succeeds. Do not retry a rejected business operation through a UI action. Expired, answered and archived interactions remain read-only.
