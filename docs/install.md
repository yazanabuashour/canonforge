# Installation

Prebuilt binaries support x86-64 and ARM64 Linux with glibc. The
[installer](../install.sh) checks the architecture and minimum glibc version;
use a source build if your system cannot run a release binary. Compilation also
requires the [Linux filesystem facilities](architecture.md#platform-and-publication).

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
installs it to `~/.local/bin` by default. Add that directory to `PATH` if needed,
then check the installed version:

```sh
canonforge --version
```

## Upgrade or roll back

Rerun the latest-release command to upgrade. The installer verifies the new
binary before replacing the installed one. To roll back, rerun the pinned
command with the earlier version.

Binary rollback does not migrate evidence packages. Follow the
[package migration procedure](architecture.md#compatibility-and-migration)
when changing the schema; an older binary may reject a newer package.

## Build from source

From a reviewed checkout, select the Rust toolchain in `rust-toolchain.toml`
(see [development setup](../CONTRIBUTING.md#set-up)), then run:

```sh
cargo install --locked --path .
```
