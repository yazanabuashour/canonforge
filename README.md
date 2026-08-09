# Canonforge

**Compile private knowledge sources into portable evidence packages.**

Canonforge turns Markdown, conversations, email, execution histories, and
extracted documents into checksummed evidence with exact source references.

```text
sources -> evidence package -> any retrieval system
```

Canonforge stops at the package. Use it with any full-text, vector, or hybrid
retrieval system without coupling that system to the compiler.

## Install

```sh
curl -fsSL https://github.com/yazanabuashour/canonforge/releases/latest/download/install.sh | sh
```

Prebuilt binaries support recent Linux distributions. See
[Installation](docs/install.md) for pinned releases and source builds.

## Use

```sh
canonforge materialize-email-attachments \
  --source-root sources \
  --file gmail/All-mail.mbox \
  --artifact-dir _artifacts/sha256 \
  --output-manifest run/email-attachments.json

canonforge compile \
  --assignments assignments.json \
  --source-root sources \
  --checksums SHA256SUMS \
  --email-attachment-manifest run/email-attachments.json \
  --output evidence-package

canonforge validate --package evidence-package
canonforge inspect --package evidence-package
```

The materialization step decodes only literal MIME parts. It never fetches
remote content, and supplied filenames never become filesystem paths.

Run `canonforge --help` for every command. To try Canonforge from a source
checkout:

```sh
cargo build --locked
scripts/demo.sh
```

## Learn more

- [Architecture](docs/architecture.md)
- [Installation](docs/install.md)
- [Source formats](skill/compile-knowledge/references/source-profiles.md)
- [Package schemas](skill/compile-knowledge/assets)
- [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) · [MIT License](LICENSE)
