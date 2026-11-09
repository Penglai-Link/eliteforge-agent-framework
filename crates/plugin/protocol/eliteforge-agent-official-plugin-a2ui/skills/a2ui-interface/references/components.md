# Selecting A2UI components

Read this guide when choosing an interface. Then read `catalog.json` through `skill_read_resource`; replace `{component}` in each catalog's `componentResourceTemplate` with the selected name to read its schema. Availability is determined by negotiation, not this guide. The bundled common catalog contains 79 components; the Agent catalog adds two built-in interaction compositions.

## Built-in compositions

| Task | Composition | Content and behavior |
| --- | --- | --- |
| Ask a question or a short questionnaire | Native ask-user → `QuestionCard` | Questions, option labels/descriptions, custom-answer and skip settings. The client fixes the title/navigation row, numbered choices and inline answer footer. |
| Review a tool approval | Native tool policy → `ApprovalCard` | The host projects prompt, tool details, decision and lifecycle. A UI action is not permission to execute a tool. |

Do not assemble either interaction from primitive controls. Prefer the native tool so suspension, answer validation and resume remain authoritative. If the host has no native ask-user tool, use `QuestionCard` only when negotiated and read its schema first; a SubmitOnce submit action must still bind `context.response` to a response model matching `responseSchema`. Its answer value is a map keyed by `questionId`, with `answerKind` (`Option`, `Other`, `Skipped`), `optionId`, `otherText` and `questionId` in each entry. It is not a `ChoicePicker` array. An approval composition never replaces tool policy.

## Atomic component families

| Need | Components |
| --- | --- |
| Layout and grouping | `Row`, `Column`, `Grid`, `Spacer`, `Divider`, `Card`, `AspectRatio`, `Direction`, `ScrollArea`, `Resizable`, `Sidebar`, `ButtonGroup`, `InputGroup`, `Item` |
| Text and labels | `Text`, `Label`, `Kbd`, `Marker`, `Badge`, `Avatar`, `Icon` |
| Input | `TextField`, `Textarea`, `NumberField`, `InputOTP`, `CheckBox`, `Switch`, `Slider`, `ChoicePicker`, `Select`, `Combobox`, `Command`, `Toggle`, `ToggleGroup`, `DateTimeInput`, `Calendar`, `DatePicker`, `Field` |
| Actions and navigation | `Button`, `Link`, `Breadcrumb`, `NavigationMenu`, `Pagination`, `DropdownMenu`, `ContextMenu`, `Menubar` |
| Progressive disclosure | `Tabs`, `Accordion`, `Collapsible`, `Popover`, `HoverCard`, `Tooltip`, `Modal`, `Drawer`, `Sheet`, `AlertDialog`, `Carousel` |
| Data and statistics | `Table`, `DataTable`, `List`, `DescriptionList`, `Statistic`, `Trend`, `Sparkline`, `Meter`, `Chart` |
| Feedback and media | `Alert`, `Toast`, `Progress`, `Spinner`, `Skeleton`, `EmptyState`, `Image`, `Video`, `AudioPlayer`, `Attachment`, `Bubble`, `Message`, `MessageScroller` |

`Card` is an optional visual container; it is not a substitute for a task-specific composition. Keep custom forms in a plain `Column` unless a boundary is useful. Overlay controls are appropriate when the interaction needs an overlay, not as default chat-form wrappers.

## Values and bindings

- Editable controls require `value: {"path":"/..."}` and an initial model value of the correct type. Keep form values under a response subtree; submit that subtree instead of labels, navigation state or report data.
- `TextField`, `Textarea`, `InputOTP`, `Select`, `Combobox` and `Command` use strings; `NumberField`, `Slider` and `Pagination` use numbers; `Switch`, `CheckBox` and `Toggle` use booleans. Check each schema for constraints.
- `TextField.placeholder` accepts a string or `{ "path": "/hint" }` binding in both bundled catalogs. It is shown only when the value is empty and never becomes submitted data. Keep `label` for accessibility; put initial input data in `value`, not `placeholder`. Check the negotiated schema when using an older client.
- `ChoicePicker` uses a string array, including single choice. `ToggleGroup` also uses an array. Make the response schema agree with the actual value.
- `Calendar` and `DatePicker` use date-only `YYYY-MM-DD` strings. `DateTimeInput` chooses date/time fields via its schema; do not assume every date control emits an ISO timestamp.
- `Field` wraps a `child` and adds `label`, optional `description` and `error`. Avoid repeating the same visible label on the child.
- Children are component IDs, not inline JSX objects. Use `child`, `children`, `tabs` or other schema-defined references as appropriate. Dynamic values use supported path bindings; expressions are unsupported.

## Statistics

| Need | Component and value shape |
| --- | --- |
| One number with formatting | `Statistic`: numeric `value`, optional `label`, `precision`, `grouping`, `prefix`, `unit` |
| Change or delta | `Trend`: numeric `value`, optional `unit`, `precision`, `positiveIsGood` |
| Compact trend line | `Sparkline`: `values` array of numbers/nulls, optional `label` |
| Value against a range | `Meter`: numeric `value`, optional `min`, `max`, `low`, `high`, `optimum`, `unit` |
| Comparison or time series | `Chart`: `type` (`line`, `bar`, `area`, `pie`, `donut`), string `labels`, `series` array of `{name, values}` |
| Key/value facts | `DescriptionList`: `items` array of `{label, value}`, where value is a string, number, boolean or null |

Bind changing data to model paths. Keep chart labels and series lengths consistent. For pie/donut use a meaningful nonnegative series. Do not generate fake data to fill a component. Use the smallest display that answers the user; keep unrelated prose outside the surface.
