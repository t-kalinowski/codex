# Standalone lifecycle contract

The executable owns one sandbox lifetime. Its single application supervisor owns native launch, descendant retirement, optional private storage, and the optional upstream network proxy. Linux's native namespace init and bubblewrap helpers remain kernel setup machinery. macOS's short native stage applies Seatbelt and execs the target in the same process. There is no application manager, manager monitor, watchdog, or mutual recovery protocol.

## Configuration

Both [input modes](PROTOCOL.md) accept this optional object:

```json
{
  "lifecycle": {
    "parent_pid": 12345,
    "sigterm": "retire",
    "private_tmp": {
      "parent": "/absolute/parent",
      "environment": ["TMPDIR", "TMP", "TEMP"]
    },
    "cleanup_timeout_ms": 1000
  }
}
```

| Field                | Default and behavior                                                                                                                                                                                                                                                                                                             |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `parent_pid`         | Absent/null disables caller-death retirement. A supplied PID must be the live direct parent at capture. PID reuse cannot substitute another parent. Once captured, its death requests retirement and exit 0. An already-invalid parent is a launch error.                                                                        |
| `sigterm`            | `"forward"` forwards SIGTERM according to inherited signal state. `"retire"` makes SIGTERM request retirement and exit 0, including when SIGTERM was inherited ignored or blocked.                                                                                                                                               |
| `private_tmp`        | Absent/null creates no private directory. Otherwise the supervisor creates a mode-0700 container and writable `data` child. Each explicitly named environment variable receives the absolute data path. The optional absolute `parent` defaults to the supervisor's native temporary directory. An empty export list is allowed. |
| `cleanup_timeout_ms` | Absent/null uses 1000 ms. Values 1–60000 bound descendant retirement after completion or cancellation; this is not a target execution timeout or a filesystem deletion deadline.                                                                                                                                                 |

Unknown lifecycle fields and invalid names/values fail before target execution. No application policy defaults are inferred. The supervisor reads no policy files and exposes no reload or mutation interface.

## Sequencing and observable behavior

Signals are captured before runtime startup. After request acceptance the supervisor validates the configured parent, creates private storage, prepares the sandbox/proxy, spawns the native stage, and establishes descendant observation before releasing its private gate. Asynchronous preparation also observes cancellation. The native stage closes its gate before target code, including target loader constructors, can execute. Target loader and private-directory environment variables are applied at this final boundary; helpers do not use the target's private TMPDIR for their own mount bookkeeping.

The supervisor forwards HUP, INT, QUIT, and optionally TERM. Inherited ignored or blocked signals remain ignored or blocked in the target, including SIGCHLD and the platform's highest catchable signal. The supervisor maintains independently waitable children. The final native pre-exec hook restores target dispositions and the complete original mask after the standard subprocess library's resets. No application signal wrapper is needed.

A completed native root's status takes precedence over pending cancellation. On normal/nonzero exit, interruption, or configured retirement, the supervisor retires descendants, restores terminal ownership, stops the proxy, and removes private storage. Only then does it return the target's exit code, `128 + signal`, or the configured retirement status. Discovery, termination, timeout, terminal restoration, proxy, and deletion errors produce a nonzero result and a stderr diagnostic. A discovery/termination failure remains an error even if a later pass finds no process; private storage is retained when retirement cannot be established. There is no successful-cleanup acknowledgment or preserve-marker escape hatch.

An incomplete framed request is cancellable without transport EOF. Before that request is accepted, no lifecycle options are available and cancellation is a startup error. Target stdin is never parsed, copied, or relayed. Its original open file description, binary bytes, offset, seekability, and terminal identity are preserved. Waiting processes release their stdin copies so target-side closure is visible immediately. Stdout/stderr carry target bytes directly, with supervisor errors on stderr.

macOS transfers an exclusively owned foreground terminal to the target group and restores it after retirement. If the caller group has a peer, that group retains ownership and the supervisor relays terminal signals. Linux retains the caller's foreground ownership while bubblewrap creates the target session; inherited terminal descriptors remain usable and terminal INT is relayed through the namespace init.

## Boundaries and intentional differences

Linux uses one host subreaper, pidfds, and native PID namespaces. It requires restricted filesystem policy, namespace-local procfs, and the native namespace prerequisites in the README. Full-disk write policies and native procfs fallback are rejected in this supervised path. Kernel-uninterruptible processes can exceed the retirement deadline; that is reported as failure, with private storage retained.

Linux's restricted-network seccomp rules are unchanged from upstream, including denials of socket-specific operations on local pairs when no managed proxy is used. The native execution gate uses permitted descriptor read/write on its private socket; the kernel still supplies the readiness sender's identity. Persistent application sidebands must obey the same rules; see [MCP_CONSOLE_HANDOFF.md](MCP_CONSOLE_HANDOFF.md).

The supervisor observes root completion without reaping it and preserves that identity until retirement finishes. On Darwin it also enumerates and retires live members of the original owned process group, including children orphaned before fork-event discovery. A separate host process outside that group is not part of this retirement.

Darwin provides neither a Linux-style subreaper nor pidfd signalling. The supervisor observes fork/exit events and records PID plus start time, retaining observed descendants across `setsid`, reparenting, and root exit. A descendant that leaves the owned group and orphans before observation remains outside this guarantee. There is also a residual identity-check-to-kill PID reuse race on Darwin. These are platform boundaries, not recovery promises.

If the sole supervisor crashes or receives SIGKILL, there is no custom recovery. Descendant cleanup, directory deletion, proxy availability, and terminal restoration are not guaranteed. A surviving workload remains subject to inherited native sandbox restrictions. Killing a native Linux namespace init may instead terminate its workload through ordinary kernel/native behavior. Tests exercise both outcomes without adding a failsafe process.

Trusted caller-supplied Seatbelt extensions can grant permissions and therefore can deliberately override native restrictions. The target receives no supervisor task port, host process-control descriptors, configuration transport, or proxy-control interface under the tested restricted policy. This does not protect the supervisor from an unrelated, already-unrestricted same-user host process. Initial executable/loader trust, ordinary launch size limits, and caller-supplied stdio authority remain normal OS launch responsibilities.

The behavioral reference is MCP Console main `620daf07c6fdc37ab2ecb6b0e5e8bdb52b142d1d`, especially `tests/boundaries/cli/sandbox/test_execution.py`, `test_supervision.py`, `test_signals.py`, and its native supervision code. This runner adds strict cleanup-error reporting and optional capabilities without copying console policy defaults. It intentionally omits console's manager failure-recovery machinery and does not implement job suspension/resumption or application restart. Windows is unsupported. MCP Console source, tests, and snapshots are unchanged.

## Executable acceptance tests

All tests run the built executable and generic Rust/system-command fixtures; none require MCP Console. Names below are in `tests/bootstrap_contract.rs` or the indicated sibling module.

| Required behavior                                             | Tests                                                                                                                                                                                                                                                                                                                |
| ------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Normal completion, nonzero exit, signal mapping               | `valid_bootstrap_launches_one_command`, `exit_codes_and_native_signal_mapping_are_preserved`, `lifecycle::private_storage_is_removed_after_success_and_nonzero_exit`                                                                                                                                                 |
| Interrupted root and detached descendants                     | `lifecycle::interruption_preserves_target_status_and_retires_its_descendants`, `lifecycle::retirement_kills_detached_descendant_and_waits_for_storage_cleanup`                                                                                                                                                       |
| Observed descendants after normal root exit                   | `startup::normal_exit_retires_an_observed_detached_descendant` (Darwin kernel-watch checkpoint; Linux PID namespace)                                                                                                                                                                                                 |
| Unobserved children in the owned group after exit 0/23, held streams and partial output | `retirement::root_exit_retires_unobserved_group_and_closes_partial_output` (Darwin discovery paused until root exit; unrelated host process remains alive) |
| Configured caller death, including startup                    | `lifecycle::configured_caller_death_retires_the_lifetime`, `startup::caller_death_during_native_startup_never_releases_target`                                                                                                                                                                                       |
| Cancellation before target release                            | `startup::cancellation_after_native_spawn_keeps_target_gated_and_cleans_storage`, `startup::cancellation_interrupts_incomplete_bootstrap_without_transport_eof`                                                                                                                                                      |
| Replacement, mode-zero directories, symlinks, removal failure | `lifecycle::private_storage_is_removed_after_success_and_nonzero_exit`, `startup::failed_private_directory_removal_is_reported`                                                                                                                                                                                      |
| Honest discovery/termination failure                          | `startup::failed_descendant_discovery_is_reported_and_storage_is_retained`, `startup::failed_termination_is_reported_and_storage_is_retained` (Darwin syscall fault injection)                                                                                                                                       |
| Original input and prompt stdin closure                       | `binary_stdin_is_independent_of_bootstrap`, `transport::waiting_executable_releases_target_stdin`, `transport::regular_file_stdin_preserves_offset_seekability_and_shared_description`, `transport::terminal_stdin_preserves_its_device_identity`, `transport::null_and_closed_stdin_follow_native_runtime_behavior` |
| Terminal interrupts and peers                                 | `terminal::controlling_terminal_delivers_interrupt_once`, `terminal::foreground_peer_keeps_terminal_ownership` (Darwin)                                                                                                                                                                                              |
| Inherited signals and waitable children                       | `lifecycle::inherited_signal_state_reaches_target_and_sigchld_remains_waitable`                                                                                                                                                                                                                                      |
| Explicit, fixed configuration                                 | `configuration::*`, `transport::bootstrap_reads_exactly_one_frame`, `security::same_user_target_cannot_control_supervisor_or_change_accepted_policy`                                                                                                                                                                 |
| Descriptor isolation and same-user control denial             | `no_launcher_or_proxy_descriptors_reach_the_target`, `transport::bootstrap_resource_is_closed_while_target_and_proxy_are_alive`, `security::same_user_target_cannot_control_supervisor_or_change_accepted_policy`                                                                                                    |
| Upstream socket denials and permitted local descriptor I/O    | `restricted_network_preserves_native_socket_denials_and_descriptor_io`                                                                                                                                                                                                                                               |
| Native boundary established before loader code                | `startup::target_loader_runs_only_after_native_enforcement`, `configuration::native_setup_rejects_procfs_fallback_before_target_execution` (Linux)                                                                                                                                                                   |
| Enforcement survives supervisor loss                          | `security::supervisor_loss_cannot_remove_native_enforcement`                                                                                                                                                                                                                                                         |
| Filesystem, network, proxy, and caller extension              | `filesystem_policy_denies_and_grants_writes`, `network_permission_controls_direct_connections`, `managed_proxy_applies_the_upstream_allowlist`, the Seatbelt extension and native-profile tests                                                                                                                      |
