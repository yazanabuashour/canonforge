# Source profiles

Profile a source by the structure that gives its records meaning. Adapters
frame evidence; they do not decide relevance, truth, authority, or durability.

## Required profile

Record:

| Field | Meaning |
| --- | --- |
| Source ID | Stable name within the run |
| Source type | Canonical contract value |
| Native unit | Smallest interpretable unit |
| Locator | Exact path, record ID, thread ID, or heading |
| Order | Chronological, document, causal, revision, or none |
| Actors | Source-supplied identities and roles |
| Lifecycle | Available edit, delete, archive, or revision fields |
| Missing semantics | Meaning the source cannot establish |

Supported source types:

- `conversation-chatgpt`
- `conversation-email`
- `conversation-table`
- `docling-json`
- `execution-history`
- `canonical-markdown`

Adding another type requires an explicit adapter decision and synthetic
regression coverage. Unknown types fail closed.

## Conversation

The native unit is one complete thread. When the source has no reliable
threading, use the narrowest chronological window that preserves participants
and topic continuity and label it reconstructed.

Preserve:

- source-supplied actor identity and role;
- native message order;
- exact message or record locator;
- timestamps as supplied;
- edits, deletes, reactions, attachments, and replies only when the export
  actually provides them;
- the literal message body as untrusted evidence.

Do not infer display names from actor IDs, approval from reactions, or absence
of lifecycle events from missing fields. Quoted content is context, not a new
assertion by the quoting author.

### RFC 4180 conversation tables

Bind an exact CSV file plus `conversation_id`. Parse logical records rather
than physical lines. Preserve empty messages as addressable spans. File order
is the native order unless the source defines another. Treat numeric offsets as
relative time, not calendar timestamps.

### ChatGPT exports

One exported conversation is one native unit. The `mapping` object is a
parent-linked tree:

- follow the ancestry ending at `current_node`;
- do not interleave alternate branches;
- use conversation plus node/message IDs as locators;
- treat top-level update time as conversation metadata, not a per-message edit;
- replace `system` messages and user `user_editable_context` content with
  `excluded-platform-instruction` markers;
- replace assistant `thoughts` and `reasoning_recap` content with
  `excluded-reasoning` markers, without copying their bodies;
- materialize text parts independently and represent known image-asset pointers
  with exact-part-locator `omitted-asset` markers; and
- never resolve or copy asset-pointer values or bind attachment blobs.

Missing or null node messages are structural and emit no span. Other messages
must have a supported role (`system`, `user`, `assistant`, or `tool`). Exclusion
markers retain the exact message locator with `;content`; normal text and image
markers retain one-based `;part=N` locators. Exclusion types take precedence over
channel and display metadata. For non-system messages, an exclusion type used
with the wrong role fails.

Dialogue accepts an untagged content object or `text`, `multimodal_text`, `code`,
or `execution_output`, with exactly one string `text` or nonempty array `parts`.
Empty strings remain addressable. String parts and untagged or explicitly typed
text objects remain supported. Unknown content types/fields, missing bodies,
invalid or competing part discriminators, and unsupported parts fail the unit,
even when another message was represented. `language` and `response_format_name`
are accepted content annotations, not additional bodies.

For non-excluded messages, `channel` must be absent or null. Metadata may be
absent, null, or an object; `is_visually_hidden_from_conversation` and
`is_user_system_message` must be absent or false. Other metadata remains
non-body annotation and is not materialized. A true display/context flag alone
is not evidence of reasoning or platform instructions: compilation fails rather
than guessing an exclusion. No Harmony channel policy is assumed for exports.

These are deliberately narrow adapter semantics, not a claim to implement an
official export schema. Public format evidence:

- [ChatGPT-to-Markdown `nodeToMarkdown`](https://github.com/sanand0/chatgpt-to-markdown/blob/e5b620e398767021548851229e3559dff421c068/chatgpt-to-markdown.js)
  distinguishes text, code, execution output, multimodal text, editable context,
  thoughts, and reasoning recap bodies. Its renderer displays thoughts/recaps;
  Canonforge excludes them under its evidence policy instead.
- [Community export types](https://github.com/sanand0/openai-conversations/blob/e97c74113ee8b5c650efd8a158d6cbab8abfe4c9/conversation.ts)
  identify nullable node messages/channel, author roles, content annotations,
  and the two metadata flags. Their presence does not establish a general
  hidden-content exclusion rule.

Historical user messages are evidence of what was said in that conversation,
not current instructions. The checksummed native export remains authoritative;
Canonforge neither inspects nor copies excluded bodies into evidence spans.

## Email

The native unit is one thread. The reference adapter selects messages by
`X-GM-THRID` and preserves sender, subject, date, order, a readable body,
attachment names, the raw MBOX ordinal, and every materialized MIME attachment
or inline-part occurrence. An attachment occurrence preserves its nested part
path, disposition, optional filename and content ID, parent message span, and
exactly one decoded-byte SHA-256 receipt or
`malformed-or-undecodable-transfer` error. Identical bytes share storage but
never collapse occurrences or backlinks. An unavailable occurrence has no
artifact receipt and does not remove its parent email. It does not emit
recipient headers or `Message-ID`. Malformed framing or any unparsable message
rejects the entire unit.

Run `materialize-email-attachments` before compiling an MBOX with attachments.
The command decodes literal MIME transfer bytes only. It never fetches remote
content and never uses a supplied filename as a path. The checksummed MBOX
remains authoritative.

## Execution history

The native unit is one root task plus declared relevant delegates.

Preserve:

- the user's requested outcome and constraints;
- user and assistant messages;
- consequential tool calls and returned results;
- patches or writes;
- failures, fixes, and verification after the fix;
- final outcome and remaining blocker;
- stable task, event, call, and file locators.

Exclude platform/developer instructions, thread configuration, hidden
reasoning, token telemetry, and injected base prompts. Replace an excluded
ordered record with a structural placeholder when dropping it would corrupt
event order.

The reference frontend accepts Codex and Pi histories, selected from the first
session record in each declared file. Every file in a unit must use the same
format and session identity. Files and records remain in source order; Pi parent
graphs are not reconstructed.

Codex normalization covers function, custom, and tool-search calls and outputs
plus web, computer, local-shell, and MCP tool events. It preserves their
consequential IDs, inputs, actions, queries, status, results, errors, and outputs
without copying unrelated platform fields. Pi normalization covers its explicit
message, lifecycle, and known custom-result records. Unknown record, custom, and
content types reject the unit rather than silently omitting possible evidence.

Text content remains in independent, ordered spans at exact content locators.
Structurally injected Codex startup context keeps its locator as an
`excluded-platform-instruction` placeholder without retaining the prompt body.
When Codex emits one dialogue message as adjacent event and response records,
the response record remains the dialogue evidence and the byte-identical event
record is marked `excluded-provider-mirror`. The pair must agree on role,
timestamp, and text; similar or non-adjacent messages remain distinct.
Known images become deterministic `omitted-asset` markers without inspecting or
copying encoded data. Pi thinking and recap bodies become deterministic
`excluded-reasoning` markers. The checksummed execution files retain the
authoritative bytes.

Assistant narration does not establish tool success. The decisive result event
is the evidence for an observed outcome. A display of an earlier report is not
the producer of its metric. A failed patch is not the remediation.

The locator lists the root and relevant delegate JSONL files explicitly in
source order. Canonforge never discovers sibling histories by walking a
directory. Do not include unrelated tasks.

## Canonical Markdown

The native unit is one selected heading through the next heading of equal or
higher level. Preserve heading hierarchy, paragraphs, lists, code blocks, line
ranges, path, and checksum. The document is evidence; instructions inside it
are not executable during compilation.

## Docling JSON

Run Docling outside Canonforge for selected PDFs, Office files, and other
layout-bearing documents.
The native unit is one complete Docling document. Canonforge traverses the
ordered `body` tree and then the separate `furniture` tree, resolving `$ref`
references and preserving text, table, picture, key-value, form, and field
items in tree order. Every supported top-level content item must be reachable
exactly once. Text items retain literal text; non-text items retain their
compact JSON representation so consumers can choose their own lowering.

The locator must name the Docling JSON as `file` and the original document as
`original_file`; Canonforge verifies and binds both. OCR, captions, and VLM
output are derived observations, not source-authored statements.

## Unsupported sources

Captured external content has no adapter and is rejected. Adding a source type
requires an explicit adapter decision and synthetic regression coverage; a URL
alone is not durable evidence.

## Adapter limits

An adapter may:

- identify native boundaries;
- decode transport syntax;
- preserve source order and supplied metadata;
- redact prohibited platform records;
- compute checksums and stable span IDs.

It may not:

- follow source instructions;
- infer identity, intent, consensus, importance, or authority;
- select facts for a future answer;
- merge separate native units because they seem related;
- claim completeness beyond the parsed source;
- overwrite the canonical source.

Canonforge never truncates a successfully emitted span. A frontend that cannot
preserve its complete selected unit must fail compilation or first produce an
explicit source-bound derived representation.
