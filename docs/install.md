# Installation

Canonforge publishes checksummed binaries for x86-64 and ARM64 Linux.

Install the latest release:

```sh
curl -fsSL https://github.com/yazanabuashour/canonforge/releases/latest/download/install.sh | sh
```

Pin an immutable release by using the same version for the installer and
binary:

```sh
version=X.Y.Z
curl -fsSL "https://github.com/yazanabuashour/canonforge/releases/download/v$version/install.sh" |
  sh -s -- --version "$version"
```

The installer verifies the downloaded binary against the release checksums and
installs it to `~/.local/bin` by default.

To build from a reviewed checkout instead:

```sh
cargo install --locked --path .
```
