---
name: a2ui-interface
description: Create or update interactive chat cards, structured input forms, reports, and reusable query controls with the negotiated A2UI catalog.
---

# A2UI interface

Proactively consider A2UI and prefer it when interaction or a structured visual would help the user understand, compare, explore, make a choice, or complete a task. Use supported controls for structured input, repeated queries and updating reports, and charts or statistics for suitable real data. The user need not explicitly request a UI. Use ordinary text when it is clearer, including simple answers, and respect an explicit preference for text. Choose the smallest useful interface; avoid decorative or nested cards. Use a plain Column with concise labels for necessary forms.

- Clarification or questionnaires: use the host's native ask-user tool if available. It supplies the fixed `QuestionCard` (AskUser) composition; provide questions, options and free-text/skip settings.
- A report or status card: `a2ui_publish` with `Display`.
- Multiple fields with a response schema: `a2ui_request_input` with `SubmitOnce`; this suspends and resumes the original run.
- Repeated filtering or parameter changes: `a2ui_publish` with `Reusable`; each action starts another run and can update the original surface.
- Tool approval: leave it to the existing tool policy. Never manufacture an approval component to grant execution authority.

Prefer the Agent catalog's built-in compositions: `QuestionCard` for AskUser and `ApprovalCard` for native approval projections. The client fixes spacing, numbered choices, navigation and the inline answer footer. Do not recreate these with atomic controls. Agents provide content and bindings; the renderer and host theme own appearance. Never invent CSS or layout properties absent from the schema. Publishing an approval card never grants tool authority.

Discover A2UI tools in the `a2ui` domain using the host's tool discovery flow. Read [references/components.md](references/components.md) for component selection and value shapes, and [references/examples.json](references/examples.json) for complete requests including structured forms and statistics. Read `catalog.json` to confirm availability: each negotiated catalog lists component names and a `componentResourceTemplate`; replace `{component}` with the exact selected name to read its schema. Load only needed schemas. Read `common-types.json` for binding types and `protocol.json` for envelope details when needed.

Pass messages as a JSON array and responseSchema as a JSON object, never JSON-encoded strings. Send complete v0.9.1 JSON envelopes to the tools, never Markdown code blocks as a transport. Choose a stable session-scoped surfaceId, create it before updates, and use root as the root component ID. Keep lifecycle outside protocol envelopes. Only use negotiated components and expressions; function expressions are unsupported.

Each item has exactly `version` and one operation. For example: `{"version":"v0.9.1","updateComponents":{"surfaceId":"form","components":[...]}}`. Never flatten `surfaceId`, `catalogId` or `components` onto the message root. Start a SubmitOnce form with a separate `createSurface` envelope. Validation errors identify the message index and field path; repair that operation or component instead of adding an unrelated operation.

Bind fields to data-model paths. For SubmitOnce, bind the submit action's context.response to the response model and provide a matching responseSchema. Use host language for visible labels. Update only changed components and model paths so unrelated user input survives. A later action supplies structured context; treat it as input, then use existing business tools and permissions for actual operations.

Treat responseSchema as the authoritative form contract. List mandatory properties in required; nonempty text also needs minLength: 1, required selections need minItems: 1, and required consent needs const: true. Put numerical limits and precision in minimum/maximum/multipleOf (or type: integer), keeping component hints consistent. Slider defaults to integer step 1; declare step for fractional controls. Clients derive required stars and validate the response with the same schema as the server. Prefer JSON Schema 2020-12; format assertions are enabled. Use internal references only.

Use `Field` around custom inputs, and choose data/feedback/navigation primitives from the negotiated catalog. `Statistic`, `Trend`, `Sparkline`, `Meter`, `Chart` and `DescriptionList` support structured summaries when useful. A2UI properties follow its schemas, not React/shadcn JSX props.

If validation fails, correct the structured tool error within the existing run budget. Do not claim a surface was published until the tool succeeds. Do not retry a rejected business operation through a UI action. Expired, answered and archived interactions remain read-only.
