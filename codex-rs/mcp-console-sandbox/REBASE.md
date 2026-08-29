# Rolling patch record

This file records the release base and the seams used by the private MCP
Console sandbox runner. It is a reimplementation guide, not a request to merge
this branch upstream.

## Release identity

- Upstream release tag: `rust-v0.150.1`
- Upstream base SHA: `90854393966b21e9ebfd21b122334eb09a20c93d`
- Workspace version: `0.150.1`
- Rust toolchain: `1.95.0`
- Public executable: `mcp-console-sandbox`
- Cargo package: `codex-mcp-console-sandbox`
- Protocol version: `1`

The runner's discovery response records the final immutable patch commit, not
only the upstream base. MCP Console must pin and build that final commit.

## Patch boundary

The patch adds one leaf crate at `codex-rs/mcp-console-sandbox`. No existing
Codex crate depends on it, and no existing Codex application caller is routed
through it. The executable protocol is the only downstream interface.

The leaf crate owns:

- length-prefixed JSON framing and version 1 protocol types;
- capability discovery and fail-closed request dispatch;
- normalized filesystem/network policy validation;
- native bootstrap endpoint ownership and direct standard streams;
- one private Unix ready gate and bounded exec-error reporting;
- native-confinement proof and runner-state canary denial before that bridge
  reports ready;
- target status, exact-identity descendant retirement, control-loss cleanup,
  target-directory cleanup, and proxy shutdown;
- executable black-box fixtures and contract tests;
- build stamping and these three documentation files.

It deliberately does not own platform policy construction, namespace setup,
seccomp, Seatbelt text, bubblewrap implementation, or the managed proxy.

The private Unix launch bridge is release-local infrastructure. It verifies
that Seatbelt is already active on macOS, or that the expected namespace,
`no_new_privs`, and requested seccomp boundary is active on Linux. It also
requires the normalized denial of a runner-owned state canary before reporting
ready. Direct bridge invocation therefore fails before target execution.

## Files outside the leaf crate

The intended added or modified files outside the leaf crate are narrow:

| File | Reason |
| --- | --- |
| `.bazelrc` | Add the opt-in `mcp-console-sandbox` workspace-status configuration used to stamp the exact revision. |
| `.github/workflows/mcp-console-sandbox.yml` | Run the executable contract and focused native crate checks on macOS and Linux; Windows currently checks compilation only. |
| `codex-rs/Cargo.toml` | Add the leaf crate to the workspace member list. |
| `codex-rs/Cargo.lock` | Record the leaf crate and synchronize the release tag's stale `0.0.0` workspace package entries to `0.150.1`, which Cargo requires after adding a member. |
| `codex-rs/linux-sandbox/src/bundled_bwrap.rs` | Resolve the exact adjacent `codex-resources/bwrap`, require the compiled digest, and expose the existing verifier for discovery. |
| `codex-rs/linux-sandbox/src/lib.rs` | Export the narrow companion-verification seam used by runner discovery. |
| `codex-rs/linux-sandbox/src/launcher.rs` | Select that exact bundled helper when the private runner requests it, without changing normal callers. |
| `codex-rs/linux-sandbox/src/linux_run_main.rs` | Add hidden embedding arguments for the exact companion and caller-owned synthetic-mount registry root. |
| `codex-rs/sandboxing/BUILD.bazel` | Make the opt-in MCP Console Seatbelt fragment available to Bazel builds. |
| `codex-rs/sandboxing/src/manager.rs` | Export one opt-in MCP Console profile constructor without changing normal callers. |
| `codex-rs/sandboxing/src/seatbelt.rs` | Append the release-local compatibility fragment only for the MCP Console profile. |
| `codex-rs/sandboxing/src/seatbelt_mcp_console_policy.sbpl` | Preserve PR #150's required macOS runtime and terminal-device policy. |

Generated Bazel metadata may also change when the repository's supported
generators require it. Before committing, regenerate this table from the final
diff against the base SHA and explain every additional existing file. Changes
to Codex CLI, TUI, core, model, authentication, approval, session, app-server,
or normal sandbox callers are outside this patch.

## Codex APIs reused

### Common policy facade

The runner builds the release's `FileSystemSandboxPolicy` and converts it with
`PermissionProfile::from_runtime_permissions`. It supplies the target,
environment, working directory, policy base, network policy, and managed proxy
context to `codex_sandboxing::SandboxManager::transform`.

That facade selects and translates:

- `SandboxType::MacosSeatbelt` on macOS;
- `SandboxType::LinuxSeccomp` and the `codex-linux-sandbox` self-reexecution
  path on Linux.

Do not copy the translation into the runner. The release-local
`SandboxManager::for_mcp_console()` profile appends only MCP Console's required
macOS runtime and terminal-device rules; normal Codex callers retain the
release policy unchanged. If these APIs move in a later release, adapt the leaf
call site or make the smallest shared low-level export.

### Managed proxy

The runner uses the existing `codex-network-proxy` seams:

- `NetworkProxyConfig` and current domain/Unix-socket permission types;
- `RemoteNetworkProxyConfig` and `NetworkProxyState` validation;
- `NetworkProxy::builder`, `run`, and
  `prepare_for_optional_environment`;
- the returned managed sandbox context and shutdown handle.

The runner owns proxy lifetime but does not duplicate HTTP, SOCKS, redirect,
upstream, domain, environment, or shutdown logic. Managed mode always pairs
the proxy with restricted native networking.

## Platform assumptions

### macOS

- `/usr/bin/sandbox-exec` exists and is the release's Seatbelt entry point.
- `SandboxManager` remains the policy translator.
- The launch bridge resolves `sandbox_init` and `sandbox_free_error` from the
  process image at runtime to verify pre-existing Seatbelt confinement without
  linking the private library into the Bazel target.
- The runner owns a Unix process group for direct interruption and termination.
- An independent lifetime-manager process observes exact descendant identities
  before gate release, tracks the complete observed fork tree across process
  groups and sessions, and performs bounded retirement even if the outer runner
  is killed. A child that exits and is reparented before a NOTE_FORK-triggered
  libproc snapshot is outside the observed tree.
- The outer runner recovers a failed lifetime manager while the exact root
  identity remains live, kills an unresponsive manager after the first bounded
  window, and preserves diagnostic state if safe recovery cannot be established.
- Normal completion is bounded by both lifecycle grace periods, two force
  windows, and two one-second manager allowances. Control loss uses two
  force-timeout-plus-allowance windows; root reaping after control EOF has one
  further second.
- The runner claims a distinct application-owned cleanup directory and removes
  it only after complete retirement; identity or removal failure is reported
  and preserves the directory.
- Inherited terminals, caller PTYs, and in-sandbox PTY creation are supported.
  Command targets become the foreground process group before gate release and
  the original launcher group is restored after root exit; service launches do
  not transfer foreground ownership. Host terminal-device isolation is not
  claimed.
- Managed proxy supports the capability-reported loopback/local-binding and
  typed Unix-socket allow surface.

### Linux

Private runtime layout is exactly:

```text
mcp-console-sandbox
codex-resources/bwrap
```

The runner does not search `PATH`. It self-dispatches as
`codex-linux-sandbox`, requires the exact adjacent helper, and keeps synthetic
mount registry data below:

```text
<state-dir>/bwrap-synthetic-mount-registry
```

The release's Linux helper remains responsible for bubblewrap, user/PID/IPC
and network namespaces, seccomp, synthetic mounts, proxy routing, and target
status projection. The outer runner adds one process group and a parent-death
signal for bounded ownership. Linux does not claim interrupt or graceful
termination across the isolated session.

The Linux Cargo build compiles the SHA-256 of the finalized `codex-bwrap`
artifact into the runner's existing Linux sandbox dependency. Discovery checks
that exact adjacent artifact, and the native launcher repeats verification on
an open descriptor immediately before execution. Bazel supplies the same
digest through the release's existing `bwrap-sha256-env` target.

### Windows

Windows is deferred. The binary accepts a private control handle and answers
`discover`, but reports `backend: unsupported`, no launch capabilities, no
required companions, and setup `unsupported`. `setup_status` reports that
state; `setup` and `launch` fail before target creation or system mutation.

No sandbox identities, ACL state, firewall/WFP state, elevated helper IPC,
desktop state, or normal `CODEX_HOME` are prepared by this patch. A later
rolling release may export the then-current Codex Windows implementation
behind the existing normalized protocol and explicit setup operations. It must
add native black-box tests before advertising support.

## Public capability gaps

- Windows setup and launch are unsupported.
- The release's `codex-sandboxing` dependency graph still compiles shared
  Windows/client/telemetry support on Unix. The runner does not initialize it;
  feature-gating that existing graph would broaden this rolling patch.
- macOS preserves native target program and argument bytes through the private
  launch bridge. Environment names and values, application state paths, and
  JSON policy paths remain Unicode in version 1.
- Host terminal-device isolation is unsupported.
- Linux interrupt and graceful termination are unsupported.
- Linux does not expose configurable loopback denial or Unix-socket policy;
  managed networking requires the supported local-binding/loopback pair.
- Unix-socket deny, SOCKS UDP, explicit local ports, managed CA/TLS
  interception, non-loopback proxy listeners, credentials, secrets, header
  injection, approvals, and interactive elicitation are unsupported.
- Platform extensions are closed and currently empty on macOS and Linux. Raw
  SBPL and arbitrary backend policy are unsupported.
- The release's managed-proxy shutdown currently returns no cleanup error. The
  protocol keeps cleanup failure separate from target and retirement outcomes;
  executable tests exercise target-directory cleanup failure independently.

These gaps must fail before target launch. Do not remove a gap merely because a
low-level API exists; first expose it through capabilities, validation, native
enforcement, and executable contract tests.

## Validation commands

Run from the repository root unless a command begins with `cd codex-rs`:

```console
python3 .github/scripts/verify_cargo_workspace_manifests.py
just bazel-lock-update
just bazel-lock-check
bazel build \
  --config=mcp-console-sandbox \
  //codex-rs/mcp-console-sandbox:mcp-console-sandbox \
  //codex-rs/bwrap:bwrap
bazel test \
  --config=mcp-console-sandbox \
  //codex-rs/mcp-console-sandbox:mcp-console-sandbox-lib-contract-test
actionlint .github/workflows/mcp-console-sandbox.yml
git diff --check
```

Run from `codex-rs`:

```console
just test -p codex-mcp-console-sandbox
just test -p codex-linux-sandbox --no-tests=pass
just test -p codex-bwrap --no-tests=pass
just test -p codex-sandboxing
just argument-comment-lint -p codex-mcp-console-sandbox
cargo clippy -p codex-mcp-console-sandbox --all-targets --all-features -- -D warnings
cargo build \
  --locked \
  -p codex-mcp-console-sandbox \
  --bin mcp-console-sandbox \
  --release
just fix -p codex-mcp-console-sandbox
just fmt
```

On Linux, prepare the debug helper before the runner test, argument lint,
Clippy, or `fix` commands:

```console
cargo build --locked -p codex-bwrap --bin bwrap
export CARGO_BIN_EXE_bwrap="$PWD/target/debug/bwrap"
export CODEX_BWRAP_SHA256="$(sha256sum "$CARGO_BIN_EXE_bwrap" | cut -d ' ' -f 1)"
```

Before the release runner build, replace both values with the finalized
release helper:

```console
cargo build --locked -p codex-bwrap --bin bwrap --release
export CARGO_BIN_EXE_bwrap="$PWD/target/release/bwrap"
export CODEX_BWRAP_SHA256="$(sha256sum "$CARGO_BIN_EXE_bwrap" | cut -d ' ' -f 1)"
```

The workflow performs the same ordering without relying on shell state.

Follow the repository instruction that tests run before the final `fix` and
`fmt`, with no test rerun afterward. Run the focused native executable suite on
macOS and Linux. Windows is a compile-only CI target in this patch; native
Windows protocol and launch tests remain deferred with the backend.

The native suites must cover capability discovery, exact source revision,
framing failures, state transitions, native command/environment preservation,
direct independent streams, filesystem rules and state protection, network
modes and proxy cleanup, terminals, signals, target outcome, descendant
retirement, and control loss. Capability-gated unsupported requests must be
tested for fail-closed rejection.

Before publication also record:

```console
git rev-parse HEAD
git diff --name-status 90854393966b21e9ebfd21b122334eb09a20c93d..HEAD
git diff --stat 90854393966b21e9ebfd21b122334eb09a20c93d..HEAD
git status --short
```

The final branch must contain one logical commit directly above the release
base and no uncommitted files.

## Rolling-release procedure

For each new stable Codex release:

1. Start a fresh branch from the next exact stable Codex release and record its
   tag, SHA, workspace version, and Rust toolchain.
2. Inspect the previous runner's protocol, executable tests, README, and this
   REBASE record.
3. Inspect the new release's current macOS, Linux, Windows, and managed-network
   implementations; do not assume the old internal seams still exist.
4. Reimplement the same external contract against the new release's internals
   with the smallest leaf crate and narrow shared exports.
5. Do not merge or cherry-pick the old rolling-patch branch.
6. Preserve protocol version 1 unless the downstream contract intentionally
   changes; document and test any version change.
7. Run native executable tests on every advertised supported platform. A
   deferred platform may run discovery and fail-closed unsupported tests only.
8. Create one logical commit directly above the new release base, stamp the
   exact immutable revision, and let MCP Console pin that commit.

Recheck the complete existing-file list and helper layout on every release.
Delete release-specific adapters that are no longer needed; do not accumulate
compatibility layers for old Codex internals.
