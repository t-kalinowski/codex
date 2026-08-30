# MCP Console sandbox runner

`mcp-console-sandbox` is a private executable that exposes the native sandbox
implementation in this Codex release through a versioned machine protocol. It
is built from an immutable revision of this fork and bundled with MCP Console.
It is not a user-facing command, is not published to crates.io, and must not be
installed on the user's normal `PATH`.

The executable and [protocol](PROTOCOL.md) are the downstream contract. MCP
Console does not link Codex crates into its main Cargo dependency graph.

## Current platform support

There is no unsandboxed backend or fallback.

| Platform | Status | Implementation |
| --- | --- | --- |
| macOS | Supported | Codex Seatbelt policy generation and `/usr/bin/sandbox-exec` through `codex-sandboxing::SandboxManager` |
| Linux | Supported | Codex's Linux helper and packaged bubblewrap, namespaces, seccomp, and synthetic mounts through `SandboxManager` |
| Windows | Discovery only | Discovery reports `backend: "unsupported"`; setup and launch fail before a target starts |

Windows launch and setup are intentionally deferred. The protocol retains the
closed Windows extension and setup vocabulary so support can be added in a
later rolling patch without inventing a second interface.

Managed networking on supported platforms uses `codex-network-proxy` together
with the native backend's direct-egress confinement. Static denied and
unrestricted modes do not start a proxy.

## Build

Use the repository toolchain, workspace dependency versions, and lockfile. On
macOS and Windows, build the primary executable directly:

```console
cd codex-rs
cargo build \
  --locked \
  -p codex-mcp-console-sandbox \
  --bin mcp-console-sandbox \
  --release
```

On Linux, build and hash the exact companion first, then compile that digest
into the runner:

```console
cd codex-rs
cargo build --locked -p codex-bwrap --bin bwrap --release
export CODEX_BWRAP_SHA256="$(sha256sum target/release/bwrap | cut -d ' ' -f 1)"
cargo build \
  --locked \
  -p codex-mcp-console-sandbox \
  --bin mcp-console-sandbox \
  --release
```

Stage those unchanged `target/release/bwrap` bytes at the relative path shown
below. Rebuilding or modifying the companion after hashing makes the runner
reject it.

The Bazel executable is
`//codex-rs/mcp-console-sandbox:mcp-console-sandbox`. Use the focused stamping
configuration when building or testing it:

```console
bazel build \
  --config=mcp-console-sandbox \
  //codex-rs/mcp-console-sandbox:mcp-console-sandbox \
  //codex-rs/bwrap:bwrap
```

The build records a full 40-hex Codex source revision. A Git checkout supplies
it from `HEAD`; source-archive and Bazel builds must supply the equivalent
stable revision stamp. An unstamped build fails.

The leaf crate has no direct dependency on the Codex CLI, TUI, core, app
server, model clients, authentication, sessions, history, or MCP servers, and
the executable initializes none of them. The release's sandbox facade has an
existing unconditional platform-support dependency that makes Cargo compile
some shared client and telemetry crates on Unix; extracting that dependency is
outside this narrow rolling patch. `mcp-console-sandbox-fixture` exists only
for executable contract tests and is not a runtime companion.

## Private installation layout

MCP Console stages artifacts in one immutable private libexec directory.

macOS:

```text
libexec/
`-- mcp-console-sandbox
```

Linux:

```text
libexec/
|-- mcp-console-sandbox
`-- codex-resources/
    `-- bwrap
```

The Linux runner uses only the exact executable at
`<runner-directory>/codex-resources/bwrap`. It does not search `PATH` for a
helper. The runner verifies it against the SHA-256 digest compiled into the
runner and repeats that verification from an open descriptor immediately
before execution. The runner self-dispatches the release's
`codex-linux-sandbox` helper from its own executable and stores synthetic-mount
bookkeeping below the caller-provided state directory.

Windows currently needs only the primary executable for capability discovery.
No Windows helper layout is promised until launch support is implemented.

## Embedding flow

The embedding application creates a private bidirectional native channel and
passes its runner endpoint at bootstrap. Each target stream endpoint is passed
separately and declared once. On Unix:

```text
mcp-console-sandbox \
  --state-dir <application-owned-absolute-path> \
  --cleanup-dir <target-facing-absolute-path> \
  --control-fd <fd> \
  [--stream-fd <fd>]... \
  -- <absolute-target> <target-arguments...>
```

Windows discovery uses `--control-handle`. The `--stream-handle` spelling is
reserved, but nonempty stream-handle lists fail while Windows launch is
deferred.

The target program and arguments follow `--`. The runner never invokes a shell
and does not resolve bare executable names. On macOS the private launch bridge
preserves native target-path and argument bytes through an inherited
descriptor. JSON policy paths and environment names and values remain Unicode
in protocol version 1.

The caller then:

1. sends `discover` with protocol version 1 and verifies the reported backend,
   source revision, setup state, and required companions;
2. sends one `launch` with filesystem, network, stream, terminal, and lifecycle
   policy;
3. uses `status`, `interrupt`, `terminate`, and `wait` as supported by the
   selected backend.

One runner owns at most one target generation. It is not a daemon and provides
no multiplexing, reconnection, persistence, remote transport, SSH, Docker, or
provider plugin system.

## Launch ownership

On macOS and Linux the transformed sandbox command starts a private
self-reexecution bridge. The bridge reports `ready` and waits at one native
gate. The outer runner installs process supervision, gives stream ownership to
the target generation, closes its unnecessary copies, and then releases the
gate. On macOS an independent lifetime manager observes exact process
identities before release, including observed descendants that later create
another process group or session. It survives uncatchable termination of the outer
runner. Before reporting ready, the bridge verifies the selected platform
sandbox and the denial of a runner-owned state canary. Direct unsandboxed
bridge invocation fails without executing the target. The bridge then executes
the target directly.

The bridge's status descriptor is close-on-exec. EOF therefore represents a
successful native `exec`; an `exec` error is reported on that private channel.
Application bytes never use the bridge or the public control protocol. Root
exit, complete observed-tree retirement, proxy cleanup, and infrastructure errors remain
separate outcome concepts.

Control-channel loss is fail-safe: a running generation is retired instead of
being knowingly left without its owner. On macOS, normal completion is bounded
by both lifecycle grace periods, two force-timeout windows, and two one-second
manager allowances. Control loss uses two force-timeout-plus-allowance windows;
reaping the identity-pinned root after control EOF has one further second.

## State and configuration isolation

`--state-dir` is an absolute, valid-Unicode directory owned by MCP Console. The
runner creates and canonicalizes it. `--cleanup-dir` names a separate existing,
absolute, valid-Unicode directory owned by the runner user. It is intended for
target-facing private files and may be granted through the launch filesystem
policy. The runner records its filesystem identity and removes it only after
successful target-tree retirement. Identity or retirement failure preserves
the directory and reports the failure separately.

The target filesystem policy always denies the state directory, including its
canonical spelling, and rejects rules that try to override runner or companion
resources. On Linux the helper uses:

```text
<state-dir>/bwrap-synthetic-mount-registry
```

The runner does not initialize or load the user's normal Codex configuration,
`CODEX_HOME`, authentication, approvals, sessions, MCP servers, or history.
It does not create ordinary Codex user state merely to launch a target.

## Environment, streams, and terminals

The runner process environment is normally the complete intended target
environment. Before launch it:

- rejects non-Unicode names or values in protocol version 1;
- rejects Unix `LD_*` and `DYLD_*` variables;
- removes only runner-private `MCP_CONSOLE_SANDBOX_*` variables;
- lets the managed proxy add its documented proxy variables when requested.

The embedding application must itself start the runner under a trusted,
loader-sanitized infrastructure environment, because loader variables can act
before runner code begins. Errors identify a rejected key but do not print its
value or the complete environment.

Stdin, stdout, and stderr independently support `inherited`, `null`, and a
declared passed descriptor or handle. Endpoints are attached directly to the
target command. The runner does not interpret, buffer, multiplex, or relay
target bytes. This preserves binary data, terminal detection, native
buffering, and EOF behavior.

Inherited terminals and caller-created PTYs are preserved. On macOS, a command
launch using the runner's or launcher's foreground controlling terminal makes
the target process group foreground before the launch gate opens and restores
the original process group after root exit. Service launches never transfer
terminal foreground ownership. PTYs may be created inside the
sandbox where the backend permits it. Protocol version 1 does not claim host
terminal-device isolation and rejects `isolate_host_devices`.

## Limitations

- Windows setup and target launch are unsupported in this patch.
- Linux does not project interrupt or graceful termination through the
  bubblewrap session boundary; its graceful deadlines must be zero.
- Linux reports a signal death through bubblewrap's conventional `128 + signal`
  exit code, and PID-namespace teardown may retire descendants with the target
  root before a `root_exited` grace phase is externally observable.
- Managed SOCKS UDP, explicit local-port exceptions, managed CA/TLS
  interception, non-loopback proxy listeners, credential brokerage, secrets,
  header injection, approvals, and interactive network elicitation are not
  supported.
- Managed Unix-socket allow rules and configurable loopback/local binding are
  available only where discovery reports them (currently macOS). Unix-socket
  deny rules are unsupported.
- On macOS native target paths and arguments may be non-Unicode. Protocol
  environment values and JSON paths remain Unicode in version 1.
- Arbitrary Seatbelt/SBPL or other backend policy text is not accepted.

See [PROTOCOL.md](PROTOCOL.md) for the wire contract and [REBASE.md](REBASE.md)
for the release patch and rolling-update procedure.
