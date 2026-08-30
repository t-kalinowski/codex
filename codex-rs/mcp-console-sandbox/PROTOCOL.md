# MCP Console sandbox protocol version 1

This document defines the private machine interface of
`mcp-console-sandbox`. The executable's native bootstrap, JSON control
messages, companion layout, and fail-closed behavior are the downstream API.
Rust APIs and Codex crate internals are not part of the contract.

## Native bootstrap

Unix:

```text
mcp-console-sandbox \
  --state-dir <absolute-state-directory> \
  --cleanup-dir <absolute-target-cleanup-directory> \
  --control-fd <fd> \
  [--stream-fd <fd>]... \
  -- <absolute-program> <argument>...
```

Windows discovery uses `--control-handle`. The `--stream-handle` spelling is
reserved, but nonempty stream-handle lists are rejected while Windows setup
and launch remain unsupported.

The control endpoint is a private bidirectional native stream, separate from
target stdin, stdout, and stderr. Declared stream endpoints must be valid,
distinct from the control endpoint, and used exactly once by the launch
request. Undeclared, duplicate, invalid, or unused endpoints are rejected.
Control and infrastructure endpoints are made non-inheritable before the
target can execute.

The target follows `--` and never passes through JSON or a shell. Its program
must be an absolute executable path. Bare names and `PATH` lookup are not
supported. On macOS the private launch bridge carries the target program and
arguments as native bytes through an inherited descriptor, preserving
non-Unicode values. JSON policy paths and environment names and values remain
Unicode in protocol version 1.

`--state-dir` must be an absolute Unicode path. The runner creates and
canonicalizes it. It belongs to the embedding application, not Codex.

`--cleanup-dir` must identify a separate existing absolute Unicode directory
owned by the runner user. The runner canonicalizes it, records its filesystem
identity, and takes cleanup ownership. It removes the directory only after the
complete observed target tree has retired. If retirement or identity-checked
removal fails, the directory is preserved and the failure remains observable.

## Framing

Each direction uses the same framing:

```text
4-byte unsigned big-endian payload length
UTF-8 JSON payload of exactly that length
```

The maximum JSON payload is 1,048,576 bytes. Zero-length, oversized,
truncated, malformed UTF-8/JSON, or schema-invalid frames fail. For a malformed
frame the runner sends a structured error when possible and closes the control
channel because frame synchronization is no longer trusted. Target application
bytes never appear on this channel.

Each client request contains:

- `type`: operation name;
- `id`: caller-chosen unsigned 64-bit correlation identifier;
- `protocol_version`: exactly `1`.

Responses repeat `id`; framing failures that prevent decoding it use
`id: null`. A version mismatch returns `version_mismatch` and does not start a
target.

## Versioning and compatibility

Protocol version 1 is exact. All request objects and nested authority-bearing
objects reject unknown fields. Unknown enum variants are invalid. A client may
ignore unknown descriptive fields in a response, but must not infer support
from their presence: capability booleans and the selected backend are
authoritative.

Adding optional descriptive response fields is compatible. Adding authority,
changing policy meaning, accepting a previously rejected policy, changing
native bootstrap, or changing required companion layout requires an explicit
contract review and may require a new protocol version.

## One-runner/one-target state machine

The normal state sequence is:

```text
idle --launch accepted--> running --root observed--> root_exited
  --descendants retired and cleanup observed--> retired
```

`retiring` and `failed` are reserved phase values in the version 1 schema.
Errors before target creation leave the runner idle and may be corrected. Once
a generation may have started, `error.target_started` is `true`, that
generation is consumed, and a second launch is invalid. A consumed launch that
cannot retain a supervisor reports the `failed` phase, and subsequent lifecycle
errors continue to report `target_started: true`. A target `exec` error
after the launch gate is released is reported as an infrastructure outcome,
not as target exit code 125.

Legal operations are:

- `discover`: capability and build discovery;
- `setup_status`, `setup`: reserved setup operations, before launch only;
- `launch`: once, from idle;
- `status`: snapshot of the current phase and known outcomes;
- `interrupt`: after launch and only when capabilities advertise it;
- `terminate`: after launch;
- `wait`: after launch, to observe final retirement and cleanup.

`discover` and pre-launch validation never grant authority or start a target.
Invalid transitions return `invalid_state`.

## Discovery

Request:

```json
{"type":"discover","id":1,"protocol_version":1}
```

Response type `capabilities` reports:

- protocol version and maximum frame size;
- runner build version, exact 40-hex Codex source revision, and release tag;
- operating system and architecture;
- selected native backend;
- filesystem bases, rule kinds, missing-path behavior, precedence, Unicode
  boundary, and state-directory protection;
- denied, unrestricted, and managed network features;
- inherited, passed, null, independent, and byte-transparent stream support;
- inherited terminal, caller PTY, in-sandbox PTY, and device-isolation support;
- interrupt, termination, root observation, tree supervision, cleanup, and
  control-loss behavior;
- exact required companion resources and relative paths;
- setup state and an optional diagnostic.

Backend values relevant to this patch are `macos_seatbelt`,
`linux_bubblewrap`, and `unsupported`. macOS requires
`/usr/bin/sandbox-exec`. Linux requires the exact executable at
`codex-resources/bwrap` relative to the runner. Missing or non-executable
resources make capabilities false and setup `unavailable`; production launch
does not search another directory or `PATH`.

Windows discovery succeeds so an embedding application can report a precise
capability gap. It returns `backend: "unsupported"`, false operational
capabilities, no promised companions, and setup `unsupported`.

## Setup operations

The request vocabulary is retained for platforms that may need idempotent
preparation. `setup_status` carries the same working directory, policy base,
filesystem, network, and platform-extension context as launch. `setup`
additionally carries `operation: "prepare"` or `"refresh"`.
Possible setup states are `not_required`, `ready`, `refresh_required`,
`unavailable`, `unsupported`, `administrative_action_required`, and `failed`.
Possible successful operations are `already_ready`, `prepared`, and
`refreshed`.

macOS and Linux report `not_required` through discovery. A valid explicit
setup request returns `already_ready` without mutation. Windows reports
`unsupported` and rejects setup before any mutation. No operation initializes
normal Codex user state or opens an interactive installer.

## Launch request

The target command is fixed by native bootstrap. `launch` supplies only policy
and lifecycle information:

```json
{
  "type": "launch",
  "id": 3,
  "protocol_version": 1,
  "launch": {
    "working_directory": "/absolute/work",
    "policy_base_directory": "/absolute/policy-base",
    "filesystem": {
      "base": "platform_minimal",
      "rules": [
        {"path":"/absolute/input","access":"read","missing":"error"},
        {"path":"/absolute/output","access":"write","missing":"error"},
        {"path":"/absolute/output/private","access":"deny","missing":"ignore"}
      ]
    },
    "network": {"mode":"denied"},
    "streams": {
      "stdin":{"mode":"null"},
      "stdout":{"mode":"passed_handle","handle":10},
      "stderr":{"mode":"inherited"}
    },
    "terminal": "preserve",
    "lifecycle": {
      "kind":"command",
      "root_exit_grace_ms":1000,
      "terminate_grace_ms":1000,
      "force_timeout_ms":5000
    },
    "platform_extensions": {}
  }
}
```

All policy validation, companion checks, proxy startup, sandbox translation,
and stream validation happen before the private launch gate is released. There
is no unsandboxed fallback. A successful `launch_accepted` response carries
`backend` and an optional `root_process_id`. Linux returns the PID as null
because a host identifier is not a stable
part of the PID-namespace contract. `launch_accepted` means the sandbox bridge
reached its ready gate and supervision owns the generation. Native target
`exec` failure remains observable in the final infrastructure outcome.

## Filesystem policy

`filesystem.base` is one of:

- `platform_minimal`: Codex's current minimal platform-readable baseline;
- `host_read_only`: host root readable unless a more specific rule denies it.

Every rule has an absolute Unicode `path`, `access` (`read`, `write`, or
`deny`), and `missing` (`error` or `ignore`). `error` rejects an absent path;
`ignore` omits only that rule. Paths are not created by policy compilation.
The working directory and policy base are separate existing absolute
directories.

The runner constructs Codex `FileSystemSandboxPolicy` entries and passes them
through `PermissionProfile::from_runtime_permissions` and
`SandboxManager::transform`. It does not implement a second platform policy
translator. The externally reported precedence is:

```text
more specific path, then deny, then write, then read
```

Thus a specific denial protects a subtree of a broader readable or writable
root. Conflicts at equal specificity choose deny, then write, then read.
Backend translation must enforce every resulting rule or launch fails.

The runner adds read access for the absolute target and required infrastructure
resources. It rejects a caller denial covering the target. The application
state directory and its canonical spelling receive explicit denials; caller
rules cannot target it or override the runner and companion resources. A broad
writable ancestor therefore does not make private state writable.

## Network policy

Static `mode` values are `denied` and `unrestricted`. `denied` selects the
native restricted network policy. `unrestricted` selects
the native enabled policy. Neither may be substituted for the other.

Managed form:

```json
{
  "mode":"managed_proxy",
  "access":"limited",
  "allowed_domains":["example.com","*.example.org"],
  "denied_domains":["blocked.example.org"],
  "socks":true,
  "socks_udp":false,
  "upstream_proxy":false,
  "local_binding":true,
  "loopback":"allow",
  "local_ports":[],
  "unix_sockets":[]
}
```

Managed mode uses the release's `codex-network-proxy` configuration, domain
validation, redirect handling, HTTP proxy, optional SOCKS proxy, optional
trusted upstream proxy, target-environment preparation, and shutdown. The
runner chooses loopback listeners, owns them for the generation, injects the
proxy environment, and combines them with native direct-egress confinement.
It never silently changes managed mode to denied or unrestricted.

`access` is `full` or `limited`. Domain allow and deny patterns use the syntax
accepted by the current Codex proxy; invalid policy fails before target start.
When rules conflict, denial wins in the proxy's policy evaluation.

Capabilities define platform differences:

- macOS supports the reported loopback/local-binding controls and typed
  Unicode Unix-socket allow rules;
- Linux requires `local_binding: true` with `loopback: "allow"` and rejects
  Unix-socket rules;
- all supported backends reject mismatched loopback/local-binding pairs;
- Unix-socket deny, SOCKS UDP, explicit local ports, managed CA/TLS
  interception, non-loopback listeners, credential brokerage, secret/header
  injection, approval hooks, and interactive elicitation are unsupported.

Managed proxy shutdown is part of cleanup. A valid target exit followed by a
proxy shutdown error remains a valid target outcome with a separate
`infrastructure.cleanup_error`.

## Platform extensions

The extension point is closed and typed: `macos` and `linux` are empty objects;
the reserved `windows` object contains only `private_desktop`. Only the object
for the current host may be present. macOS and Linux currently
have no extension fields. The Windows object is reserved, but Windows launch
is unsupported. Every unknown extension field is rejected. Version 1 accepts
no raw SBPL, seccomp program, backend script, or arbitrary native policy text.

## Environment

The runner's launch environment is the intended complete target environment.
Protocol version 1 requires every name and value to be Unicode. Before target
launch the runner removes only names beginning `MCP_CONSOLE_SANDBOX_` and lets
the managed proxy add or replace its documented proxy variables. It does not
strip normal `CODEX_*`, `CARGO_*`, or application variables.

On Unix, any inherited name beginning `LD_` or `DYLD_` is rejected before
target start. The embedding application must also launch the runner itself
under a trusted loader-sanitized environment because those variables can affect
the runner before it can inspect them. Errors name a rejected key but never
print its value, credentials, or a complete environment map.

The sandbox command uses `env_clear()` and reconstructs exactly this filtered
environment plus values required by managed networking or the native backend.
Private control descriptors are close-on-exec and are not represented by
target environment variables.

## Standard streams and terminals

Each of `stdin`, `stdout`, and `stderr` independently uses `inherited`, `null`,
or `passed_handle`. Its unsigned numeric `handle` is declared through
`--stream-fd` or `--stream-handle`. The runner duplicates or transfers the native endpoint
directly into the transformed target command and closes its unnecessary copies
after supervision is installed. It never proxies target bytes through async
tasks or the control protocol. Binary transparency, independent stdout/stderr,
native buffering, terminal detection, and EOF therefore remain OS-native.

`terminal: "preserve"` preserves inherited terminals and caller-supplied PTYs.
On macOS, a `command` launch whose selected endpoint is the runner's or
launcher's foreground controlling terminal assigns it to the target process
group before gate release and restores the original foreground process group
after root exit. A `service` launch never transfers terminal foreground
ownership. PTYs
created by the target are governed by the native sandbox and advertised
capability. `isolate_host_devices` is rejected in version 1 rather than
silently weakened. Reopening an unrelated terminal device is not claimed as
isolated unless a future capability explicitly says so.

## Launch gate and process ownership

Supported Unix launches use one private ready gate:

1. the runner transforms a self-reexecution bridge with the selected native
   sandbox;
2. the bridge verifies native confinement and denial of a runner-owned state
   canary, reports ready, and blocks;
3. the runner installs supervision and transfers stream ownership; on macOS,
   an independent lifetime manager commits exact-identity descendant tracking;
4. the runner assigns any foreground terminal, releases unnecessary endpoint
   copies, opens the gate, and returns `launch_accepted`;
5. the bridge restores inherited ignored HUP, INT, and TERM dispositions and
   directly `exec`s the native target;
6. close-on-exec status distinguishes no reported exec error from an explicit,
   bounded exec-error frame.

This bridge is infrastructure, not an application-stream relay or a second
sandbox implementation. Direct invocation outside the selected native sandbox
fails before target execution. The native wrapper's wait status projects the
target exit status. On Linux, bubblewrap represents a signal death as the
conventional `128 + signal` exit code. macOS uses the root process group for
direct interrupts, while termination and retirement use the exact identities
in the complete observed tree across process-group and session changes. Linux
uses the release's bubblewrap PID/session namespaces and helper, plus an outer
process group and parent-death signal. The namespace retires session-escaping
descendants when its root exits, so Linux may proceed directly from `running`
to `retired` without an externally observable `root_exited` grace phase.
Descendants are retired at bounded deadlines under the capabilities claimed by
discovery.

Linux stores synthetic-mount registry data under
`<state-dir>/bwrap-synthetic-mount-registry`. The helper alias and exact
`codex-resources/bwrap` path remain alive for the generation.

## Interrupt, termination, and wait

`interrupt` sends the native interrupt supported by the backend. It is
supported on macOS and rejected on Linux.

`terminate` carries:

```json
{"graceful_ms":1000,"force_ms":5000}
```

macOS first requests graceful termination of the complete observed tree and
force-kills its remaining identities at the deadline. Linux cannot project graceful
termination through this release's isolated bubblewrap session, so
`graceful_ms` and launch `terminate_grace_ms` must be zero; termination is
forced.

Launch lifecycle fields are all explicit; there are no protocol defaults:

- `kind`: `command` or `service`; command launches may take terminal foreground
  ownership, while service launches do not;
- `root_exit_grace_ms`: time descendants may remain after root exit;
- `terminate_grace_ms`: natural post-root graceful retirement period;
- `force_timeout_ms`: bound for observing force retirement.

Lifecycle, terminate, and wait timeouts may not exceed 300,000 ms. `wait`
carries `retirement_timeout_ms` and returns `final` only when the runner has an
outcome or returns a structured timeout/cleanup error.

On macOS the normal lifetime-manager window is
`root_exit_grace_ms + terminate_grace_ms + force_timeout_ms + 1,000 ms`. If the
manager does not complete, the runner kills its exact identity and reserves a
second `force_timeout_ms + 1,000 ms` window for fallback recovery. The maximum
normal-final window is therefore
`root_exit_grace_ms + terminate_grace_ms + 2 * force_timeout_ms + 2,000 ms`.
Control loss uses two `force_timeout_ms + 1,000 ms` windows. After a final
outcome, the runner keeps the root waitable, pinning its PID and process-group
identity, until control-channel EOF; reaping then has a one-second allowance.

## Status and final outcome

`status` returns:

```json
{
  "phase":"root_exited",
  "target":{"kind":"exited","code":0,"signal":null,"error":null},
  "retirement":null
}
```

`final` keeps three independent concepts:

```json
{
  "type":"final",
  "id":9,
  "outcome":{
    "target":{"kind":"signaled","code":null,"signal":15,"error":null},
    "retirement":{"complete":true,"forced":false,"error":null},
    "infrastructure":{"error":null,"cleanup_error":null}
  }
}
```

`target` is null when no trustworthy target result exists, including native
`exec` failure. Unix target kinds are `exited`, `signaled`, and `unknown`.
`retirement.complete` describes the owned tree, not merely the target root;
`forced` records whether force was used. Infrastructure launch/observation
failure and post-target cleanup failure are distinct. Target exit code 1 is
therefore not confused with failure to create a sandbox, and incomplete
descendant cleanup does not overwrite a valid target exit.

## Control loss and fail-closed rules

EOF or failure on the trusted control channel retires a started target tree,
waits under the bounded control-loss windows above, and shuts down the managed
proxy. There is no reconnection. Closing the gate before release prevents target
application code from running. On macOS the independent lifetime manager also
retires the complete observed tree after uncatchable outer-runner termination
such as `SIGKILL`. If that manager itself fails or stops responding while the
root identity is still live, the outer runner reconstructs the reachable tree
and performs bounded fallback cleanup.

Before launch, each requested capability is either validated and passed to the
native implementation or rejected with a structured error. The runner never:

- executes without a selected native sandbox;
- widens `platform_minimal` or drops read/write/deny/missing-path policy;
- changes the requested network mode or permits managed-proxy direct egress;
- grants target write access to runner state or private resources;
- substitutes root-only lifetime for a claimed tree-retirement contract;
- converts a native value lossily, searches arbitrary helper paths, or invokes
  a shell;
- loads normal Codex configuration, authentication, approvals, or sessions.

Errors contain `code`, `phase`, `message`, and `target_started`. Source-chain
context may be included, but environment maps, tokens, private proxy headers,
and credentials must not be included.
