# MCP Console sandbox protocol version 1

This document defines the private machine interface exposed by `mcp-console-sandbox`. The executable protocol, including its native bootstrap arguments and companion layout, is the downstream API. Rust types are release-local implementation details.

## Native bootstrap

On Unix, invoke the runner as:

```text
mcp-console-sandbox \
  --state-dir <state-path> \
  --control-fd <fd> \
  [--stream-fd <fd>]... \
  -- <absolute-program> <argument>...
```

On Windows, use `--control-handle <handle>` and repeatable `--stream-handle
<handle>` arguments.

`state-path`, `absolute-program`, and all trailing arguments are native
operating-system values. The program and arguments never enter JSON and are
never passed through a shell. Version 1 requires an absolute existing target
executable with a valid-Unicode path and defines no target `PATH` lookup. The
runner rejects a non-Unicode program path rather than converting it lossily for
native policy translation. Unix arguments preserve `OsString` bytes; the
Windows standalone helper transports native UTF-16 argument code units.
Embedded NUL is invalid.

The control endpoint is a connected, bidirectional native socket, pipe, or
handle created by the caller. It is private infrastructure and is separate
from stdin, stdout, and stderr. The caller must make the runner side of the
control endpoint and every endpoint named by `--stream-fd` or
`--stream-handle` inheritable into the runner child. Peer endpoints and all
unrelated handles remain private to the caller. The runner makes claimed
endpoints non-inheritable before processing requests. Every endpoint later
selected by a protocol `passed_handle` must be declared exactly once, and
every declared endpoint must be used by the launch. The runner claims these
values synchronously before creating the async runtime, state, proxy, setup
session, or helper processes. It rejects invalid, duplicate, control,
standard-stream, undeclared, and unused endpoints. Native process creation
receives private duplicates, and the runner closes its original copies after
target creation.

Bootstrap failures before the control endpoint is usable are unframed.
Command-line parsing, invalid bootstrap endpoints, runtime creation,
infrastructure-path validation or canonicalization, and state creation write
to stderr and exit with status 2 before target start. The caller must treat
this as infrastructure failure, not a target outcome.

`state-path` must be absolute and valid Unicode. The runner rejects it before
creating state if that condition does not hold, creates it if needed,
canonicalizes it, verifies the canonical result again, and uses only that
application-owned directory for mutable runner state. The canonical runner
executable and companion-resource paths must also be valid Unicode. The runner
does not discover or initialize a normal Codex home. Windows state, target,
working, policy, and helper paths must be absolute local-disk paths rather than
UNC paths.

The native bootstrap transports `state-path` without lossy conversion but
protocol version 1 intentionally applies the Unicode boundaries above. Windows
working and policy paths are already Unicode JSON values; native target
arguments may retain unpaired UTF-16 surrogate code units, but the target
program path may not.

### Companion layout

Companions are resolved only relative to the canonical runner executable:

```text
<runner-dir>/mcp-console-sandbox
<runner-dir>/codex-resources/bwrap
<runner-dir>/codex-resources/codex-windows-sandbox-setup.exe
<runner-dir>/codex-resources/codex-command-runner.exe
```

Linux requires only `bwrap`; Windows requires only the two `.exe` files.
macOS declares no companion. Production discovery never searches `PATH`.

On Linux, the same focused executable self-dispatches with argv[0]
`codex-linux-sandbox`. Private paired arguments pin the canonical bubblewrap
and state paths. Before availability or launch succeeds, the runner executes
that exact companion with one private compatibility query. Success requires a
zero exit, empty stderr, and one exact private companion protocol-2 and
Codex-release token on stdout within two seconds and 1,024 bytes. The private
companion ABI version is independent of this document's public runner protocol
version 1. Extra output, timeout, signal,
nonzero exit, or a different token rejects the companion before target start.
The bubblewrap implementation repeats the check, reopens the selected file at
exec, and verifies the embedded digest when the build supplied one. Bazel adds
that digest identity; Cargo relies on the compatibility token and trusted
immutable private layout.

On Windows, setup and launch pass exact absolute local-disk paths to the two
helpers. Before operational capability discovery, setup, or launch, the runner
executes both exact paths with these closed private queries and responses:

```text
--codex-mcp-console-sandbox-windows-setup-compatibility-v1
mcp-console-sandbox-windows-setup/1 codex/0.150.1\n

--codex-mcp-console-sandbox-windows-command-runner-compatibility-v1
mcp-console-sandbox-windows-command-runner/1 codex/0.150.1\n
```

The runner clears the query environment, supplies null stdin, and permits at
most 1,024 bytes on each output stream within two seconds. Success requires a
zero exit, empty stderr, and byte-exact stdout. Each helper handles only its own
query before normal initialization, so inspection neither changes setup state
nor requests UAC. These helper-specific private ABI-1 tokens are tied to the
Codex workspace release and are independent of public protocol version 1. The
command helper's private target pipe protocol remains an implementation detail,
not another downstream API.

## Framing

Each public control message is one frame:

1. a four-byte unsigned big-endian payload length;
2. exactly that many bytes of UTF-8 JSON.

The maximum JSON payload is 1,048,576 bytes. The prefix is not part of the
limit. Oversized, truncated, or malformed framing closes the runner after a
structured error is attempted. A zero-length payload is malformed JSON.
Target application bytes never use this channel.

The Unix target-exec bridge uses separate private readiness, preparation gate,
commit gate, and length-prefixed status channels. It first reports that it is
waiting, then blocks until the outer runner has installed its owner-loss
watchdog. After the preparation gate is released, the bridge creates a focused
target-exec shim. The shim reports ready while the requested target application
is still blocked. The outer runner installs process-tree supervision, transfers
infrastructure ownership, closes its target-stream copies, and then commits the
launch. A close-on-exec status pipe confirms that the shim became the requested
target before `launch_accepted` is sent. The status frame limit is 16,384 bytes.
Every private descriptor is close-on-exec and never reaches the target. After
that confirmation, resident bridge and Linux outer-monitor processes also
release their copies of target stdin, stdout, and stderr before launch
acceptance.

## Request and response envelope

Every request is a JSON object containing:

- `type`, a snake-case operation name;
- `id`, an unsigned 64-bit request identifier;
- `protocol_version`, exactly `1`;
- the fields defined for that operation.

For example:

```json
{"type":"discover","id":1,"protocol_version":1}
```

Responses echo `id`. A frame or JSON error for which no identifier could be
decoded uses `"id": null`.

```json
{
  "type": "error",
  "id": 7,
  "error": {
    "code": "version_mismatch",
    "phase": "protocol",
    "message": "protocol version 2 is unsupported; this runner requires 1",
    "target_started": false
  }
}
```

All request objects, nested authority-bearing objects, and tagged variants
reject unknown fields. Unknown request types and enum values are invalid.
Consumers may ignore additive descriptive response fields, but must never
infer support for an unknown authority-bearing field or a capability reported
as false.

A version mismatch returns `version_mismatch`, cannot start a target, and
leaves the control loop available for another request.

## State machine

One runner owns at most one accepted target generation:

```text
idle --launch--> running --> root_exited --> retired
```

`retiring` and `failed` are reserved phase values in version 1; this release
does not emit them. The operations are:

| Operation | Validity and result |
| --- | --- |
| `discover` | Accepted in every phase; returns `capabilities` |
| `setup_status` | Accepted only while idle, before a generation; returns `setup_status` |
| `setup` | Accepted only while idle; explicit on Windows and rejected on macOS and Linux |
| `launch` | Accepted only before a generation has been created |
| `status` | Accepted in every phase; returns the current snapshot |
| `interrupt` | Requires a live backend that reports interrupt support |
| `terminate` | Requires a non-retired generation and supported termination semantics |
| `wait` | Requires a generation; returns `final` or a bounded observation error |

Validation, companion, proxy-startup, sandbox-preparation, and other failures
before the backend may create a target are reported with
`target_started=false`; they leave the runner idle and may be corrected before
another `launch`. Once the Unix preparation gate is released or a Windows
`Spawn` request may have been sent, any subsequent failure or uncertainty is
reported with `target_started=true` and consumes the generation even when the
target application remained blocked and the runner could not send
`launch_accepted`. Later setup and launch operations return `invalid_state`.
After `launch_accepted`, another launch always returns
`invalid_state`. `retired` means tree retirement is observed; infrastructure
cleanup may still be finishing. Once `final` is cached, later `wait` calls
return it, but a short `wait` immediately after a `retired` status can still
time out while cleanup completes. There is no reconnect, persistence, target
replacement, or multiplexing.

## Discovery

`discover` returns `capabilities` containing:

- protocol version and maximum public frame size;
- runner package version, exact Codex source revision, and release tag;
- operating system, architecture, and native backend;
- filesystem, network, stream, terminal, and lifecycle feature booleans;
- required companion names and relative paths;
- current setup state.

The serialized `capabilities` object has these exact version 1 fields:

- top level: `protocol_version`, `maximum_frame_size`, `runner_version`,
  `codex_source_revision`, `codex_release_tag`, `operating_system`,
  `architecture`, `backend`, `filesystem`, `network`, `streams`, `terminal`,
  `lifecycle`, `required_companions`, and `setup`;
- `filesystem`: `platform_minimal`, `host_read_only`, `read_rules`,
  `write_rules`, `deny_read_rules`, `deny_write_rules`,
  `missing_path_error`, `missing_path_ignore`, `precedence`,
  `state_directory_protected`, and `unicode_policy_paths_only`;
- `network`: `denied`, `unrestricted`, `managed_proxy`, `full_access`,
  `limited_access`, `http`, `socks`, `socks_udp`, `upstream_proxy`,
  `domain_allow_patterns`, `domain_deny_patterns`, `local_binding_policy`,
  `loopback_policy`, `unix_socket_policy`, `unix_socket_allow_rules`,
  `unix_socket_deny_rules`, `explicit_local_ports`, `managed_ca`,
  `direct_egress_confinement`, and `non_loopback_listeners`;
- `streams`: `inherited`, `passed_handle`, `null`, `independent`,
  `byte_transparent`, and `application_bytes_on_control_channel`;
- `terminal`: `inherited_terminal`, `caller_supplied_pty`,
  `controlling_terminal_reopen`, `pty_creation_inside_sandbox`, and
  `host_device_isolation`;
- `lifecycle`: `interrupt`, `graceful_termination`, `forced_termination`,
  `root_exit_observation`, `process_tree_supervision`,
  `full_tree_retirement`, `cleanup_after_root_exit`, and
  `control_loss_retires_target`.

Each `required_companions` entry is
`{"name":string,"relative_path":string,"required":boolean}`. `setup` is
`{"state":state,"detail":string|null}`. `local_binding_policy` and
`loopback_policy` report whether the choice is configurable; accepted value
pairs remain the network-policy contract below.

`network.unix_socket_allow_rules` and `network.unix_socket_deny_rules` report
the two authorities independently. The retained `network.unix_socket_policy`
field is a compatibility summary that means at least one typed Unix-socket
rule is supported; it does not imply support for both authorities. Consumers
must check the authority-specific field before requesting a rule.

The backend enum is `macos_seatbelt`, `linux_bubblewrap`,
`windows_restricted_token`, `windows_elevated`, or `unsupported`. The accepted
launch reports the backend actually selected. A Windows launch uses the
standalone elevated helper with either the online or offline restricted
identity; policy-dependent identity selection does not expose a new backend
language.

The source revision is a full 40-hex Git SHA stamped from
`STABLE_GIT_COMMIT` when supplied or from the repository `HEAD` at build time.
There is no fallback revision. A source-archive build without an explicit
stamp, an unavailable Git `HEAD`, or a non-full SHA fails. An embedding
application should reject a revision other than the immutable revision it
packaged.

When the native backend or a required companion is unavailable, setup reports
`unavailable`, operational capabilities are false, and launch fails before
target validation or execution. Capabilities describe enforcement available
on that concrete host, not merely code compiled into the binary.

Linux concrete-host discovery first verifies the exact packaged companion and
then runs a two-second no-op through the same self-reexecution, helper, and
inner target path used for launch. The no-op requests `platform_minimal`, an
explicit read rule for the runner, and denied networking. It therefore checks
the required user, PID, IPC, and network namespaces, `/proc` mount selection,
seccomp, and target dispatch. The probe runs in a private process group with a
parent-death signal; the runner kills that group before reaping the probe on
success, failure, or timeout. The runner includes at most 4,096 bytes of probe
stderr in the structured unavailable detail and marks truncation. The probe
has an empty environment and no target application input. Any probe failure
reports the backend unavailable and makes every operational capability false.

The available-backend matrix is:

| Area | macOS | Linux | Windows |
| --- | --- | --- | --- |
| `platform_minimal` filesystem base | yes | yes | no; request rejected |
| `host_read_only`, read/write/deny, missing-path modes, protected state | yes | yes | yes |
| Network denied, unrestricted, managed HTTP/SOCKS, upstream, domain rules, direct-egress confinement | yes | yes | yes |
| Configurable loopback/local binding | yes | no; fixed allow/true pair only | yes |
| Managed Unix-socket allow rules | yes | no | no |
| Managed Unix-socket deny rules | no | no | no |
| SOCKS UDP, explicit local ports, managed CA, non-loopback listeners | no | no | no |
| Inherited, null, and passed streams, independently and byte-transparently | yes | yes | yes |
| Inherited terminal and caller PTY semantics | yes | yes | no terminal-specific claim |
| Controlling-terminal reopen | yes | no | no |
| PTY creation plus host terminal-device isolation | yes | no | no |
| Interrupt and graceful signal termination | yes | no | no |
| Forced termination, root observation, cleanup, control-loss retirement | yes | yes | yes |
| Full-tree retirement | no; process group only | yes | yes; Job Object |

macOS reports `process_tree_supervision=false` as well as
`full_tree_retirement=false`: its process-group supervision is useful but does
not establish ownership of descendants that deliberately leave the group.

## Setup

Setup is policy-dependent. The `setup` object contains the launch fields that
affect native preparation: `working_directory`, `policy_base_directory`,
`filesystem`, `network`, and `platform_extensions`. It intentionally omits
streams, terminal behavior, and lifecycle deadlines.

The inspection request is:

```json
{
  "type": "setup_status",
  "id": 2,
  "protocol_version": 1,
  "setup": {
    "working_directory": "C:\\work",
    "policy_base_directory": "C:\\work",
    "filesystem": {"base":"host_read_only","rules":[]},
    "network": {"mode":"denied"},
    "platform_extensions": {"windows":{"private_desktop":false}}
  }
}
```

An explicit operation uses:

```json
{
  "type": "setup",
  "id": 3,
  "protocol_version": 1,
  "operation": "prepare",
  "setup": {
    "working_directory": "C:\\work",
    "policy_base_directory": "C:\\work",
    "filesystem": {"base":"host_read_only","rules":[]},
    "network": {"mode":"denied"},
    "platform_extensions": {"windows":{"private_desktop":false}}
  }
}
```

`operation` is `prepare` or `refresh`. Setup states are `not_required`,
`ready`, `refresh_required`, `unavailable`, `unsupported`,
`administrative_action_required`, and `failed`.

With an available backend, macOS and Linux return `not_required`; an explicit
setup operation returns `unsupported_platform` and cannot start a target. The
common request shape still requires the closed `setup` object. On those
platforms `setup_status` validates platform extensions and managed-network
authority, but does not resolve working, policy, or filesystem paths. `launch`
performs full path and policy validation before target start. A missing native
backend or required companion reports `unavailable` instead.

The policy-free setup summary inside Windows `discover` is
`refresh_required` once both companions are present. It tells the caller to
send policy-specific `setup_status`; only that response can report whether the
requested Windows state is ready.

The Windows standalone setup seam is idempotent and scoped to `--state-dir`:

- status inspection is read-only, never requests UAC, compares the requested
  filesystem policy with the last successfully applied ACL generation, and
  verifies the actual fixed machine-global firewall rules under a short lease;
- filesystem-policy drift reports `refresh_required`; `prepare` applies its
  non-elevating ACL refresh and reports `refreshed`, while initial identity or
  firewall preparation reports `prepared` and is the only path that may request
  UAC;
- `refresh` requires prepared identities, never requests UAC, and reapplies
  path ACL state without changing WFP rules;
- launch acquires its lifetime lease, verifies the exact outbound, loopback,
  port-complement, and stale-rule state, and refreshes policy-dependent ACLs
  before it starts a target;
- missing or incompatible identity markers and changed offline firewall
  policy report that administrative preparation is required;
- a setup operation is successful only after every required filesystem ACL
  mutation and stale-denial revocation succeeds;
- invalid helpers, paths, identities, or state report unavailable or a
  structured setup failure.

Setup prepares the online and offline sandbox identities, filesystem ACLs, and
WFP policy. Its policy-specific inputs are derived from the normalized launch
policy. Setup adds platform default reads when required, grants the command
helper read access, and denies writes to the state directory and both packaged
helpers. The typed private-desktop choice is applied by the command helper at
launch. Setup never initializes the normal Codex home and emits no Codex
telemetry from the focused runner.

The caller-owned state directory holds setup markers, protected identity
credentials, helper scratch files, and diagnostics. The Windows identities and
WFP rules are system state, and filesystem ACLs apply to the requested host
paths; those resources are not represented as files beneath the state
directory.

The standalone identities, group, firewall rule names, WFP filter keys, and
setup mutexes occupy a fixed MCP Console namespace distinct from ordinary
Codex. The standalone namespace supports one active Windows policy generation
machine-wide and uses its own non-inheritable global lease. Launch acquires the
lifetime lease, uses the exact setup companion's shared rule specifications to
verify the installed standalone generation, refreshes ACL state, and rechecks
local setup state before creating a target. A marker from another state
directory or identity namespace cannot stand in for that verification. A
verification failure carries at most 16 KiB of redacted helper diagnostics
naming the mismatched rule or property; the helper runs with an empty
environment, and the payload itself is not included. A busy or abandoned lease
returns `setup_failed` or `launch_failed` with `target_started=false`; it never
mutates standalone policy or starts a target. An abandoned mutation requires a
fresh `prepare` operation. The supervisor holds the lifetime lease through full
Job retirement, confirmed standalone command-helper process exit, and proxy
cleanup. The helper process exit is the boundary proving that its kill-on-close
Job handle and other helper-owned handles are closed. The helper releases its
target thread handle immediately after resume and its target process handle
immediately after recording root exit, before observing Job retirement. The
native final event ends the helper command channel. The runner therefore closes
its control writer immediately, waits five seconds for normal helper exit, then
force-terminates and confirms the helper at a bounded deadline if necessary.
Forced helper exit is reported in `infrastructure.cleanup_error` without
replacing the target or Job retirement result. If helper exit cannot be confirmed, no
`final` is made observable and the lifetime lease remains held. Once helper
exit and proxy cleanup are complete, the lease is released immediately before
publishing `final`.

This ordering is also the Windows helper-sealing trust boundary. The fixed MCP
Console sandbox identities, their protected credentials, and their state are
private infrastructure and must not be used to start processes outside this
runner. Version 1 supports only runner-created processes under those identities;
independently launched same-identity processes are outside the contract. The
supported invariant combines confirmed prior-helper exit with the machine-global
one-generation lease. The new helper is created suspended and sealed before it
runs, and its gated control thread is sealed before the suspended target resumes.
Administrators and processes with `SeDebugPrivilege` are outside the sandbox
authority model.

For managed networking, setup starts and retains the Codex proxy so that the
Windows Firewall rules and later target launch use the same selected listener
ports and restricting SID. The fixed WFP filters continue to enforce the
standalone identity boundary. Repeating the same setup policy reuses that
prepared session; changing it shuts down the old proxy and prepares a
replacement. The policy portion of the later `launch` must match the prepared
setup request. Launch consumes the retained setup and proxy ownership; control
close before launch shuts them down.

A managed Windows embedding should therefore perform policy-specific setup and
launch in the same runner process. A fresh runner may prepare during launch,
but incompatible identity markers or different proxy ports require explicit
setup and fail before the target starts.

Successful setup returns `setup_completed` with `operation` equal to
`already_ready`, `prepared`, or `refreshed`.

## Launch

The native bootstrap carries the target. `launch` carries normalized policy:

```json
{
  "type": "launch",
  "id": 4,
  "protocol_version": 1,
  "launch": {
    "working_directory": "/private/work",
    "policy_base_directory": "/private/policy",
    "filesystem": {
      "base": "host_read_only",
      "rules": [
        {
          "path": "/private/work/output",
          "access": "write",
          "missing": "error"
        }
      ]
    },
    "network": {"mode":"denied"},
    "streams": {
      "stdin": {"mode":"inherited"},
      "stdout": {"mode":"inherited"},
      "stderr": {"mode":"inherited"}
    },
    "terminal": "preserve",
    "lifecycle": {
      "kind": "command",
      "root_exit_grace_ms": 5000,
      "terminate_grace_ms": 0,
      "force_timeout_ms": 5000
    }
  }
}
```

All JSON path fields must be valid Unicode and absolute. The working and policy
base directories must exist and may differ. Windows additionally requires
absolute local-disk paths. `platform_extensions` may be omitted and defaults
to an empty closed object.

On Unix, the in-sandbox bridge waits at a private preparation gate until the
outer runner has started its owner-loss watchdog. Watchdog failure or runner
death before gate release closes the gate without creating the target and is
retryable. After release, the bridge creates a target-exec shim that reports
ready but cannot execute the requested application until the outer runner has
installed supervision and sent a commit. The bridge acknowledges the commit
only after the shim's close-on-exec channel proves native `exec` success.
`launch_accepted` follows that acknowledgement. A missing interpreter or any
other error or uncertainty after preparation-gate release returns
`launch_failed`, phase `launch`, with `target_started=true`; it consumes the
generation and remains observable through `status` and `wait` even when target
application code never ran.

On Windows, the outer runner creates the standalone helper suspended, verifies
that the runner and sandbox `TokenUser` SIDs differ, and replaces the helper
process and initial-thread owners and protected DACLs with runner- and
`SYSTEM`-only access before resuming it. Failure leaves the helper suspended
and terminates it before it can create a target. The exact runner SID crosses
only the closed helper wire. The helper creates its later control thread behind
a gate and installs a protected runner- and `SYSTEM`-only DACL plus an explicit
`OWNER RIGHTS` denial before the target can resume. A failure to create or seal
that thread retires the still-suspended target Job. The standalone helper then creates the target suspended
and assigns it to its non-breakaway Job Object before sending `Ready`. The outer runner first
installs the supervisor and releases unnecessary stream copies, then sends
`CommitLaunch`. The helper resumes the target and sends `Committed` before the
runner returns `launch_accepted`. Once the `Spawn` request may have reached the
helper, loss of `Ready`, loss of the commit acknowledgement, or another
uncertainty consumes the generation and is reported with `target_started=true`.
The accepted PID is the target root PID, not an infrastructure process.

`launch_accepted.root_process_id` is optional and informational. macOS and
Windows return a host-visible target PID. Linux returns `null` because the
target PID is scoped to bubblewrap's PID namespace. Consumers must use
lifecycle protocol operations rather than treat this value as portable.

Every deadline in launch, terminate, and wait is limited to 300,000 ms.

## Filesystem policy

`filesystem.base` is:

- `platform_minimal`: Codex's current minimal platform/runtime reads followed
  by explicit rules;
- `host_read_only`: host reads with writes limited to explicit writable roots
  and native runtime resources.

Windows reports `filesystem.platform_minimal=false` and rejects that base
before setup or target creation. Its current restricted-token and ACL facade
exports `host_read_only` only; the runner does not approximate the missing
minimal base by widening it.

Each rule contains an absolute Unicode `path`, `access` of `read`, `write`, or
`deny`, and `missing` of `error` or `ignore`. A missing `error` path rejects
launch; a missing `ignore` path contributes no rule. `read` grants reads,
`write` grants reads and writes, and `deny` denies both reads and writes for the
selected subtree.

The runner constructs Codex's shared `FileSystemSandboxPolicy` once. Codex
resolves conflicts by specificity: a more-specific path wins; at equal
specificity, deny wins over write, and write wins over read. Backend-specific
translation consumes that shared result. A denial is never discarded; a
combination the selected backend cannot enforce fails before target start.

Rules inside the canonical runner state directory, including rules whose path
resolves there through another spelling, are rejected. The native policy gets
an explicit denial for the canonical state path, so a broad writable ancestor
cannot expose state. The in-sandbox Unix launch bridge receives a specific
readable rule for the runner executable, and every backend receives the
specific target-executable read needed to start the requested program. A deny
covering the target executable is rejected before launch rather than being
overridden by that required read. Any non-read rule naming the runner
executable, the runner-relative companion directory, or a path below that
directory is rejected using both lexical and canonical spellings. Windows
setup additionally grants its command helper read access and adds explicit
deny-write ACL inputs for state and both helpers.

## Network policy

`network` is tagged by `mode`:

- `denied` requests the native restricted-network policy;
- `unrestricted` requests the native enabled-network policy;
- `managed_proxy` starts and owns Codex's managed proxy and applies native
  direct-egress confinement.

The `managed_proxy` object has:

- `access`: `full` permits every HTTP method and tunnels HTTPS CONNECT;
  `limited` permits only HTTP GET, HEAD, and OPTIONS, rejects HTTPS CONNECT
  because version 1 does not enable interception, and rejects non-HTTPS SOCKS
  TCP plus all SOCKS UDP;
- `socks` and `socks_udp` booleans;
- `upstream_proxy`, which controls use of a trusted inherited upstream proxy;
- `local_binding`;
- `loopback`: `allow` or `proxy_only`;
- optional `allowed_domains` and `denied_domains` arrays, each defaulting to
  empty;
- optional `local_ports`, an array reserved by version 1 and defaulting to
  empty;
- optional `unix_sockets`, absolute Unicode paths tagged with `access`,
  defaulting to empty. Version 1 accepts only `access=allow`; `access=deny` is
  reserved and fails with `unsupported_policy` before proxy or target startup.

Each domain entry is one of:

- an exact host such as `api.example.com`;
- `*.example.com`, which matches subdomains but not `example.com` itself;
- `**.example.com`, which matches `example.com` and its subdomains.

Protocol version 1 rejects every other glob form before proxy startup. If the
same pattern appears in both arrays, denial wins. Entries are hosts, not URL
authorities; a host-and-port form is rejected. `allowed_domains` is a positive
allowlist for proxied destinations, so an empty list permits none; this is
independent of the full-versus-limited method policy. Explicit local binding,
when supported and selected, remains the documented exception for local
traffic.

HTTP proxying is intrinsic. SOCKS is optional. `socks_udp=true` and nonempty
`local_ports` are rejected on every platform. Linux accepts only
`local_binding=true` with `loopback=allow`; macOS and Windows accept that pair
or `local_binding=false` with `loopback=proxy_only`. Unix-socket allow rules
are accepted only on macOS. Unix-socket deny rules and duplicate Unix-socket
paths are rejected. A mismatched loopback/local-binding pair, deny rule, or
duplicate Unix-socket path fails before proxy startup.

Managed mode uses `codex-network-proxy` for listener selection, domain rules,
redirect handling, optional SOCKS, trusted upstream use, and target proxy
environment values. Listeners are loopback-only. The runner selects a
restricted native network policy, supplies the proxy's native sandbox context,
and keeps the proxy handle through target retirement. Launch failure,
control-channel loss, or final retirement shuts it down.

Windows denied and managed modes select the WFP-confined offline identity.
Managed setup receives the proxy listener ports, local-binding policy, and the
optional proxy restricting SID. Unrestricted mode selects the online identity.
The proxy listener remains owned by the outer runner.

Version 1 does not expose managed CA or TLS interception, SOCKS UDP, explicit
local ports, non-loopback listeners, credential brokerage, secret or header
injection, approval callbacks, interactive elicitation, or a general gateway.
Managed mode never degrades to unrestricted or fully denied networking.

## Environment

The runner's launch environment is the intended complete target environment
outside reserved infrastructure namespaces. It clears the child environment
and reconstructs it after removing every inherited `CARGO_BIN_EXE_*`,
`CODEX_*`, and `MCP_CONSOLE_SANDBOX_*` key. Managed networking removes stale
ordinary proxy keys and injects only backend-owned replacements. No normal
Codex configuration is merged.

Managed mode injects the following documented target variables:

- `CODEX_NETWORK_PROXY_ACTIVE=1` and
  `CODEX_NETWORK_ALLOW_LOCAL_BINDING=0|1`;
- the managed HTTP URL in `HTTP_PROXY`, `HTTPS_PROXY`, `http_proxy`,
  `https_proxy`, `YARN_HTTP_PROXY`, `YARN_HTTPS_PROXY`,
  `npm_config_http_proxy`, `npm_config_https_proxy`, `npm_config_proxy`,
  `NPM_CONFIG_HTTP_PROXY`, `NPM_CONFIG_HTTPS_PROXY`, `NPM_CONFIG_PROXY`,
  `BUNDLE_HTTP_PROXY`, `BUNDLE_HTTPS_PROXY`, `PIP_PROXY`,
  `DOCKER_HTTP_PROXY`, `DOCKER_HTTPS_PROXY`, `WS_PROXY`, `WSS_PROXY`,
  `ws_proxy`, and `wss_proxy`;
- `NO_PROXY`, `no_proxy`, `npm_config_noproxy`, `NPM_CONFIG_NOPROXY`,
  `YARN_NO_PROXY`, and `BUNDLE_NO_PROXY`, set to Codex's loopback and private
  network list when local binding is enabled and to the empty string otherwise;
- `ELECTRON_GET_USE_PROXY=true` and `NODE_USE_ENV_PROXY=1`;
- `ALL_PROXY`, `all_proxy`, `FTP_PROXY`, and `ftp_proxy`, set to the managed
  SOCKS5h URL when SOCKS is enabled and to the managed HTTP URL otherwise.

Windows environment names are case-insensitive. On Windows, the runner emits
one uppercase key for each case-insensitive equivalence class in this list,
removes inherited aliases case-insensitively, and rejects conflicting managed
values instead of emitting duplicate aliases.

Windows setup temporarily uses `CODEX_WINDOWS_SANDBOX_PROXY_PORTS` to project
the selected loopback listener ports into the offline Windows Firewall policy,
then removes it from the target environment. Version 1 disables managed CA,
attribution, credential brokerage, and secret or header injection, so it
injects no variables for those features.

On macOS with managed SOCKS enabled, the runner preserves a caller-provided
`GIT_SSH_COMMAND` as its native value, including a non-Unicode value. If the
caller did not provide one, the runner injects Codex's managed SOCKS fallback.

On Unix, any native environment name whose bytes begin `LD_` or `DYLD_` is
rejected before target launch, including a non-UTF-8 name. The error names the
key but never its value. Because the native loader acts before runner code, the
embedding application must also start the runner under a trusted
infrastructure environment from which loader-affecting variables have been
removed. Windows matches runner-private names case-insensitively and uses the
same trusted-launcher boundary for loader-affecting variables. The runner never
prints a complete environment map, credentials, tokens, or private headers.

Windows environment values retain native UTF-16 code units, including unpaired
surrogates. Environment names must be valid Unicode, valid Windows names, and
unique under case-insensitive matching. Codex's setup-time filesystem-policy
projection uses only valid-Unicode values; a non-Unicode `TEMP` or `TMP` value
is rejected because it affects authority, while other non-Unicode values remain
in the native target environment.

## Standard streams and terminals

`stdin`, `stdout`, and `stderr` are independent. Each is one of:

```json
{"mode":"inherited"}
{"mode":"null"}
{"mode":"passed_handle","handle":17}
```

On Unix, `handle` is a nonnegative native descriptor representable as `i32`.
On Windows it is a native handle value. In both cases the value must name one
bootstrap endpoint claimed with `--stream-fd` or `--stream-handle`. The three
requested values must exactly match the claimed set. Bootstrap values must be
valid and pairwise distinct, and cannot name the control endpoint or a runner
standard stream. Control-channel alias checks compare native kernel-object
identity, so duplicating the control descriptor or handle under another value
does not make it a valid target stream.

An inherited Unix standard descriptor must be open at bootstrap and still open
when `launch` is validated. The Rust runtime maps a standard descriptor that was
closed at process entry to `/dev/null`; the runner rejects `/dev/null` in
inherited mode. On Windows, the runner duplicates each usable standard handle
before starting its async runtime and compares kernel-object identity again at
launch validation. An unavailable, closed, or replaced inherited handle is
rejected before setup. Callers that want the null device must request `null`
mode.

Each endpoint is attached directly with native process-creation primitives.
The runner does not read, decode, buffer, combine, multiplex, or relay target
bytes. Its unnecessary copies close during process creation, so target EOF is
controlled only by the target and intended endpoint owners. Control and helper
handles that the target does not need are non-inheritable. Application bytes
never appear on the control channel.

`terminal` is `preserve` or `isolate_host_devices`. On Unix, inherited
terminals and caller-supplied PTYs retain native descriptor and TTY-detection
semantics. On macOS, `preserve` keeps the controlling-terminal session and
supports `/dev/tty` reopening. `isolate_host_devices` instead denies
path-based reopening of `/dev/tty` and existing `/dev/ttys*`, while allowing
PTYs carrying the sandbox-created PTY property; inherited descriptors are
unaffected. Linux retains bubblewrap's `--new-session`, so it reports native
TTY descriptors but not controlling-terminal reopening, host-device
isolation, or in-sandbox PTY creation. Windows accepts ordinary direct handles
but rejects terminal isolation and claims neither ConPTY nor Unix signal
behavior.

## Platform extensions

`platform_extensions` is closed. Its possible keys are `macos`, `linux`, and
`windows`; foreign-platform extension objects and unknown fields are rejected.
A foreign-platform `null` value is equivalent to omission.

macOS and Linux extension objects are empty. Windows exposes only the typed
`private_desktop` boolean, defaulting to false. Terminal isolation is the typed
top-level field described above. Arbitrary SBPL, raw backend policy text, and
backend scripts are not accepted.

## Status, interruption, termination, and wait

`status` returns `phase` and any known target and retirement outcomes.

macOS `interrupt` sends SIGINT to the supervised process group. macOS
`terminate` sends SIGTERM, schedules SIGKILL after `graceful_ms` if the group
remains, and observes retirement until the additional `force_ms` deadline.
Linux cannot project either signal across bubblewrap's isolated session; its
launch `terminate_grace_ms` and terminate-operation `graceful_ms` must both be
zero, and terminate immediately force-retires the namespace generation.
Windows likewise supports neither interrupt nor a graceful signal; both
graceful deadlines must be zero, and `force_ms` bounds forced Job retirement.

```json
{
  "type": "terminate",
  "id": 8,
  "protocol_version": 1,
  "deadlines": {"graceful_ms":0,"force_ms":5000}
}
```

After natural root exit, macOS waits `root_exit_grace_ms`, sends SIGTERM, waits
`terminate_grace_ms`, sends SIGKILL if necessary, and observes the process
group for `force_timeout_ms`. macOS deliberately claims only that group.

On Linux, packaged bubblewrap reports the host PID of the final namespace PID
1 over its private `--info-fd` channel. The runner opens a pidfd and registers
it with the watchdog before releasing either target launch gate. The launch
bridge remains PID 1 after reporting the root outcome and reaps adopted
namespace descendants. If they retire during `root_exit_grace_ms`, retirement
completes naturally. Otherwise the runner marks the outcome forced, sends
SIGKILL through the namespace-PID1 pidfd and to the outer helper process group,
and observes both the pidfd and outer helper for `force_timeout_ms`. Linux also
uses `--die-with-parent` and an outer parent-death signal. It never projects
SIGTERM into the namespace.

After natural root exit, Windows gives descendants `root_exit_grace_ms`, then
force-terminates any remaining Job members and bounds that operation with
`force_timeout_ms`. It has no intermediate graceful-signal phase, so launch
`terminate_grace_ms` must be zero.

On Unix, a separate focused-runner watchdog owns a private socket. Unexpected
runner death or owner-channel loss makes it kill the owned macOS process group.
On Linux it independently kills the registered namespace-PID1 pidfd and the
outer helper process group. Normal finalization disarms and reaps the watchdog.
On Windows, helper/control EOF closes or terminates the non-breakaway Job and
waits for it. A Windows event-channel failure closes control and kills and
waits for the helper as a fail-safe.

`wait` supplies `retirement_timeout_ms`. A timeout returns `cleanup_failed`
without discarding the generation; a later `status` or `wait` can observe the
cached result.

`command` and `service` currently share the same supervision behavior.

On macOS, a new `terminate` request replaces any pending force timer for that
generation. The Unix runner relinquishes its process-group ownership and
cancels any timer before publishing `retired`. Further otherwise-supported
`interrupt` or `terminate` operations are then invalid even while proxy or
watchdog cleanup is still in progress; a platform-unsupported operation
remains unsupported.

## Final outcome

`final` keeps target, retirement, and infrastructure results separate:

```json
{
  "type": "final",
  "id": 9,
  "outcome": {
    "target": {"kind":"exited","code":1,"signal":null,"error":null},
    "retirement": {"complete":true,"forced":false,"error":null},
    "infrastructure": {"error":null,"cleanup_error":null}
  }
}
```

For an observed root outcome, target kinds are `exited`, `signaled`, and
`unknown`. Unix reports the native terminating signal where available. An
unknown native outcome may carry a bounded `error`; it does not replace the
separate infrastructure result. `target` is `null` when no trustworthy root
outcome could be observed; the infrastructure result explains why. On Unix,
only a valid private target-completion frame establishes the target outcome.
The launch bridge's own wait status is corroborating infrastructure state and
is never projected as the target result, including when forced retirement
terminates the bridge before it sends the frame. A target exit code of 1 is not
an infrastructure error. A valid root outcome followed by incomplete
descendant cleanup keeps the target result and reports retirement failure
separately. Proxy, watchdog, helper, or post-outcome cleanup failure belongs in
the infrastructure result.

Consumers must read `final` for the target result. The runner never projects
the target exit code or signal onto its own operating-system exit status. A
cleanly closed control loop exits 0; bootstrap, control, or runner
infrastructure failure exits 2. The runner status is therefore
infrastructure-only and cannot replace the structured target, retirement, and
cleanup outcome.

## Control-channel loss

Loss of the trusted public control channel while idle exits cleanly. After a
generation exists, the runner marks retirement forced, kills the owned macOS
group, Linux namespace-PID1 pidfd plus outer helper group, or Windows Job,
cancels any pending natural root-exit grace period, observes bounded retirement,
and shuts down proxy/helper resources. On Unix, forced observation uses
`force_timeout_ms` rather than waiting out `root_exit_grace_ms`. There is no
reconnect. The Unix watchdog also covers abrupt runner death; Windows relies on
helper/control EOF and Job kill-on-close ownership.

## Errors and fail-closed behavior

Errors contain `code`, `phase`, a redacted `message`, and `target_started`.
Codes are:

```text
malformed_frame        malformed_json
version_mismatch       invalid_state
invalid_request        invalid_path
unsupported_platform   unsupported_policy
backend_unavailable    companion_missing
setup_failed           launch_failed
control_failed         cleanup_failed
```

`companion_missing` means an exact required companion is absent, unusable, or
incompatible. `backend_unavailable` means the companion passed its identity
and compatibility checks but the concrete host could not provide another
required native primitive, including Linux pidfd or namespace preflight.

Phases are `protocol`, `discovery`, `setup`, `validation`, `proxy_startup`,
`sandbox_preparation`, `launch`, `running`, and `retirement`.

Every authority-affecting request is enforced or rejected before target start.
There is no unsandboxed fallback. The runner never silently widens
`platform_minimal`, drops a filesystem denial, ignores missing-path behavior,
bypasses a managed proxy, grants target writes to its state, changes full-tree
ownership into root-only ownership, performs lossy native conversion, searches
an arbitrary helper path, or loads normal Codex configuration.

## Forward compatibility

Protocol version 1 is exact in requests. An incompatible operation, policy, or
authority-model change requires a new protocol version. Additive descriptive
response fields do not require a new version and should be ignored by version
1 consumers. Unknown required behavior must never be ignored.
