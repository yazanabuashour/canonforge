# Contributing to Canonforge

Canonforge favors small, explicit changes that preserve its fail-closed source
and evidence-package contracts.

## Set up

Install Git and ShellCheck, and install Rust through a toolchain manager that
honors `rust-toolchain.toml`. Rustup selects the pinned compiler automatically;
the included Mise configuration enables the same project selection.

Use a focused branch and keep unrelated local changes out of the patch.

## Protect private data

The public repository contains code, documentation, schemas, and newly written
synthetic fixtures. Never commit:

- raw exports, captures, attachments, or normalized source content;
- generated evidence packages, Docling output, OCR, checksums, manifests, or
  other derived representations of real sources;
- databases, indexes, embeddings, query results, or run receipts;
- hashes, counts, paths, timestamps, or aggregate reports that fingerprint a
  private corpus; or
- archives, credentials, tokens, local environment files, absolute home paths,
  or archived prompts that exist only to reproduce a private run.

A checksum is still an identifier for private data. Redacting the body while
publishing its hash is not sufficient.

Keep real-source work in owner-private directories outside the repository. Do
not weaken filesystem checks for local convenience. If a regression needs a
fixture, write the smallest case from scratch with fictional names,
identifiers, dates, and prose; do not paraphrase a private source closely enough
to preserve its identity or distinctive facts.

Before committing, inspect the complete diff. The fixture author remains
responsible for verifying that its content is synthetic.

## Required checks

Run the complete local sequence before requesting review:

```sh
scripts/ci.sh
```

The script rejects a different compiler version before running Rust commands.
This catches system package managers that ignore the project pin.

Changes to source parsing, checksums, protected paths, schemas, or package
publication need regression coverage for both the successful path and
ambiguous, tampered, or unsafe input.

## Pull requests

Explain the user-visible outcome, security implications, tests run, and any
wire or compatibility impact. Do not include private logs or screenshots.
Report suspected vulnerabilities through [SECURITY.md](SECURITY.md), not a
public issue.
