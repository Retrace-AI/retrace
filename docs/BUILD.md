# Building Retrace from source

Retrace's TUI/agent binary is a fork of the Codex CLI, living under `codex-src/`.

```sh
cd codex-src/codex-rs
cargo build --release -p codex-cli --bin retrace
# -> codex-src/codex-rs/target/release/retrace
```

Install a locally built binary over an existing install:

```sh
cp codex-src/codex-rs/target/release/retrace ~/.retrace/bin/retrace-bin
```

## Release artifacts

`.github/workflows/release.yml` builds `retrace-macos-<arch>.tar.gz` on every `v*`
tag. The tarball contains:

```
retrace-bin              # the compiled binary
runtime/                 # proxy, admin, launcher, admin shim
config-skeleton/         # empty, secret-free ~/.retrace starter
launchd/                 # the proxy keep-alive plist template
```

`install.sh` downloads and unpacks this tarball.
