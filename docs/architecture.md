# Architecture

Canonforge compiles frozen sources into a backend-neutral evidence package.
The checksummed native source remains canonical; evidence spans are addressable
reading views, not replacements for the source.

```text
frozen sources
  -> source-specific frontends
  -> source-bound native units and ordered spans
  -> validated, atomically published package
  -> independently chosen downstream projections
```

For compilation steps, see [Compile knowledge](../skill/compile-knowledge/SKILL.md).
For each frontend's selection, locators, and exclusions, see
[Source profiles](../skill/compile-knowledge/references/source-profiles.md).

## Product boundary

Compilation starts with a source assignment, an owner-private source root, and
checksums. It ends after package publication, validation, and inspection.
A feature belongs in Canonforge when it represents a source faithfully and
reproducibly or validates the file contract.

Downstream consumers own chunking, embeddings, storage, authorization,
retrieval, ranking, derived-state inference, and generated answers. Their IDs,
chunks, scores, and answers never become Canonforge evidence. Canonforge has no
database, query or serving lifecycle, runtime plugins, or backend abstraction.
The versioned file contract is the integration boundary.

Source content, embedded prompts, tool output, optical character recognition
(OCR) results, and parser output are untrusted data. Frontends decode transport
syntax, preserve native order and source-supplied metadata, and exclude
prohibited platform records. They do not
decide relevance, authority, truth, consensus, or importance. Canonforge never
silently truncates evidence: an unrepresentable unit must fail or use an
explicit source-bound derived representation.

## Evidence package

A package contains `manifest.json` and one JSON evidence-unit record per
manifest entry under `units/`. Each unit preserves one stable `unit_id`, its
native locator, every contributing source receipt, ordered spans, ordered email
attachment occurrences, and a digest over the complete unit. Spans retain exact
locators, roles, timestamps, text, and text checksums.

Each email attachment occurrence binds an exact MIME-part locator and parent
message span to either a decoded artifact receipt or an explicit
`malformed-or-undecodable-transfer` error. Duplicate bytes share an artifact,
not an occurrence. Non-email units have no attachments. Attachment
materialization adds no size limit without a measured receipt. Any future
tripwire must report its configured limit, requested bytes, and exact MIME-part
locator.

The normative contracts are:

| Contract | Schema |
| --- | --- |
| Compilation input | [Source assignment](../skill/compile-knowledge/assets/source-assignment.schema.json) |
| Package membership | [Package manifest](../skill/compile-knowledge/assets/evidence-package-manifest.schema.json) |
| Evidence record | [Evidence unit](../skill/compile-knowledge/assets/evidence-unit.schema.json) |
| `inspect` stdout | [Package inspection](../skill/compile-knowledge/assets/package-inspection.schema.json) |
| Conversation inventory receipt | [Inventory manifest](../skill/compile-knowledge/assets/conversation-inventory-manifest.schema.json) |
| Materialized email inventory | [Attachment manifest](../skill/compile-knowledge/assets/email-attachment-manifest.schema.json) |
| Materialization stdout | [Attachment receipt](../skill/compile-knowledge/assets/email-attachment-receipt.schema.json) |

[Canonical digest bytes](../skill/compile-knowledge/references/canonical-digests.md)
defines the language-independent checksum recipe and conformance vector.
Schema-valid JSON alone is not sufficient: validation also checks identities,
complete membership, source bindings, span and attachment relationships, and
digests.

## External document extraction

Canonforge accepts Docling JSON bound to its original document. It does not
invoke Docling, perform OCR or layout analysis, fetch remote content, or
materialize conversation image pointers. See the
[extraction procedure](../skill/compile-knowledge/SKILL.md#3-prepare-document-extraction)
for email artifacts.

A consumer can associate an extracted document with each email occurrence by
matching the original-file SHA-256 receipt. Unsupported binaries remain linked
artifacts rather than document assignments. Consumers decide how to handle
structural marker roles such as `omitted-asset` and `excluded-reasoning` before
indexing or retrieval.

## Platform and publication

The implementation requires Linux `openat2`, a mounted `/proc/self/fd`, and
`renameat2(RENAME_NOREPLACE)` on publication filesystems. Protected path
components may not be symlinks. Output parents must already exist and be owned
by the current user without group or other write permission. Protected input
files and source directories must be owner-only.

Package publication is atomic. A byte-identical existing package is accepted
idempotently; different contents are rejected without replacement. Public
output paths reserve lock and staging suffixes; the
[compilation guide](../skill/compile-knowledge/SKILL.md#prepare-private-paths)
describes those restrictions. These checks protect compilation, not same-user
process isolation or downstream serving. See [Security](../SECURITY.md).

## Compatibility and migration

Releases follow semantic versioning. Before 1.0, a minor release may change the
CLI or compiler behavior; patch releases remain compatible with that minor line.

Package compatibility follows `schema_version`, not the binary version.
Canonforge compiles, validates, and inspects only evidence-package schema v3;
stale and unknown versions are rejected. Incompatible file-contract changes
increment the schema. Compiler fixes may change content and digests without a
schema change, so pin the release for reproducible compilation and rebuild
consumer projections after deliberately recompiling.

Migrate v1 or v2 packages by recompiling their frozen sources into a new output
path. Validate the new package, upgrade every consumer to accept v3 attachment
availability, then switch consumer inputs. Retain the earlier package and its
matching binary for rollback. Immutable publication intentionally refuses to
replace the earlier directory in place.

## Implementation boundaries

- [`cli`](../src/cli.rs) owns command parsing and routing.
- [`compiler`](../src/compiler.rs) owns decoding, evidence records, validation,
  and inspection.
- [`protected_fs`](../src/protected_fs.rs) owns path binding, private modes,
  locking, staging recovery, durability, and atomic publication.

These modules are internal, not public Rust integration interfaces. Compilation
is source-centric: it verifies and reads each unique source file once, parses
that snapshot once per assigned profile, and releases parsed state before
reading the next source. Units may complete out of assignment order; manifest
entries always retain assignment order.

The workflow's responsibility boundaries are:

1. [`compile_workflow`](../src/compiler/compile_workflow.rs) validates the
   assignment and plans source dependencies.
2. [`source_receipts`](../src/compiler/source_receipts.rs) verifies planned
   sources and reconciles attachment receipts. Planned artifacts are verified
   by their source pass; other selected artifacts are verified on first use.
   Completion rejects any unverified planned source or expected receipt.
3. [`planned units`](../src/compiler/compile_workflow/unit.rs) own source slots,
   file identities, execution-session consistency, extraction completeness, and
   ordered span and attachment assembly. Frontends receive read-only requests.
   Original documents need receipts, not parsing; other slots need extraction.
4. [`package writer`](../src/compiler/package/writer.rs) seals and validates each
   completed unit, stages it immediately, and retains only manifest entries and
   assignment identities. The writer and loader share relationship checks.
5. Source-receipt completion succeeds before the writer publishes. A staged
   email unit cannot bypass verification of a later planned artifact.
