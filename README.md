# Canonforge

**Compile private knowledge sources into portable evidence packages.**

Canonforge turns Markdown, conversations, email, execution histories, and
extracted documents into checksummed evidence with exact source references.
The checksummed native source remains canonical; the package is a reproducible
reading view.

```text
frozen sources -> evidence package -> independently chosen retrieval system
```

Canonforge compiles, validates, and atomically publishes packages. Downstream
consumers own chunking, indexing, authorization, retrieval, and answers.

## Install

```sh
curl -fsSL https://github.com/yazanabuashour/canonforge/releases/latest/download/install.sh | sh
```

Canonforge is Linux-only. See [Installation](docs/install.md) for release-binary
requirements, pinned releases, and source builds.

## Try a synthetic compilation

From a source checkout with the [development prerequisites](CONTRIBUTING.md#set-up):

```sh
cargo build --locked
scripts/demo.sh
```

The demo creates fictional Markdown in an owner-private temporary directory,
compiles it, validates the package, and prints its JSON inventory. It removes
the temporary files on exit and never reads your sources.

## Compile your sources

Follow [Compile knowledge](skill/compile-knowledge/SKILL.md) to prepare private
paths, source assignments, and checksums, then compile and hand off a package.
Keep real inputs and outputs outside this repository.

Run `canonforge --help` to list commands and `canonforge COMMAND --help` for
command-specific options.

## Reference

- [Architecture and compatibility](docs/architecture.md)
- [Source profiles](skill/compile-knowledge/references/source-profiles.md)
- [Package schemas](skill/compile-knowledge/assets)
- [Canonical digest bytes](skill/compile-knowledge/references/canonical-digests.md)
- [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) · [MIT License](LICENSE)
