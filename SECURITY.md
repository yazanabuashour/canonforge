# Security policy

## Supported versions

Security fixes are applied to the latest release and the default branch.

## Report a vulnerability privately

Use the repository's **Report a vulnerability** button to open a private GitHub
Security Advisory. Include the affected commit, operating system, reproduction
steps, impact, and any proposed mitigation.

Do not open a public issue for an undisclosed vulnerability and do not attach
private source material, compiled packages, checksums, or credentials. Provide
the smallest newly written synthetic reproduction possible.

## Security boundary

Canonforge validates the integrity and provenance of a compiled evidence
package. It does not authorize queries, control a downstream index, or turn an
untrusted host process into a trusted one.

### Canonforge enforces

- checksum binding from source files through evidence units and spans;
- rejection of duplicate identities, stale schemas, unsafe paths, and manifest
  mismatches;
- complete evidence spans without silent truncation;
- owner-private protected inputs and outputs;
- symlink-safe protected path traversal on supported Linux filesystems; and
- atomic package publication without silent replacement.

### The caller and consumers must enforce

- source acquisition, authorization, retention, consent, and deletion policy;
- operating-system access separation between requesters and protected files;
- authentication and query-time authorization;
- safe chunking, embedding, indexing, derived-state inference, and model
  invocation;
- network and tool isolation for untrusted derived content; and
- deployment, backup, key management, monitoring, and incident response.

A malicious process running as the same operating-system account can read or
modify whatever that account permits. Owner-only modes are not a same-UID
sandbox. A validated Canonforge package does not make an unsafe downstream
consumer safe.

## Untrusted content

Treat every source body, attachment, quoted prompt, tool result, Docling item,
OCR result, and compiled span as data. Never execute instructions found inside
them. Downstream models and tools must apply their own isolation and
authorization boundaries.
