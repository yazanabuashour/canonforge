# Architecture contract

Canonforge compiles frozen heterogeneous sources into a backend-neutral evidence
package. It does not index, search, rank, authorize queries, derive downstream
state, or generate answers.

```text
frozen source snapshots
  -> source-specific frontends
  -> checksummed native units and evidence spans
  -> validated evidence package
  -> independently chosen downstream projections
```

## Product boundary

Compilation begins with an explicit source assignment, an owner-private source
root, and a checksum index. It ends after Canonforge has atomically published a
package whose manifest, native-unit identities, source bindings, spans, and
digests validate.

A feature belongs in Canonforge when it is required to represent a source
faithfully and reproducibly or to validate the compiled package. A feature does
not belong when it decides:

- which evidence matches a query;
- how evidence is chunked, embedded, indexed, filtered, or ranked;
- which principal may retrieve evidence at query time;
- which evidence a downstream system should retain or derive from; or
- what answer should be generated from evidence.

Those decisions belong to downstream consumers. Full-text, vector, hybrid, and
other retrieval systems are consumers, not Canonforge backends.

## Authority and derivation

The checksummed native source remains canonical. A compiled span is an
addressable reading view bound to that source; it does not replace the source.
Source content, embedded prompts, tool output, OCR, and parser output are
untrusted data.

Canonforge frontends may:

- identify native-unit boundaries;
- decode source transport syntax;
- preserve source order, actors, roles, timestamps, and locators;
- exclude platform records that must not be published as evidence; and
- produce deterministic, checksummed evidence spans.

They may not decide relevance, authority, truth, consensus, or importance.
Canonforge never truncates a successfully compiled span. If a source cannot be
represented completely, compilation fails or the frontend must preserve an
explicit derived representation bound to the complete source.

## Evidence package

An evidence package is an immutable directory containing:

- `manifest.json`, which lists every native unit and its digest; and
- one JSON evidence-unit record per manifest entry under `units/`.

Each evidence unit preserves one stable `unit_id`, its source type and native
locator, every contributing source file and checksum, ordered evidence spans,
and a digest over the complete unit. Each span preserves its own locator, role,
timestamp, text, and checksum.

The normative schemas are:

- `source-assignment.schema.json`;
- `evidence-package-manifest.schema.json`; and
- `evidence-unit.schema.json`.

`package-inspection.schema.json` defines `inspect` output, and
`conversation-inventory-manifest.schema.json` defines the source-inventory
receipt. [Canonical digest bytes](../skill/compile-knowledge/references/canonical-digests.md)
defines the language-independent checksum recipe and conformance vector.

## Source frontends

The built-in frontends currently cover:

- selected Markdown heading units;
- active ChatGPT conversation paths;
- Gmail MBOX threads;
- RFC 4180 conversation tables;
- Codex-style execution histories; and
- Docling JSON documents.

Docling is the document extraction boundary for PDFs, images, Office files, and
other layout-bearing formats. Canonforge does not implement OCR or document
layout analysis. It compiles Docling's source-bound representation into the
same evidence package used for conversations and execution histories.

Adding a source type requires a concrete source profile and synthetic success,
tamper, and ambiguity coverage. Unknown source types fail closed.

## Downstream consumers

Consumers read and validate the file contract. Canonforge does not provide a
runtime plugin interface or database abstraction. Each consumer chooses its
own lowering:

- FTS may flatten selected spans into lexical documents;
- vector retrieval may choose chunks, models, and embeddings;
- hybrid retrieval may build several disposable projections;
- specialized consumers may select only compatible records and derive their
  own state; and
- authorization-aware systems may interpret preserved source policy metadata
  under their own trusted policy snapshot.

Consumer-generated row IDs, chunks, descriptors, embeddings, scores, query
results, and answers never become Canonforge evidence.

## Platform and publication

The current implementation requires Linux `openat2`, a mounted `/proc/self/fd`,
and `renameat2(RENAME_NOREPLACE)` on private artifact filesystems. Protected
path components may not be symlinks. Output parents must already exist and be
owner-controlled. Package publication is atomic and refuses replacement.

This filesystem policy protects private compilation inputs and outputs; it is
not a serving lifecycle, catalog, replication protocol, or backend deployment
contract.

## Explicit non-features

Canonforge has no database, index, query command, result format, reranker,
answerer, grader, policy service, document-write lifecycle, hosted service,
network service, or generic backend abstraction.

Wire formats are versioned. Recompile evidence packages after a contract
change.

## Versioning

Canonforge releases follow semantic versioning. Before 1.0, a minor release may
change the CLI or compiler behavior; patch releases remain compatible with that
minor line.

Evidence-package compatibility follows `schema_version`, not the Canonforge
binary version. An incompatible file-contract change increments the schema so a
consumer can reject it explicitly. Compiler fixes may change compiled content
or digests without changing the schema, so pin the Canonforge release when a
compilation must be reproducible and rebuild downstream projections after
deliberately recompiling.

## Implementation boundaries

- `cli` owns the four compiler-facing commands and routing only.
- `compiler` owns source decoding, evidence-package records, validation, and
  inspection.
- `protected_fs` owns Linux path binding, private modes, atomic publication,
  and durability primitives.
