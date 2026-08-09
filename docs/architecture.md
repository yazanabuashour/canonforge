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

Evidence-package schema v2 preserves one stable `unit_id`, its source type and
native locator, every contributing source file and checksum, ordered evidence
spans, ordered email attachment occurrences, and a digest over the complete
unit. Each span preserves its own locator, role, timestamp, text, and checksum.
Each email attachment occurrence binds an exact MIME-part locator and parent
message span to a content-addressed source receipt. Non-email units have an
empty attachment array.

The normative schemas are:

- `source-assignment.schema.json`;
- `evidence-package-manifest.schema.json`; and
- `evidence-unit.schema.json`.

`package-inspection.schema.json` defines `inspect` output, and
`conversation-inventory-manifest.schema.json` defines the source-inventory
receipt. `email-attachment-manifest.schema.json` and
`email-attachment-receipt.schema.json` define materialization file and stdout
contracts. [Canonical digest bytes](../skill/compile-knowledge/references/canonical-digests.md)
defines the language-independent checksum recipe and conformance vector.

## Source frontends

The built-in frontends currently cover:

- selected Markdown heading units;
- active ChatGPT conversation paths;
- Gmail MBOX threads;
- RFC 4180 conversation tables;
- Codex and Pi execution histories; and
- Docling JSON documents.

Docling JSON is an accepted source-bound representation for PDFs, Office files,
and other layout-bearing formats. Canonforge does not invoke Docling and does
not implement OCR, VLM processing, or document layout analysis.

### Email artifacts

The Gmail MBOX frontend and `materialize-email-attachments` command share one
mailbox framing and MIME-part walk. Materialization decodes literal MIME
attachment and inline-part transfer bytes, hashes decoded bytes with SHA-256,
and stores each unique blob once beneath a digest-derived path. It never fetches
remote images or URLs. Supplied filenames remain descriptive fields and are
never interpreted as filesystem paths.

The deterministic inventory keeps every occurrence even when several messages
or threads contain identical bytes. Its aggregate receipt reports parsed
messages, occurrences, unique blobs, decoded and duplicate bytes, media types,
dispositions, and malformed or undecodable parts. Canonforge introduces no
attachment-size or total-size limit without a measured receipt. Any future
tripwire must identify its configured limit, requested bytes, and exact
MIME-part locator.

During compilation, the frontend compares the supplied inventory with the MIME
parts observed in the immutable MBOX snapshot and verifies each selected
artifact receipt. One email unit source list begins with the MBOX and then adds
unique artifacts in first-occurrence order. Several occurrence records may
reference the same artifact receipt. The checksummed MBOX remains authoritative;
artifacts are reproducible decoded transport views.

### Omitted media and reasoning

The checksummed native source retains the authoritative bytes. When a supported
ChatGPT, Codex, or Pi record says that an image exists, the frontend emits an
ordinary span at the exact message-part or content-item locator with role
`omitted-asset` and deterministic text such as
`{"kind":"image","status":"not-materialized"}`. Pi markers also retain the
source-supplied scalar MIME type. Canonforge does not decode image data, resolve
asset pointers, copy their values, store assets, classify bytes, or route them
through Docling.

Pi thinking and recap bodies are likewise absent from the reading view. Their
ordered positions are represented by deterministic `excluded-reasoning`
markers. Unknown record and content types fail compilation instead of being
silently dropped.

These rules do not change the other frontends. Markdown and notes retain literal
links without fetching them. Docling remains available only for an explicitly
supplied, source-bound document extraction; omitted media is never sent to it
automatically.

Adding a source type requires a concrete source profile and synthetic success,
tamper, and ambiguity coverage. Unknown source types fail closed.

Compilation is source-centric. Canonforge first validates the assignment and
builds its source-dependency plan, then verifies and reads each unique source
file once. It parses that immutable snapshot once per assigned source profile
and extracts every dependent unit. Parsed state is bounded to that source and
released before the next source is read. Evidence units and manifest entries
remain in assignment order even when shared-source units finish together.

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

Marker roles describe source structure rather than substantive text. Consumers
decide how to filter `omitted-asset` and `excluded-reasoning` spans before
chunking, embedding, indexing, retrieval, or ranking.

### External document extraction workflow

Document extraction remains outside Canonforge:

1. Materialize email artifacts.
2. Select unique PDF and Office artifacts for extraction.
3. Run Docling externally with OCR and VLM processing disabled.
4. Assign one `docling-json` unit per unique extracted artifact, using the
   content-addressed artifact as `original_file`.

A downstream consumer can associate that document with every email occurrence
by matching the original-file SHA-256 receipt. JPEGs, PNGs, documents without
extracted text, encrypted files, archives, and unsupported binaries remain
linked artifacts. The external workflow keeps them out of Docling assignments
and Qdrant projections; Canonforge itself does not invoke or configure Qdrant.

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

Schema-v2 migration is a side-by-side cutover: compile into a new output path,
validate it, upgrade every consumer to accept v2, and only then switch the
consumer input. Retain the v1 package and its prior Canonforge binary until the
rollback window closes. Rolling back only the binary does not make it able to
read a v2 package, and immutable publication intentionally refuses to replace a
v1 directory in place.

## Implementation boundaries

- `cli` owns compiler-facing commands and routing only.
- `compiler` owns source decoding, evidence-package records, validation, and
  inspection.
- `protected_fs` owns Linux path binding, private modes, atomic publication,
  and durability primitives.
