# MCP Console sandbox runner

`mcp-console-sandbox` is a private executable that exposes this Codex release's native sandboxes to MCP Console through an explicitly versioned machine protocol. It is not a user-facing command, is not published to crates.io, and must not be installed on a user's normal `PATH`.

The executable and [protocol](PROTOCOL.md) are the downstream contract. MCP Console does not link this crate or any other Codex crate into its main Cargo dependency graph.

## Backends

The runner has no unsandboxed backend and no unsandboxed fallback.

| Platform | Native implementation                                                                                      | Process ownership                                                                                                         |
| -------- | ---------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| macOS    | Codex Seatbelt policy construction through `/usr/bin/sandbox-exec`                                         | One Unix process group, with bounded descendant retirement; descendants that deliberately leave the group are not claimed |
| Linux    | Codex bubblewrap helper, user/PID/IPC namespaces, seccomp, synthetic mounts, and proxy routing             | Bubblewrap PID namespace plus process-group supervision and a parent-death watchdog                                       |
| Windows  | Codex standalone restricted identity, ACL/WFP setup, elevated command helper, and non-breakaway Job Object | Job-owned target tree with kill-on-close retirement                                                                       |

On macOS and Linux, the target is launched through a private in-sandbox self-reexecution bridge and target-exec shim. The shim remains blocked after it reports ready until the outer runner has installed supervision, transferred infrastructure ownership, and committed the launch. A close-on-exec status channel confirms native target-exec success before the runner sends `launch_accepted`; the bridge then projects the target's exit code or Unix terminating signal to the outer supervisor. macOS reports the host-visible target PID. Linux returns no root PID because the reported value is local to bubblewrap's PID namespace. Private bridge status is separate from both the public control channel and the target's application streams.

Windows support uses the release-local standalone sandbox facade. Setup is explicit and policy-dependent: the online identity is used for unrestricted networking, while denied and managed-proxy networking use the WFP-confined offline identity. Those identities, their group, firewall rules, WFP filters, and setup mutexes use a fixed MCP Console namespace distinct from ordinary Codex setup. The runner creates the command helper suspended and replaces its process and initial-thread owners and protected DACLs with the runner's actual `TokenUser` and `SYSTEM` before allowing helper code to run. The exact runner SID crosses only the closed helper wire. Before the target resumes, the helper creates its control thread behind a gate and installs a protected runner- and `SYSTEM`-only DACL with an explicit `OWNER RIGHTS` denial. The sandbox identity receives no helper process or thread access. The command helper owns a non-breakaway, kill-on-close Job Object and reports root exit separately from full-job retirement. Windows does not resume the suspended target until the runner has received `Ready`, installed supervision, released unnecessary stream copies, and sent `CommitLaunch`. `launch_accepted` follows the helper's `Committed` acknowledgement. Windows does not claim Unix-style interrupt, graceful signal projection, ConPTY creation, or terminal-device isolation. Its launch `terminate_grace_ms` and explicit terminate `graceful_ms` must therefore both be zero.

Linux keeps bubblewrap's isolated session boundary. It does not project SIGINT or SIGTERM into the target namespace, so its launch `terminate_grace_ms` and explicit terminate `graceful_ms` must also be zero. Explicit termination is forced. Natural root exit still honors `root_exit_grace_ms` before force-retiring any remaining namespace descendants.

## Build

Use the repository toolchain and lockfile:

```console
cd codex-rs
cargo build \
  --locked \
  -p codex-mcp-console-sandbox \
  --bin mcp-console-sandbox \
  --release
```

The build must stamp a full 40-hex Codex source revision. From a Git checkout, the build script reads `HEAD`; a source-archive build must supply the same immutable revision through `STABLE_GIT_COMMIT`. The build fails if neither is available or if the value is not a full SHA.

The Bazel executable target is `//codex-rs/mcp-console-sandbox:mcp-console-sandbox`. Every Bazel command that builds or tests this runner must use `--config=mcp-console-sandbox`; the opt-in configuration stamps the current full Git revision and workspace release version used by the runner and companion compatibility tokens. An unstamped runner fails to compile. Build the companion target for the selected platform from the same revision:

```console
# Linux
bazel build \
  --config=mcp-console-sandbox \
  //codex-rs/mcp-console-sandbox:mcp-console-sandbox \
  //codex-rs/bwrap:bwrap

# Windows
bazel build \
  --config=mcp-console-sandbox \
  //codex-rs/mcp-console-sandbox:mcp-console-sandbox \
  //codex-rs/windows-sandbox-rs/no-telemetry:codex-windows-sandbox-setup \
  //codex-rs/windows-sandbox-rs/no-telemetry:codex-command-runner
```

The leaf `BUILD.bazel` exposes these binaries as contract-test runfiles. It does not create an installer or a libexec package; MCP Console remains responsible for copying the selected artifacts into the layout below.

Build the platform companion from the same checkout when packaging a Cargo artifact:

```console
# Linux
cargo build --locked -p codex-bwrap --bin bwrap --release

# Windows
cargo build \
  --locked \
  -p codex-windows-sandbox \
  --no-default-features \
  --bin codex-windows-sandbox-setup \
  --bin codex-command-runner \
  --release
```

Building this package does not intentionally build or ship the Codex CLI, TUI, app server, model clients, authentication, sessions, history, or MCP servers. Cargo disables the Windows dependency's default telemetry feature. Bazel uses private feature-free runner, sandboxing, Linux, Windows, and helper build edges while leaving normal Codex targets telemetry-enabled. The focused graph has no `codex-otel`, `codex-api`, or `codex-client` edge. Its low-level `codex-protocol` dependency does transitively compile generic `codex-http-client` and upstream OpenTelemetry support, but this executable does not initialize or use them.

`mcp-console-sandbox-fixture` is an executable-level contract-test fixture. It is not a companion resource and must not be included in a runtime package.

## Private installation layout

MCP Console stages the runner and same-revision companions in one immutable private libexec directory:

```text
libexec/
|-- mcp-console-sandbox
`-- codex-resources/
    |-- bwrap                              # Linux only
    |-- codex-windows-sandbox-setup.exe   # Windows only
    `-- codex-command-runner.exe           # Windows only
```

Linux constructs only `<runner-dir>/codex-resources/bwrap`, canonicalizes and validates that executable, and never consults `PATH` in embedding mode. The focused runner self-reexecutes with the reserved `codex-linux-sandbox` invocation name and pins the canonical bubblewrap and state paths in private arguments. Before reporting availability or launching a target, it runs a bounded private compatibility query and requires the exact private companion protocol-2 and Codex-release token with no extra output. This companion ABI is independent of the runner's public protocol version 1. A Bazel build additionally embeds and verifies the companion digest. Cargo packaging relies on the exact layout, closed compatibility token, and immutable private-directory trust boundary.

Windows constructs the two exact paths shown above. The files must be sibling resources under `codex-resources`, use the exact names, and be absolute local-disk paths. Neither setup nor launch searches `PATH`. Before reporting operational capabilities, running setup, or launching a target, the runner executes each exact helper with its own private compatibility query. Each query must exit successfully within two seconds, write no stderr, and produce the exact helper-specific private ABI-1 and Codex-release token on stdout within 1,024 bytes. The helpers answer these queries before setup or command-runner initialization, so compatibility inspection does not mutate setup state or request elevation. This private companion ABI is independent of public runner protocol version 1.

## Embedding flow

The caller creates a private bidirectional native channel and makes the runner side inheritable into the runner child. It must likewise make every endpoint declared by `--stream-fd` or `--stream-handle` inheritable; peer and unrelated endpoints remain private to the caller. It then invokes:

```text
mcp-console-sandbox \
  --state-dir <application-owned-absolute-path> \
  --control-fd <fd> \
  [--stream-fd <fd>]... \
  -- <absolute-target> <native-target-arguments...>
```

Windows uses `--control-handle <handle>` and repeatable `--stream-handle
<handle>` arguments. Every endpoint later named by a protocol
`passed_handle` must be declared exactly once at bootstrap, and every declared
endpoint must be used by the launch. The runner claims these endpoints before
creating its async runtime or other infrastructure, makes them
non-inheritable, and rejects control, standard-stream, duplicate, invalid,
undeclared, or unused values. Native object-identity checks also reject a
control endpoint duplicated under another descriptor or handle value. The
target program and arguments follow `--`; they retain their native
operating-system representation and never pass through JSON or a shell. Bare
executable names and `PATH` resolution are not supported. Windows state,
target, working, and policy paths must be absolute local-disk paths.

The caller then:

1. sends protocol version 1 `discover` and checks the reported capabilities,
   setup state, source revision, and companion layout;
2. performs Windows setup when required;
3. sends one `launch` with normalized filesystem, network, stream, terminal,
   and lifecycle policy;
4. observes `status`, requests `interrupt` or `terminate` when supported, and
   calls `wait` for the final target, retirement, and infrastructure outcome.

On Windows, platform setup inspection is read-only, `prepare` is the only
operation that may request administrative elevation, and `refresh` is
non-elevating. Policy-specific status verifies the installed fixed firewall
rules through the exact setup companion while holding a short machine-global
lease; a stale per-state marker alone never reports ready. A managed-network
`setup_status` may allocate retained proxy listeners so that inspection and
launch use the same ports; the caller should perform setup and launch in the
same runner process. The fixed Windows sandbox identities and firewall rules
use a dedicated MCP Console namespace and permit one active standalone policy
generation machine-wide. Standalone setup mutations take the non-inheritable
global lease briefly. Launch acquires its lifetime lease, verifies the exact
outbound, loopback, and stale-rule state, refreshes ACLs, then holds the lease
through tree retirement and proxy cleanup. A concurrent standalone runner
fails before mutation or target start. Ordinary Codex retains its distinct
identifiers and behavior. The lease is released before the runner publishes
the final outcome. Verification failures include a bounded, redacted
diagnostic identifying the mismatched firewall rule or property without
exposing the setup payload or environment.

One runner owns at most one target generation. A failure reported with
`target_started=true` consumes that generation even when no
`launch_accepted` response was sent; `status` and `wait` still expose its
terminal target, retirement, infrastructure, and cleanup results. There is no
daemon, reconnection, multiplexing, target replacement, or persistent session
state. Before the backend may create a target, failures remain retryable with
`target_started=false`. Once the Unix preparation gate is released or the
Windows `Spawn` request may have been sent, subsequent uncertainty consumes the
generation with `target_started=true` even if the commit gate prevented target
application code from running.

On Unix, a target exit or signal is reported only after the private launch
bridge sends a valid target-completion frame. If forced retirement terminates
the bridge before that frame arrives, `target` is `null` and the missing
observation is an infrastructure error; the bridge's signal is not attributed
to the target.

## State and configuration isolation

`--state-dir` belongs to the embedding application. The runner creates and
canonicalizes it after rejecting a non-Unicode path. The canonical runner
executable, companion-resource paths, and state path must all remain valid
Unicode. Filesystem rules inside the canonical state path, including rules that
resolve into it through another spelling, are rejected, and the native policy
receives an explicit denial. A broad writable ancestor therefore does not make
runner state target-writable. Non-read rules for the runner executable or
anything below its `codex-resources` directory are also rejected, and the
native policy adds specific readable infrastructure entries. Launch also adds
the specific read needed for the absolute target executable; a caller deny
covering that target fails before launch instead of being overridden.

Linux stores synthetic-mount bookkeeping at
`<state-dir>/bwrap-synthetic-mount-registry`. Windows stores setup markers,
protected identity credentials, helper scratch files, and diagnostics below
the caller's directory. The sandbox identities and WFP rules are system state;
filesystem ACLs apply to the policy's host paths. Normal startup does not
initialize the Codex CLI, load Codex configuration, authenticate, start a
session, read conversation history, or discover the user's normal `CODEX_HOME`.

## Environment and streams

The runner launch environment is the intended complete target environment. It
uses `env_clear()` and reconstructs the target environment after removing:

- every inherited `CARGO_BIN_EXE_*`, `CODEX_*`, and
  `MCP_CONSOLE_SANDBOX_*` variable;
- stale proxy variables when managed networking supplies replacements.

The selected backend and managed proxy then inject only the documented values
needed by the target. Managed mode sets its active and local-binding markers,
HTTP/HTTPS and WebSocket proxy variables, `NO_PROXY` variants, Electron and
Node proxy switches, and `ALL_PROXY`/FTP variables. SOCKS mode uses a SOCKS5h
URL for the latter group. The exact key set and values are part of
[the protocol](PROTOCOL.md#environment). Windows consumes its selected proxy
port marker during setup and removes it before target launch. Because Windows
environment names are case-insensitive, managed aliases are emitted once under
an uppercase canonical name.

On macOS with managed SOCKS enabled, an existing `GIT_SSH_COMMAND` is
preserved in its native representation, including a non-Unicode value. When it
is absent, the runner injects Codex's managed SOCKS fallback.

On Unix, any environment name whose native bytes begin `LD_` or `DYLD_` is
rejected before target launch; the error includes the key but never its value.
This check runs after the operating-system loader has already started the
runner, so the embedding application must also launch the runner itself under
a trusted, loader-sanitized infrastructure environment. On Windows, private
keys are matched case-insensitively. Target values retain native UTF-16 code
units, including unpaired surrogates; names must be valid Unicode and unique
under case-insensitive matching. The same trusted-launcher boundary applies to
loader-affecting variables. Errors never print the complete environment.

stdin, stdout, and stderr independently support inherited, null, or
caller-passed native endpoints. Endpoints are attached directly to the target. The
runner does not read, buffer, decode, combine, or relay application bytes, and
application bytes never enter the control protocol. Runner-owned copies close
after native target creation and before launch acceptance so they do not
prolong EOF. Resident Unix bridge, Linux cleanup, bubblewrap monitor, and
Windows helper processes likewise release their unnecessary target-stream
copies after committed launch. On Unix, an inherited descriptor must be open
and cannot be `/dev/null`; use null mode for that behavior. On Windows, each
usable inherited standard handle is snapshotted by kernel-object identity
before the async runtime starts. A handle that was unavailable at bootstrap,
closed, or replaced before launch is rejected before setup or target creation.

Inherited terminals and caller-supplied PTYs retain their native descriptor
and TTY-detection behavior on Unix. macOS `preserve` also retains the
controlling-terminal session, including `/dev/tty` reopening. Its typed
isolation mode instead denies path-based reopening of pre-existing host
terminal devices while keeping inherited descriptors and PTYs created inside
the sandbox usable. Linux keeps Codex bubblewrap's `--new-session`: inherited
TTY descriptors work, but controlling-terminal reopening and host-device
isolation are not reported. Windows accepts ordinary inherited or passed
handles but does not provide ConPTY-specific or terminal-isolation semantics.

## Current limitations

- Protocol version 1 accepts only Unicode JSON paths and requires valid-Unicode
  runner, companion-resource, application-state, and absolute target-program
  paths. The program still arrives through native argv and fails explicitly
  rather than being converted lossily. Target arguments remain byte-preserving
  on Unix and native UTF-16 on Windows.
- Managed proxying supports HTTP, optional SOCKS, full or limited access,
  exact-host domain rules, `*.example.com` subdomain rules, `**.example.com`
  apex-and-subdomain rules, trusted upstream proxies, and the fixed or
  configurable loopback/local-binding combinations documented for each
  backend. Limited access permits only HTTP GET, HEAD, and OPTIONS; version 1
  has no interception path for limited HTTPS. Other glob forms fail before
  proxy startup, and denial wins when the same pattern is
  both allowed and denied. SOCKS UDP, explicit local-port exceptions, managed
  CA/TLS interception, non-loopback listeners, credentials, secret or header
  injection, approvals, and interactive elicitation are unsupported.
- Typed Unix-socket allow rules are supported only by macOS. Protocol version
  1 rejects Unix-socket deny rules and duplicate socket paths before proxy or
  target startup. Discovery reports allow and deny support independently;
  `unix_socket_policy` is only a compatibility summary for any typed rule
  support. Linux accepts only `loopback=allow` with `local_binding=true` and
  reports configurable loopback and local-binding policy as unsupported.
- macOS cannot claim descendants that deliberately escape its process group,
  so it reports both process-tree supervision and full-tree retirement as
  unsupported.
- Linux does not project interrupt or graceful termination through
  bubblewrap's isolated session; both graceful deadlines must be zero.
- `command` and `service` currently use the same supervision behavior.
- Ordinary Cargo Linux builds verify the closed companion ABI but do not carry
  Bazel's additional same-binary digest stamp.
- Windows companions verify closed helper-specific ABI and Codex-release tokens
  from the trusted immutable libexec layout, but have no separate same-binary
  digest stamp.

See [PROTOCOL.md](PROTOCOL.md) for the complete wire contract and
[REBASE.md](REBASE.md) for release provenance and rolling-release maintenance.
