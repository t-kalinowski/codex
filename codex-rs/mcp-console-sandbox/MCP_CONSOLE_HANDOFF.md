# MCP Console handoff: sideband I/O with upstream Linux restrictions

Update MCP Console to use the standalone supervisor and make its relay-to-worker sideband work under the pinned upstream Linux network policy. The runner's socket-operation relaxation has been removed. Keep that policy unchanged during integration.

Reference checkout: MCP Console main `c3027d71a86f837804ff3234dd1fab5c9103ff40`. At that revision, `sandbox-runner.json` pins `3ee7d3190983b482b312ddfc3201c464179a1245`, protocol 2. Adopt runner commit `270b25515f305d30f5889d815014cd4756d12f5a` from `t-kalinowski/codex`, branch `mcp-console/sandbox-runner/rust-v0.150.1`, keeping protocol 2 and rebuilding the staged artifacts. This note specifies work in MCP Console; no Console source, tests, or snapshots were changed while preparing it.

## Why a local socket encounters the sandbox

The sandboxed relay creates the sideband and launches the worker inside the same sandbox. Both execute filtered syscalls. The runner's application supervisor is a separate host process that owns launch and cleanup; it is not the relay and does not own their sideband.

The upstream restricted-network seccomp filter checks syscall numbers and arguments. It does not resolve a descriptor to determine that both endpoints belong to this sandbox. Children inherit the filter across fork and exec. [Kernel seccomp documentation](https://docs.kernel.org/userspace-api/seccomp_filter.html).

The pinned filter permits `socketpair(AF_UNIX)` and ordinary descriptor `read`/`write`, but denies `sendto`, `shutdown`, `getsockname`, `getpeername`, `getsockopt`, and `setsockopt`. Rust's Linux `UnixStream::write` uses `send`/`sendto`; creating the pair successfully therefore does not establish that every socket method is usable. These statements concern restricted networking without a managed proxy; the upstream proxy mode has its own rules.

The standalone runner retains a socket for its short startup exchange. The gated native endpoint uses `File` read/write on the owned socket descriptor. The host supervisor sets `SO_PASSCRED` before launch and receives kernel-supplied sender credentials, so process identification still works. That exchange ends before target execution and does not have the long-lived sideband's cancellation requirements.

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

## Runner integration

Update `sandbox-runner.json` and restage through `scripts/stage-sandbox-runner` using a clean checkout at the new exact pin. Keep protocol version 2. Follow [PROTOCOL.md](PROTOCOL.md) and [LIFECYCLE.md](LIFECYCLE.md):

- Use `--config-env NAME -- command [args...]` for a thin exec-style frontend. JSON contains the policy and explicit lifecycle options; arguments, cwd, and ordinary target environment remain launch inputs. Keep the framed `--bootstrap-fd N` interface only where its larger request capacity is needed. Configuration is fixed at frame acceptance in that mode.
- Express Console's existing private-directory exports, caller-death behavior, SIGTERM retirement, filesystem/network policy, and trusted macOS extension explicitly. Pass the actual direct parent's PID if using `lifecycle.parent_pid`. Do not retain an intermediate manager solely to satisfy an old PID relationship.
- Remove the application-level sandbox manager/monitor and `sandbox-target` signal-restoration wrapper when switching to this supervisor. Keep application session/restart logic and relay/worker protocol responsibilities in Console. The native runner owns descendant retirement, private storage, terminal restoration, and proxy lifetime.
- Treat retirement or cleanup errors as failures. The runner deliberately provides no custom cleanup recovery after its sole supervisor crashes or receives SIGKILL; surviving workloads retain native enforcement. Document that intentional difference and replace manager-specific recovery expectations with tests of retained native enforcement. Preserve public cleanup checks while the supervisor is alive.

The current `src/sandbox/runner.rs`, platform launchers, and `src/sandbox/installation.rs` are the main integration points. Keep sideband compatibility work separate from unrelated runtime or transcript changes.

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
