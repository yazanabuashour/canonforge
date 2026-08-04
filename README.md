# Canonforge

**Compile private knowledge sources into portable evidence packages.**

Canonforge turns Markdown, conversations, email, execution histories, and
extracted documents into checksummed evidence with exact source references.

```text
sources -> evidence package -> any retrieval system
```

Canonforge stops at the package. Use it with full-text search, embeddings,
pgvector, Mem0, or another retrieval system without coupling that backend to
the compiler.

## Install

```sh
curl -fsSL https://github.com/yazanabuashour/canonforge/releases/latest/download/install.sh | sh
```

Prebuilt binaries support recent Linux distributions.

## Use

```sh
canonforge compile \
  --assignments assignments.json \
  --source-root sources \
  --checksums SHA256SUMS \
  --output evidence-package

canonforge validate --package evidence-package
canonforge inspect --package evidence-package
```

Run `canonforge --help` for every command. To try Canonforge from a source
checkout:

```sh
cargo build --locked
scripts/demo.sh
```

## Learn more

- [Architecture](docs/architecture.md)
- [Source formats](skill/compile-knowledge/references/source-profiles.md)
- [Package schemas](skill/compile-knowledge/assets)
- [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) · [MIT License](LICENSE)
