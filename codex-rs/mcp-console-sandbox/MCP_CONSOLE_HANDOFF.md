# Historical MCP Console handoff: sideband I/O

This records the sideband migration proposed against MCP Console main `c3027d71a86f837804ff3234dd1fab5c9103ff40`. Paths, symbols, and test references below refer to that revision. It is historical design context, not a current downstream task list or pin recommendation. [PR266.md](PR266.md) records the subsequent fixes at their original revisions. Current runner integration and reapplication instructions are in [INTEGRATION.md](INTEGRATION.md) and [REBASE.md](REBASE.md).

The proposal was to adopt the standalone supervisor and adapt the relay-to-worker sideband to the unchanged upstream Linux network policy. The runner's earlier socket-operation relaxation was removed by `270b25515f305d30f5889d815014cd4756d12f5a`.

The reference checkout's `sandbox-runner.json` pinned `3ee7d3190983b482b312ddfc3201c464179a1245`, protocol 2. No Console source, tests, or snapshots were changed while preparing this handoff.

## Why a local socket encounters the sandbox

The sandboxed relay creates the sideband and launches the worker inside the same sandbox. Both execute filtered syscalls. The runner's application supervisor is a separate host process that owns launch and cleanup; it is not the relay and does not own their sideband.

The upstream restricted-network seccomp filter checks syscall numbers and arguments. It does not resolve a descriptor to determine that both endpoints belong to this sandbox. Children inherit the filter across fork and exec. [Kernel seccomp documentation](https://docs.kernel.org/userspace-api/seccomp_filter.html).

The pinned filter permits `socketpair(AF_UNIX)` and ordinary descriptor `read`/`write`, but denies `sendto`, `shutdown`, `getsockname`, `getpeername`, `getsockopt`, and `setsockopt`. Rust's Linux `UnixStream::write` uses `send`/`sendto`; creating the pair successfully therefore does not establish that every socket method is usable. These statements concern restricted networking without a managed proxy; the upstream proxy mode has its own rules.

The current runner's native endpoint uses `File` read/write on its owned socket descriptor. The host supervisor sets `SO_PASSCRED` before launch and receives kernel-supplied sender credentials. Linux namespace init retains that endpoint for signals and retirement after setup; the workload never inherits it. This internal control channel and the application's sideband have separate ownership and cancellation contracts; see [INTEGRATION.md](INTEGRATION.md#procfs-control-and-lifetime-review).

## Sideband work

Inspect these Console paths before editing:

- `src/sideband.rs`: endpoint creation and inheritance, framing, `Writer::send`, `Writer::shutdown`, nonblocking retirement reads, and fork-child descriptor closure.
- `src/worker_relay.rs`: `SidebandWriter::cancel_and_join`, `SidebandReader`, retirement draining, and the direct worker's stdio ownership.
- `src/worker_client/startup.rs`, its platform modules, and `src/worker_client/unix.rs`: readiness and relay launch. Distinguish host-only sockets from sideband operations executed inside the sandbox.
- `src/worker/core.rs` and sideband callers: worker-side writes, runtime initialization, and inherited signal handling.

Two anonymous pipes are a reasonable starting design: one relay-to-worker request pipe and one worker-to-relay response pipe. Create them in the already-sandboxed relay, then inherit only the worker's read and write ends into that child. This preserves the runner's rule that unrelated host descriptors do not pass through its boundary. It also gives each direction its own EOF and avoids socket-specific methods. [Pipe semantics](https://man7.org/linux/man-pages/man7/pipe.7.html).

The previous socket design reduced descriptor bookkeeping and allowed `shutdown(Write)` to interrupt a write while keeping reads available for draining. Returning to pipes needs an explicit replacement for that cancellation behavior:

1. Give each writing endpoint one I/O owner. Use nonblocking writes with readiness polling and an explicit cancellation wakeup, so a full pipe cannot trap shutdown or restart in a blocking write or thread join. Handle partial writes and interrupted syscalls without interleaving messages. Closing another thread's descriptor is not a substitute for cancelling its blocked operation. [Linux close behavior](https://man7.org/linux/man-pages/man2/close.2.html).
2. Preserve the reader's retirement contract: drain complete buffered/readable frames within the existing deadline, abandon an incomplete tail, and terminate even if a descendant holds a pipe end open. A cancellation wakeup must not depend on sideband EOF or worker cooperation.
3. Close unused ends promptly after spawn and before waiting. Mark adopted worker descriptors close-on-exec, remove the transport environment variables, and close both descriptors in fork-only runtime descendants. Those child closures must leave the parent's channel usable.
4. Account for `SIGPIPE`/`EPIPE` when using descriptor writes and for duplicated descriptors delaying EOF. Preserve the inherited-signal contract and test peer loss; do not introduce a blanket process-wide signal change just to mask a transport error.
5. Preserve JSON framing, message serialization, raw stdout/stderr separation, ordering, and backpressure. Keep one chosen transport rather than adding pipe/socket fallback modes.

A socket using permitted descriptor I/O and explicit cancellation could also satisfy the policy. The decision is about ownership and cancellation complexity, not whether a local socket is intrinsically incompatible with sandboxing. Do not preserve the old success test by enabling networking, adding a proxy, moving the relay outside the sandbox, or restoring syscall allowances.

## Runner integration reference

At the reference revision, `src/sandbox/runner.rs`, platform launchers, `src/sandbox/installation.rs`, and `scripts/stage-sandbox-runner` were the downstream integration points. The proposed adoption removed the separate manager/monitor and `sandbox-target` signal wrapper. Application session/restart logic and relay/worker framing remained downstream responsibilities.

Use the current [protocol handoff](PROTOCOL.md#downstream-handoff) for both configuration transports and caller responsibilities, and [LIFECYCLE.md](LIFECYCLE.md) for retirement and supervisor-loss limits. A historical pin or recovery test here must not override those contracts.

## Acceptance criteria

Write public regressions first and run them against the newly pinned executable on Linux and macOS. Linux cases must use restricted networking with no proxy. Use checkpoints and readiness events rather than longer sleeps or relaxed deadlines.

| Contract                                                         | Existing Console references and required coverage                                                                                                                                                                 |
| ---------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Ordinary sideband messages and stream separation                 | `tests/boundaries/client_server/protocol/test_transport.py::test_routes_send_over_sideband`; malformed/invalid sideband cases in `output/test_streams.py`.                                                        |
| A blocked writer cannot delay shutdown                           | `sandbox/test_shutdown.py::test_shutdown_deadline_does_not_wait_for_sideband_writer`; fill the actual transport, observe the blocked/backpressured state, then cancel.                                            |
| Partial frames and held-open descendants cannot delay retirement | `test_restart_cancels_partial_sideband_frame`, `test_shutdown_cancels_partial_sideband_frame`, `test_restart_cancels_reader_after_operation_result`.                                                              |
| Preserve complete output before abandoning a partial tail        | `test_restart_drains_readable_frame_before_abandoning_partial_tail`; preserve complete-frame ordering and raw output.                                                                                             |
| Descriptor and fork ownership                                    | Worker receives exactly its two pipe ends if pipes are chosen; no setup/control descriptors leak; fork-only descendants cannot use the sideband; the parent's sideband remains usable after child cleanup.        |
| Peer loss, EOF, and signals                                      | Exercise both directions, a full output buffer, duplicate endpoints, and peer exit. Preserve stdin closure, binary data, inherited signals, and terminal behavior.                                                |
| Native policy remains enforced                                   | Normal protocol exchange succeeds while new host network connections and forbidden filesystem writes fail. Retain the runner's `restricted_network_preserves_native_socket_denials_and_descriptor_io` regression. |

The unqualified shutdown test names above are in `tests/boundaries/client_server/sandbox/test_shutdown.py`. Run Console's `scripts/check` after focused regressions. Report actual platform results and any intentional lifecycle-contract changes; do not update snapshots merely to accommodate lost output, weaker cancellation, or broader permissions.
