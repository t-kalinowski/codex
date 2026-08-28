# Rolling-release maintenance

This crate is a downstream rolling patch over one exact stable Codex release. Refresh it by reimplementing the executable contract against the next release's current sandbox internals. Do not merge or cherry-pick the previous patch branch.

## Patch identity

- Upstream release tag: `rust-v0.150.1`
- Upstream base SHA: `90854393966b21e9ebfd21b122334eb09a20c93d`
- Codex workspace version: `0.150.1`
- Rust toolchain: `1.95.0`
- Protocol version: `1`
- Cargo package: `codex-mcp-console-sandbox`
- Executable: `mcp-console-sandbox`

The patch commit must be one logical commit directly above the upstream base and carry:

```text
MCP-Console-Patch-Base: 90854393966b21e9ebfd21b122334eb09a20c93d
MCP-Console-Sandbox-Protocol: 1
```

MCP Console must pin the final immutable patch commit, not this release tag or a mutable branch.

The runner build must stamp that final full 40-hex source revision. A Git checkout supplies `HEAD`; a source-archive build must set `STABLE_GIT_COMMIT`. There is no fallback to the upstream base or another placeholder revision. Bazel builds and tests must use `--config=mcp-console-sandbox`, which scopes Git-revision and workspace-version status stamping to the focused runner targets. Compilation fails when the final binary is not stamped with a full SHA.

## Public executable contract

The stable downstream surface is:

- the `mcp-console-sandbox` executable and its native bootstrap options;
- native target program and trailing arguments after `--`;
- the application-owned absolute state directory;
- a caller-created private native full-duplex control endpoint;
- repeatable bootstrap stream endpoints whose values must exactly match the launch request's `passed_handle` set;
- 1 MiB, four-byte big-endian length-prefixed JSON frames;
- protocol version 1 request, response, capability, policy, lifecycle, error, and outcome shapes in [PROTOCOL.md](PROTOCOL.md);
- the same-revision companion layout in [README.md](README.md).

The contract contains no Codex application, model, authentication, approval, session, conversation, MCP-server, or configuration concepts. Rust crate APIs, the Unix exec-status pipe, the Unix watchdog channel, and the Windows helper wire format are private implementation details.

`launch_accepted.root_process_id` is informational and backend-scoped. Linux returns `null` because the target PID is scoped to bubblewrap's PID namespace. macOS and Windows return a host-visible target PID. Lifecycle control always uses protocol operations.

## Files outside the leaf crate

Reconcile this inventory against `git diff --name-only` before the final commit. Every listed change is either workspace metadata or a narrow seam in a crate that already owns the native behavior.

| File                                                                   | Reason                                                                                                                                                                                                                                                                                                                                                              |
| ---------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `.bazelrc`                                                             | Add an opt-in runner-only configuration that stamps the exact Git revision through Bazel workspace status.                                                                                                                                                                                                                                                          |
| `.github/scripts/verify_cargo_workspace_manifests.py`                  | Record the rolling patch's temporary, exact Cargo feature exceptions. Matching explicit telemetry-free Bazel variants cover every excepted edge; unused exceptions fail the verifier.                                                                                                                                                                             |
| `.github/workflows/mcp-console-sandbox.yml`                            | Run the executable contract suite, focused modified-sandbox-crate tests, configured Bazel contract target, and complete release runner-plus-companion smoke tests on native Linux, macOS, and Windows hosts; serialize native Windows tests around the standalone policy lease and skip only two confirmed unchanged base Seatbelt path-spelling failures on macOS. |
| `defs.bzl`                                                             | Allow one focused crate target to replace generated local-crate dependencies, supply private build and compiler environments, stamp its library, and keep binary-local compile data and flags while leaving ordinary target behavior unchanged.                                                                                                                     |
| `codex-rs/Cargo.toml`                                                  | Register the private leaf package as a workspace member.                                                                                                                                                                                                                                                                                                            |
| `codex-rs/Cargo.lock`                                                  | Lock the leaf package and its low-level dependency edges. Cargo also normalizes stale `0.0.0` versions for 142 existing workspace path packages to the workspace version `0.150.1`; no registry dependency version changes as a result.                                                                                                                             |
| `codex-rs/bwrap/BUILD.bazel`                                           | Stamp the packaged companion's closed compatibility token with the workspace release version.                                                                                                                                                                                                                                                                      |
| `codex-rs/bwrap/src/main.rs`                                           | Answer the closed, side-effect-free private companion ABI v2 query before entering bubblewrap.                                                                                                                                                                                                                                                                      |
| `codex-rs/linux-sandbox/BUILD.bazel`                                   | Add a private implementation-identical library edge that selects feature-free sandboxing and retains the packaged-bubblewrap digest stamp.                                                                                                                                                                                                                          |
| `codex-rs/linux-sandbox/Cargo.toml`                                    | Disable the sandboxing crate's Windows telemetry default on this low-level edge.                                                                                                                                                                                                                                                                                    |
| `codex-rs/linux-sandbox/src/bundled_bwrap.rs`                          | Select, reopen, digest-check, optionally run the private embedding compatibility check, and execute one exact packaged bubblewrap while preserving native argv.                                                                                                                                                                                                     |
| `codex-rs/linux-sandbox/src/bwrap.rs`                                  | Preserve native target arguments through bubblewrap command construction.                                                                                                                                                                                                                                                                                           |
| `codex-rs/linux-sandbox/src/embedding.rs`                              | New closed embedding facade for runner-relative bubblewrap and state-directory selection.                                                                                                                                                                                                                                                                           |
| `codex-rs/linux-sandbox/src/embedding_tests.rs`                        | Prove exact layout validation and rejection of incidental `PATH` resources.                                                                                                                                                                                                                                                                                         |
| `codex-rs/linux-sandbox/src/exec_util.rs`                              | Convert native Unix arguments to `CString` without UTF-8 loss.                                                                                                                                                                                                                                                                                                      |
| `codex-rs/linux-sandbox/src/launcher.rs`                               | Prefer an explicitly activated packaged launcher over legacy installation discovery, retain native argv, request monitor-stream release, and add final-launch-only bubblewrap process reporting for the packaged embedding path.                                                                                                                                     |
| `codex-rs/linux-sandbox/src/lib.rs`                                    | Export the narrow embedding facade and register its tests.                                                                                                                                                                                                                                                                                                          |
| `codex-rs/linux-sandbox/src/linux_run_main.rs`                         | Accept paired embedding arguments, use application state for synthetic mounts, preserve native helper/final exec argv, keep the process-info channel out of `/proc` preflight, and release the embedding cleanup parent's target-stream and info-channel copies.                                                                                                     |
| `codex-rs/linux-sandbox/src/linux_run_main_tests.rs`                   | Cover paired embedding input and native command parsing.                                                                                                                                                                                                                                                                                                            |
| `codex-rs/linux-sandbox/src/proxy_routing.rs`                          | Read only known managed-proxy values as Unicode so unrelated native target environment entries remain lossless.                                                                                                                                                                                                                                                     |
| `codex-rs/linux-sandbox/tests/suite/managed_proxy.rs`                  | Prove ordinary Codex packaged-bubblewrap fallback retains digest-only execution and does not acquire the runner's private compatibility-v2 requirement.                                                                                                                                                                                                             |
| `codex-rs/sandboxing/BUILD.bazel`                                      | Add a private feature-free sandboxing library that points at the no-telemetry Windows variant.                                                                                                                                                                                                                                                                      |
| `codex-rs/sandboxing/Cargo.toml`                                       | Make Windows telemetry an explicit default feature while keeping the underlying Windows edge feature-free for private consumers.                                                                                                                                                                                                                                    |
| `codex-rs/sandboxing/src/landlock.rs`                                  | Add a native-argv Linux helper constructor while retaining the existing String wrapper.                                                                                                                                                                                                                                                                             |
| `codex-rs/sandboxing/src/landlock_tests.rs`                            | Cover non-UTF-8 native Linux arguments.                                                                                                                                                                                                                                                                                                                             |
| `codex-rs/sandboxing/src/seatbelt.rs`                                  | Add a closed macOS terminal policy and the ordered Seatbelt rules for denying pre-existing terminal reopen.                                                                                                                                                                                                                                                         |
| `codex-rs/sandboxing/src/seatbelt_tests.rs`                            | Cover unchanged default policy, opt-in terminal isolation, and rule ordering.                                                                                                                                                                                                                                                                                       |
| `codex-rs/utils/pty/src/win/job.rs`                                    | Add a strict suspended-child assignment and resume seam for synchronous compatibility helpers; assignment failure terminates rather than resuming outside the non-breakaway Job.                                                                                                                                                                                    |
| `codex-rs/vendor/bubblewrap/bubblewrap.c`                              | Add one private packaged-only PID-1 option that releases the host monitor's target-stream copies after namespace setup.                                                                                                                                                                                                                                             |
| `codex-rs/windows-sandbox-rs/Cargo.toml`                               | Make telemetry an opt-out feature so private runner artifacts can omit `codex-otel` while existing callers retain their default graph.                                                                                                                                                                                                                              |
| `codex-rs/windows-sandbox-rs/BUILD.bazel`                              | Keep the normal Bazel library and helpers telemetry-enabled, and expose one feature-free private library stamped with the workspace version plus source and manifest inputs for private helpers.                                                                                                                                                                    |
| `codex-rs/windows-sandbox-rs/no-telemetry/BUILD.bazel`                 | Build the two exact private Windows companion executables against the feature-free library without changing normal helper targets.                                                                                                                                                                                                                                  |
| `codex-rs/windows-sandbox-rs/src/bin/command_runner/main.rs`           | Answer the closed command-runner compatibility query before dispatching the exact private standalone helper invocation or existing command-runner path.                                                                                                                                                                                                             |
| `codex-rs/windows-sandbox-rs/src/bin/setup_main/main.rs`               | Answer the closed setup-helper compatibility query before normal setup initialization.                                                                                                                                                                                                                                                                              |
| `codex-rs/windows-sandbox-rs/src/bin/setup_main/win.rs`                | Use the crate-local wire-compatible metrics shape and the requested closed policy namespace so the setup binary builds without telemetry and prepares the correct principals.                                                                                                                                                                                       |
| `codex-rs/windows-sandbox-rs/src/bin/setup_main/win/firewall.rs`       | Select namespace-specific firewall names, require every active profile to enable Windows Firewall, and configure and verify the complete closed COM rule scope before reporting standalone setup ready.                                                                                                                                                              |
| `codex-rs/windows-sandbox-rs/src/bin/setup_main/win/read_acl_mutex.rs` | Select a namespace-specific read-ACL mutex.                                                                                                                                                                                                                                                                                                                         |
| `codex-rs/windows-sandbox-rs/src/bin/setup_main/win/sandbox_users.rs`  | Provision the exact identities and group selected by the closed policy namespace.                                                                                                                                                                                                                                                                                   |
| `codex-rs/windows-sandbox-rs/src/elevated/runner_client.rs`            | Share the existing bounded named-pipe connection helper within the owning crate.                                                                                                                                                                                                                                                                                    |
| `codex-rs/windows-sandbox-rs/src/identity.rs`                          | Inspect prepared identities from an explicit application state directory and require the expected policy namespace.                                                                                                                                                                                                                                                 |
| `codex-rs/windows-sandbox-rs/src/lib.rs`                               | Export the standalone facade, helper dispatch, telemetry-neutral setup shape, and closed internal policy namespace.                                                                                                                                                                                                                                                 |
| `codex-rs/windows-sandbox-rs/src/process.rs`                           | Add native UTF-16 command/environment creation, direct handles, redacted errors, non-breakaway Job selection, and explicit suspended start.                                                                                                                                                                                                                         |
| `codex-rs/windows-sandbox-rs/src/policy_lease.rs`                      | Coordinate the fixed standalone identities and firewall state with one MCP Console-specific non-inheritable machine-global lease and fail closed while an abandoned generation still owns its setup Job.                                                                                                                                                            |
| `codex-rs/windows-sandbox-rs/src/policy_namespace.rs`                  | Define the closed ordinary-Codex and standalone-MCP-Console identity, group, and mutex namespaces while preserving ordinary defaults.                                                                                                                                                                                                                               |
| `codex-rs/windows-sandbox-rs/src/policy_namespace_tests.rs`            | Prove the two principal and mutex namespaces are exact and disjoint.                                                                                                                                                                                                                                                                                                |
| `codex-rs/windows-sandbox-rs/src/setup.rs`                             | Accept exact setup helpers, explicit state, and a backward-compatible closed policy namespace; contain standalone setup in the named Job; and invoke the read-only firewall verifier with bounded, environment-cleared diagnostics.                                                                                                                                  |
| `codex-rs/windows-sandbox-rs/src/setup_containment.rs`                 | Own each standalone setup helper tree in a fixed global kill-on-close Job and retire it before setup returns or abandoned policy ownership can recover.                                                                                                                                                                                                             |
| `codex-rs/windows-sandbox-rs/src/setup_containment_tests.rs`           | Deterministically prove owner-loss retirement and fail-closed abandoned-lease recovery while an old Job owner remains.                                                                                                                                                                                                                                             |
| `codex-rs/windows-sandbox-rs/src/setup_error.rs`                       | Classify setup containment and exact-verification failures without changing existing setup error behavior.                                                                                                                                                                                                                                                         |
| `codex-rs/windows-sandbox-rs/src/standalone.rs`                        | New public low-level standalone types and facade exports.                                                                                                                                                                                                                                                                                                           |
| `codex-rs/windows-sandbox-rs/src/standalone/client.rs`                 | New exact-helper launch client, direct-handle duplication, two-phase ready/commit handshake, observation, fail-safe helper retirement, and standalone policy lease ownership.                                                                                                                                                                                       |
| `codex-rs/windows-sandbox-rs/src/standalone/compatibility.rs`          | Query both exact helpers through one bounded, environment-cleared, side-effect-free private ABI and Codex-release gate while a non-breakaway Job owns each complete query tree.                                                                                                                                                                                     |
| `codex-rs/windows-sandbox-rs/src/standalone/helper.rs`                 | New capability-restricted target helper with suspended pre-commit creation, non-breakaway Job ownership, limited rights against infrastructure processes, and separate root/final outcomes.                                                                                                                                                                         |
| `codex-rs/windows-sandbox-rs/src/standalone/helper_tests.rs`           | Deterministically cover pre-commit control loss and explicit/control-loss Job termination failure.                                                                                                                                                                                                                                                                  |
| `codex-rs/windows-sandbox-rs/src/standalone/setup.rs`                  | New read-only status, idempotent prepare/refresh, exact-resource, ACL, identity, and firewall adapter fixed to the MCP Console namespace.                                                                                                                                                                                                                           |
| `codex-rs/windows-sandbox-rs/src/standalone/wire.rs`                   | New bounded, versioned private helper framing with native UTF-16 values and closed ready/commit messages.                                                                                                                                                                                                                                                           |
| `codex-rs/windows-sandbox-rs/src/standalone_tests.rs`                  | Cover native values, setup semantics, handles, helper protocol, job lifetime, and control-failure behavior.                                                                                                                                                                                                                                                         |
| `codex-rs/windows-sandbox-rs/src/token.rs`                             | Add the standalone restricted-token constructor whose write-restricting SIDs are exactly the active filesystem capabilities plus optional managed-network identity. Existing Codex constructors are unchanged.                                                                                                                                                     |
| `codex-rs/windows-sandbox-rs/src/token_tests.rs`                       | Verify the exact standalone restricting SID set and retain coverage of the existing token shapes.                                                                                                                                                                                                                                                                   |
| `codex-rs/windows-sandbox-rs/src/wfp.rs`                               | Preserve the ordinary WFP wrapper and add namespace-specific filter selection for standalone setup.                                                                                                                                                                                                                                                                 |
| `codex-rs/windows-sandbox-rs/src/wfp/filter_specs.rs`                  | Define exact, disjoint WFP filter display names and filter keys for ordinary Codex and standalone MCP Console. Windows Firewall rule names remain in `setup_main/win/firewall.rs`.                                                                                                                                                                                  |
| `codex-rs/windows-sandbox-rs/src/wfp_setup.rs`                         | Use the telemetry-neutral metrics alias while preserving existing telemetry behavior.                                                                                                                                                                                                                                                                               |
| `codex-rs/windows-sandbox-rs/src/wfp_setup_no_telemetry.rs`            | New no-op metrics adapter for runner/helper builds with default features disabled.                                                                                                                                                                                                                                                                                  |
| `codex-rs/windows-sandbox-rs/src/winutil.rs`                           | Quote native Windows arguments without UTF-8 conversion.                                                                                                                                                                                                                                                                                                            |

`MODULE.bazel.lock` must be regenerated with `just bazel-lock-update` after the Cargo graph settles and must be included if it changes. The crate universe reads `codex-rs/Cargo.toml` and `codex-rs/Cargo.lock`; no separate workspace manifest allowlist is needed. The leaf crate has its own `BUILD.bazel`.

Cargo selects `default-features = false` on the runner's Windows and sandboxing edges. The workspace-manifest verifier records only those exact edges, the two matching feature definitions, and the optional telemetry dependency as temporary rolling-patch exceptions; its unused-exception check keeps the list exact. Bazel keeps the canonical Windows target on `telemetry` and uses explicit private variants for the runner, sandboxing, Linux sandbox, Windows sandbox, and Windows helpers. The focused Bazel dependency closure has no path to `codex-otel`, `codex-api`, or `codex-client`; the private variants do not alter canonical target labels or existing callers.

No existing Codex caller depends on the leaf crate or is routed through it. The Seatbelt terminal extension defaults to the previous policy, the existing Linux discovery path remains available outside embedding mode, and Windows telemetry remains a default feature for existing builds.

## Release-local seams

The leaf runner uses these internal APIs:

- `codex_protocol::permissions` for the normalized filesystem policy, specificity, and access precedence;
- `codex_sandboxing::seatbelt` for macOS policy construction and the closed terminal-isolation enum;
- `codex_sandboxing::landlock` for native Linux helper argv construction;
- `codex_linux_sandbox::{prepare_packaged_bwrap, run_main}` for deterministic packaging and focused helper self-dispatch;
- `codex_network_proxy::{NetworkProxy, NetworkProxyState}` and its managed sandbox context for proxy ownership, target environment, and direct-egress confinement;
- `codex_windows_sandbox` standalone setup, native command, direct stream, restricted identity, ACL/WFP, private desktop, and Job APIs;
- `codex-utils-pty` process-group and parent-death primitives on Unix.

The rolling patch may adapt these seams again when upstream internals change. It must not copy their platform implementations into the leaf crate.

## Platform assumptions

### macOS

The runner requires `/usr/bin/sandbox-exec` and uses Codex's current Seatbelt translator. It adds the private launch bridge as a native command suffix, not as a shell string. `terminal=preserve` creates the supervised process group without leaving the controlling-terminal session. `terminal=isolate_host_devices` starts a new session and selects the typed `DenyPreexistingReopen` policy: reopening `/dev/tty` and existing `/dev/ttys*` is denied, sandbox-created PTYs are reallowed, and inherited descriptors remain usable.

The runner owns one process group, uses Codex's member-fallback signal helpers, and has a private kill-on-owner-loss watchdog. It reports `full_tree_retirement=false` because a descendant can deliberately create a new session. The in-sandbox bridge waits at a private launch gate until the watchdog is ready; watchdog failure or runner loss before gate release cannot start the native target. After release, a target-exec shim reports ready while the target application remains blocked on a second commit gate. The outer runner installs supervision and releases stream copies before commit, then confirms native exec through a close-on-exec channel. Uncertainty after the first release consumes the generation. Explicit termination or control loss wakes retirement out of any pending natural root-exit grace; control loss immediately kills the group and observes it for `force_timeout_ms`.

### Linux

The private package layout is exactly:

```text
mcp-console-sandbox
codex-resources/bwrap
```

Embedding activation validates the canonical layout and executable, selects it ahead of system or installation discovery, and passes the canonical state directory. The helper uses `<state-dir>/bwrap-synthetic-mount-registry`. The runner self-dispatches as `codex-linux-sandbox`, then uses Codex's current bubblewrap user/PID/IPC namespaces, PID 1, `--new-session`, `--die-with-parent`, capability drop, synthetic mounts, `/proc`, network namespace, seccomp, and proxy bridge. Embedding mode disables the legacy Landlock fallback. A private `--embedding-bwrap-info-fd` seam adds bubblewrap's `--info-fd` only to the final sandbox launch, not the `/proc` preflight, so the runner receives the host PID for the namespace PID 1 without changing existing callers.

Bazel supplies `CODEX_BWRAP_SHA256`, and the helper reopens and executes the verified file through `/proc/self/fd`. Before discovery or launch succeeds, both build paths execute the exact companion with a two-second, 1,024-byte private query and require the exact protocol-2 and workspace-release token, zero exit, empty stderr, and no extra output. Plain Cargo builds do not embed the additional digest; their compatibility and identity boundary is the closed token plus the immutable private layout.

Linux preserves non-UTF-8 native target arguments end to end. JSON working, policy, and Unix-socket paths remain Unicode. Protocol version 1 also requires valid-Unicode runner, companion-resource, and application-state paths. Before either target launch gate is released, the runner opens a pidfd for bubblewrap's reported namespace-PID1 host PID and registers it with an acknowledged watchdog command. Forced and natural retirement jointly observe that pidfd and the outer helper. Forced retirement independently signals both, so failure in one path cannot suppress the other. The PID namespace, pidfd, and Unix process group support the full-tree claim; Linux also sets a parent-death signal and uses the private owner-loss watchdog.

After committed native exec is confirmed, the resident namespace PID 1, synthetic-mount cleanup parent, and packaged bubblewrap host monitor replace their copies of target stdin, stdout, and stderr with `/dev/null`. This keeps the infrastructure processes resident for supervision and cleanup without delaying target pipe or PTY EOF. The monitor option is private, accepted only with bubblewrap's PID-1 mode, and selected only for the exact packaged embedding path; existing Linux sandbox callers retain their previous stream ownership.

Bubblewrap's isolated session prevents the outer runner from projecting SIGINT or SIGTERM to the native target. Linux therefore reports interrupt and graceful termination unsupported, requires both graceful deadlines to be zero, and uses forced retirement. After the bridge reports root exit, its PID 1 remains alive to reap namespace descendants. Descendants may run for `root_exit_grace_ms`; remaining namespace processes are then force-retired through the namespace-PID1 pidfd and outer helper group and reported with `retirement.forced=true`.

### Windows

The private layout is exactly:

```text
mcp-console-sandbox.exe
codex-resources/codex-windows-sandbox-setup.exe
codex-resources/codex-command-runner.exe
```

All paths must be absolute local-disk paths and both helpers must have their exact names in the same directory. The standalone facade never consults `PATH`. It uses the caller's state directory wherever existing Windows code expects a Codex home.

Before operational capability discovery, setup, or launch, the runner executes both exact helper paths with distinct compatibility switches. Each helper must exit successfully within two seconds, emit no stderr, and return its exact private ABI-1 and workspace-release token on stdout within 1,024 bytes. Query dispatch precedes setup and command-runner initialization, uses an empty environment and null stdin, and cannot mutate setup state or request UAC. A wrong, stale, noisy, slow, or same-name helper rejects the Windows backend before target start. The private helper ABI is independent of public protocol version 1. Cargo obtains the release component from the workspace package version; the focused Bazel workspace-status command reads that same value from `codex-rs/Cargo.toml` and stamps it into the private feature-free library.

Setup status is read-only and cannot display UAC. It verifies the installed fixed standalone firewall rules under a short global lease instead of trusting only a state-directory marker. `prepare` is idempotent and is the only UAC-capable operation. `refresh` requires ready standalone identities and a matching global firewall generation, is idempotent, and never requests UAC. Launch verifies that same generation under its lifetime lease and then refreshes ACL state before target creation. The online identity implements unrestricted networking; the offline identity carries WFP denied/managed confinement, proxy ports, local-binding policy, and an optional proxy restricting SID.

The fixed MCP Console identities, group, firewall names, WFP keys, and mutexes are disjoint from ordinary Codex policy objects. They permit one active standalone policy generation machine-wide. Standalone launch acquires its own lifetime lease, verifies the exact outbound, loopback, port-complement, and stale-rule state through the setup implementation, refreshes ACLs, rechecks local setup readiness, and holds the lease through Job retirement and managed-proxy cleanup. Busy or abandoned acquisition fails before target start; abandonment requires setup `prepare`. Ordinary Codex setup retains its existing identifiers and behavior.

The application state directory contains setup markers, protected identity credentials, helper scratch files, and diagnostics. The identities and WFP rules are system state, while filesystem ACLs apply to policy-selected host paths. Refresh reapplies path ACLs but does not rewrite WFP state.

The exact command helper receives native UTF-16 target arguments and values, duplicates ordinary inherited or caller-passed handles directly, and creates the target suspended in a non-breakaway kill-on-close Job. It reports `Ready` over a separate private bounded pipe. The outer runner installs supervision and releases its stream copies before sending `CommitLaunch`; only then does the helper resume the target and return `Committed`. Any uncertainty after the `Spawn` request may have been sent consumes the generation. Root-exit and final events remain separate. Descendants receive a natural retirement grace period, then the helper force-terminates and observes the Job. Helper input EOF, control failure, owner drop, and runner failure retire the Job, including while the target is still suspended before commit.

Windows does not claim ConPTY, terminal isolation, interrupt projection, or a graceful signal. Both launch `terminate_grace_ms` and terminate-operation `graceful_ms` must be zero. `private_desktop` is the only Windows platform extension. Companion compatibility uses the trusted immutable directory, exact filenames, and closed helper-specific private ABI and Codex-release tokens; there is no separate same-binary digest stamp.

## Managed proxy seams

Version 1 uses the existing `codex-network-proxy` listener, domain policy, redirect, HTTP, optional SOCKS, trusted upstream, target environment, native sandbox context, and shutdown behavior. The runner owns listener selection and lifetime. Managed launch always combines the proxy with restricted native networking; it cannot fall back to unrestricted or fully denied mode.

The clean exported surface is full/limited access, domain allow and deny patterns, HTTP, SOCKS, trusted upstream use, and the backend's supported loopback/local-binding pair. macOS also accepts typed Unix-socket allow rules. Protocol version 1 rejects Unix-socket deny rules and duplicate socket paths; capabilities report allow and deny support independently. SOCKS UDP, explicit local-port exceptions, managed CA/TLS interception, non-loopback listeners, credential brokerage, secret/header injection, approval hooks, and interactive elicitation remain unsupported.

Protocol version 1 accepts exact-host patterns, `*.example.com` for subdomains only, and `**.example.com` for the apex plus subdomains. The leaf runner rejects other glob forms before starting the proxy. Denial wins when the same pattern appears in both lists. On macOS with managed SOCKS enabled, the runner preserves a caller-provided native `GIT_SSH_COMMAND`, including a non-Unicode value, and injects Codex's managed fallback only when that variable is absent.

## Capability and validation gaps

- Plain Cargo Linux artifacts verify the closed companion ABI but have no same-binary bubblewrap digest stamp.
- Windows helper compatibility verifies closed helper-specific private ABI and Codex-release tokens from the immutable layout, but has no separate same-binary digest stamp.
- macOS owns a process group but cannot claim descendants that deliberately leave it, so it reports `process_tree_supervision=false` and `full_tree_retirement=false`.
- No backend accepts Unix-socket deny rules in protocol version 1. macOS supports allowlist entries; Linux and Windows support neither authority.
- Linux cannot configure loopback/local-binding behavior beyond the required `allow`/`true` pair and cannot enforce typed Unix-socket policy or host terminal-device isolation.
- Linux cannot project interrupt or graceful termination through bubblewrap's isolated session; launch and operation graceful deadlines must be zero.
- Windows has no interrupt or graceful-signal projection, ConPTY-specific behavior, Unix-socket policy, or terminal-device isolation.
- No backend supports SOCKS UDP, explicit local-port exceptions, managed CA or TLS interception, non-loopback listeners, credentials, secret/header injection, approval hooks, or interactive elicitation in protocol version 1.
- Native Linux executable and sandbox tests ran in a native Linux container. Native Windows executable tests require the rolling-patch workflow's Windows host.

Do not remove a gap merely because a low-level API exists. Remove it only after the public executable reports, rejects, or enforces the behavior as documented and its black-box contract test passes.

## Validation record

Validation completed during implementation includes:

- deterministic Unix preparation-gate and commit-gate tests proving that pre-commit control loss cannot execute target application code;
- a public Unix executable test proving that post-commit native `exec` failure consumes the generation and remains observable as infrastructure failure;
- a public Windows executable Ready-loss fault test cross-compiled through the Windows-GNU contract target, proving the intended native test path consumes the generation without executing target application code;
- public Windows executable regressions that replace either exact helper with the other valid same-release helper, require unavailable operational capabilities and pre-start launch rejection, and are cross-compiled through the Windows-GNU contract target for native Windows CI;
- checked-in public Windows executable regressions for bootstrap standard-handle replacement and cross-state-directory firewall-generation drift; the Windows-GNU contract target compiles both, while behavioral execution remains part of native Windows CI;
- the native macOS package suite: 73 of 73 tests passed, with no skips or nextest leak classifications;
- the native Linux runner suite: 76 of 76 tests passed, with no skips, retries, nextest leak classifications, or remaining runner/bubblewrap processes;
- the native Linux sandbox suite: 157 of 157 tests passed, with no skips or retries;
- the packaged bubblewrap crate has no Rust tests; `just test -p codex-bwrap --no-tests=pass` and its all-target compilation completed successfully, while the workflow's executable contract and release smoke tests exercise the packaged helper;
- the native macOS `codex-utils-pty` suite: 29 of 29 tests passed; the native Windows workflow runs the same focused crate suite for the changed Job seam;
- strict combined Clippy for the runner, Linux sandbox, and packaged bubblewrap;
- the native macOS sandbox suite: 94 tests passed and the two unchanged base Seatbelt `/var` versus `/private/var` path-spelling tests failed; both changed terminal-reopen and native-argument regressions passed individually, and the native workflow skips exactly those two unchanged base failures;
- the Windows low-level crate's 12 host tests with both default and disabled default features;
- Windows-GNU Clippy with both feature configurations and a feature-free library build;
- Windows-GNU runner test compilation and strict Clippy of the Windows executable contract target;
- `actionlint .github/workflows/mcp-console-sandbox.yml` after configuring the native Windows suite with nextest `--test-threads 1`, which serializes test processes that acquire the standalone MCP Console policy lease;
- Windows-GNULLVM Bazel builds of the focused runner and both feature-free helper targets, with both helper artifacts carrying the workspace's `0.150.1` release component rather than rules_rust's default `0.0.0` package version;
- an unconfigured `bazel build //codex-rs/mcp-console-sandbox:mcp-console-sandbox` failed at compile time because the final binary lacked a full source-revision stamp;
- `bazel build --config=mcp-console-sandbox //codex-rs/mcp-console-sandbox:mcp-console-sandbox` succeeded, and an external discovery request reported the exact output of `git rev-parse HEAD`;
- Bazel dependency queries proving that the configured focused target has no path to `codex-otel`, `codex-api`, `codex-client`, or `codex-core`;
- `just fmt` and `git diff --check`.

Native Windows execution was unavailable on this macOS host. The checked-in workflow runs the executable suite and focused tests for every modified native sandbox crate on Ubuntu 24.04, macOS 14, and Windows Server 2022 hosts.

The release-gate command set is:

```console
cd codex-rs
just test -p codex-mcp-console-sandbox
just test -p codex-sandboxing
just test -p codex-linux-sandbox
just test -p codex-windows-sandbox
just test -p codex-bwrap --no-tests=pass
just test -p codex-utils-pty
cargo clippy -p codex-mcp-console-sandbox --all-targets --all-features -- -D warnings
cargo build --locked -p codex-mcp-console-sandbox --bin mcp-console-sandbox --release
just fix -p codex-mcp-console-sandbox
just fix -p codex-linux-sandbox
just fix -p codex-sandboxing
just fix -p codex-windows-sandbox
just fix -p codex-bwrap
just fmt
cargo fmt --all --check
```

The final local package rerun used:

```console
cd codex-rs
just test -p codex-mcp-console-sandbox --status-level all --final-status-level all
```

The Windows-GNU test and feature-free helper checks used local Zig compiler wrapper scripts at the paths shown here; the wrappers are not repository artifacts:

```console
cd codex-rs
CC_x86_64_pc_windows_gnu=/tmp/mcp-console-zigcc \
CXX_x86_64_pc_windows_gnu=/tmp/mcp-console-zigcxx \
AR_x86_64_pc_windows_gnu=/tmp/mcp-console-zigar \
cargo check \
  --locked \
  -p codex-mcp-console-sandbox \
  --tests \
  --bins \
  --target x86_64-pc-windows-gnu

CC_x86_64_pc_windows_gnu=/tmp/mcp-console-zigcc \
CXX_x86_64_pc_windows_gnu=/tmp/mcp-console-zigcxx \
AR_x86_64_pc_windows_gnu=/tmp/mcp-console-zigar \
cargo check \
  --locked \
  --target x86_64-pc-windows-gnu \
  -p codex-windows-sandbox \
  --no-default-features \
  --all-targets

CC_x86_64_pc_windows_gnu=/tmp/mcp-console-zigcc \
CXX_x86_64_pc_windows_gnu=/tmp/mcp-console-zigcxx \
AR_x86_64_pc_windows_gnu=/tmp/mcp-console-zigar \
cargo clippy \
  --locked \
  --target x86_64-pc-windows-gnu \
  -p codex-windows-sandbox \
  --all-targets \
  -- \
  -D warnings

CC_x86_64_pc_windows_gnu=/tmp/mcp-console-zigcc \
CXX_x86_64_pc_windows_gnu=/tmp/mcp-console-zigcxx \
AR_x86_64_pc_windows_gnu=/tmp/mcp-console-zigar \
cargo clippy \
  --locked \
  --target x86_64-pc-windows-gnu \
  -p codex-windows-sandbox \
  --no-default-features \
  --all-targets \
  -- \
  -D warnings

CC_x86_64_pc_windows_gnu=/tmp/mcp-console-zigcc \
CXX_x86_64_pc_windows_gnu=/tmp/mcp-console-zigcxx \
AR_x86_64_pc_windows_gnu=/tmp/mcp-console-zigar \
cargo clippy \
  --locked \
  --target x86_64-pc-windows-gnu \
  -p codex-mcp-console-sandbox \
  --all-targets \
  --all-features \
  -- \
  -D warnings
```

The repository's Bazel metadata and target checks are:

```console
just bazel-lock-update
just bazel-lock-check
bazel test \
  --config=mcp-console-sandbox \
  //codex-rs/mcp-console-sandbox:mcp-console-sandbox-lib-contract-test
bazel build \
  --config=mcp-console-sandbox \
  --platforms=//:windows_x86_64_gnullvm \
  //codex-rs/mcp-console-sandbox:mcp-console-sandbox \
  //codex-rs/windows-sandbox-rs/no-telemetry:codex-command-runner \
  //codex-rs/windows-sandbox-rs/no-telemetry:codex-windows-sandbox-setup
bazel query \
  'somepath(//codex-rs/mcp-console-sandbox:mcp-console-sandbox, //codex-rs/otel)'
```

The final release review also checks `git diff --check`, the complete changed-file list, diffstat, runner dependency graph, release build, and native workflow result for every supported platform.

## Refresh procedure

1. Start a fresh branch from the next exact stable Codex release.
2. Inspect the previous runner's protocol, tests, README, and REBASE notes.
3. Inspect the new release's current sandbox implementations.
4. Reimplement the same external contract against the new internals.
5. Do not merge or cherry-pick the old patch branch.
6. Preserve protocol version 1 unless the downstream contract intentionally changes.
7. Run native tests on all supported platforms.
8. Create one logical commit above the new release base.

Before each refresh is committed, update the release tag, base SHA, workspace version, toolchain, companion assumptions, internal seams, complete file inventory, capability gaps, and validation record in this file.
