# Ostrom CLI

This package is the npm launcher for the `ostrom` command. It installs one
platform-specific optional dependency containing the compiled Rust binary and
does not download or modify executables during installation.

The public npm package name is intentionally pending. Once it is registered,
install the CLI with:

```sh
npm install --global @ostrom/cli
ostrom --version
```
