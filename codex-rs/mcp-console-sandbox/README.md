# Standalone native sandbox executable

`mcp-console-sandbox` exposes this release's native sandbox through one executable, without application sessions, configuration files, or a Rust API dependency. It runs an opaque command with inherited stdin, stdout, and stderr. Linux uses the native bubblewrap, seccomp, and proxy paths; macOS uses the native process Seatbelt profile. Other platforms report unsupported on stderr and exit 1 without launching the target.

The default supervisor owns launch, descendant retirement, optional private storage, and the upstream managed proxy. Caller-death and SIGTERM retirement are explicit options. Linux also accepts explicit Landlock execution, which has no supervisor or process isolation. The [lifecycle contract](LIFECYCLE.md) defines cleanup ordering and platform limits, including runner loss; the [Linux compatibility guide](LINUX_COMPATIBILITY.md) defines host requirements.

The [complete JSON configuration reference](PROTOCOL.md#complete-json-reference) covers both transports and every nested field. `filesystem: {"kind":"unrestricted"}` retains the independently selected native network policy. `external-sandbox` delegates enforcement to an outer sandbox unless a managed proxy requires native routing; its Linux supervision covers the original process group.

## Invocation

Put JSON in one selected child environment variable, and keep arguments, cwd, and ordinary environment values as normal launch inputs:

```sh
SANDBOX_CONFIG='{"version":2,"filesystem":{"kind":"restricted","entries":[{"path":{"type":"special","value":{"kind":"root"}},"access":"read"}]},"network":"restricted","lifecycle":{"private_tmp":{"environment":["TMPDIR"]}}}' \
  mcp-console-sandbox --config-env SANDBOX_CONFIG -- /bin/cat
```

For larger requests, `--bootstrap-fd N` accepts one framed JSON request through an inherited descriptor above stdio. Both modes require UTF-8 arguments, paths, and environment values. Configuration is consumed once; stdout belongs to the target, and launch errors use stderr and a nonzero exit. [PROTOCOL.md](PROTOCOL.md) defines the request, trust boundaries, proxy environment, caller-supplied macOS rules, and a runnable Python caller.

## Build and validation

Use the release's Rust 1.95.0 toolchain, workspace dependencies, and lockfile. Cargo, Bazel, rustup, and CI actions may download uncached dependencies and tools. Once dependencies are available, Cargo accepts `--locked --offline`.

For an ordinary GNU Linux build, install a C toolchain, `pkg-config`, and libcap development files (`build-essential pkg-config libcap-dev` on Ubuntu), then build the pair:

```console
cd codex-rs
cargo build --locked --release \
  -p codex-mcp-console-sandbox --bin mcp-console-sandbox \
  -p codex-bwrap --bin bwrap
sandbox_target_dir="$(cargo metadata --locked --format-version=1 --no-deps |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
```

Stage `$sandbox_target_dir/release/mcp-console-sandbox` and `$sandbox_target_dir/release/bwrap` together when supplying the bundled helper. The ordinary Cargo helper links to system libcap, so the destination needs its runtime library. On macOS, build only the runner with `cargo build --locked -p codex-mcp-console-sandbox --bin mcp-console-sandbox --release`. The fixture binary is a test target, not a runtime companion.

Linux selects a suitable host `bwrap` from the trusted launch `PATH` first, then the upstream bundled-helper search. An adjacent `bwrap`, the existing `codex-resources` layout, and install-context locations are supported. No patched bubblewrap option or runner-specific adjacent-helper check is required. Packaging should embed the shipped helper's digest through build-time `CODEX_BWRAP_SHA256`; a selected bundled helper is hashed and executed through the same open descriptor. An unused missing or modified bundle does not block a suitable host helper; a selected modified bundle fails verification without fallback.

Portable Linux pairs use `x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl`. Follow the release's [target matrix and recipe](../../.github/workflows/rust-release.yml) and [musl tool setup](../../.github/scripts/install-musl-build-tools.sh), passing `TARGET` and a `GITHUB_ENV` output file. Apply that file's environment only to musl builds. The recipe builds pinned musl libcap, uses Zig 0.14.0 for native dependencies and musl GCC for Rust linking, and disables aws-lc jitter entropy. This package enables vendored OpenSSL without relying on `codex-core` feature unification.

Build and strip `bwrap` first, export its SHA-256 as `CODEX_BWRAP_SHA256`, then build the runner with the same target and `--locked --release`. Inspect both with `readelf -W -l -d -V`: portable pairs must have no interpreter, `NEEDED` libraries, or symbol-version requirements. They need no host libcap or OpenSSL runtime libraries. Their target command still needs its own interpreter, libraries, and compatible libc; static runner artifacts do not establish portability of an application's R, Python, or SQL runtime.

For Linux Cargo tests, build the ordinary debug helper before running the executable suite:

```console
cargo build --locked -p codex-bwrap --bin bwrap
env "CARGO_BIN_EXE_bwrap=$sandbox_target_dir/debug/bwrap" \
  just test -p codex-mcp-console-sandbox --retries 0
```

Bazel uses `bazel build //codex-rs/mcp-console-sandbox:mcp-console-sandbox` and `bazel test //codex-rs/mcp-console-sandbox:bootstrap-contract-test`. Test data supplies the runner, fixture, and bundled helper; no source-revision stamp or workspace status configuration is required.

The [focused workflow](../../.github/workflows/mcp-console-sandbox.yml) runs macOS and GNU Linux executable/native suites and tests both musl architectures' transport and lifecycle contracts. GNU fault-injection tests stay separate because their loader interposers cannot instrument static executables. [REBASE.md](REBASE.md) contains the full upgrade checklist, macOS native-test exclusions, and revision-specific results; workflow definitions alone do not establish that a run passed. [INTEGRATION.md](INTEGRATION.md) inventories the code carried over upstream.

## Telemetry and network

The runner initializes no model client, authentication, application session, or OpenTelemetry exporter. There is no runner telemetry or telemetry switch; transitive telemetry dependencies do not activate an exporter. Build tools do not send application telemetry to OpenAI.

For managed execution with restricted networking and no proxy, native rules restrict target network operations. External execution delegates that enforcement to the caller. A managed proxy listens and connects according to the supplied upstream policy. With networking enabled, the target may make its own requests. Proxy behavior and environment changes are specified in [PROTOCOL.md](PROTOCOL.md#network-and-target-environment).
