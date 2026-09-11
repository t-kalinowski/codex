# Linux host compatibility

The historical comparison table starts at the former downstream pin `d488fc969da435f93ea5937c7f284fa91a8c2575` and records runner `7aacbcf1bca0f173f036617a5ee8ae71e18fb8cc`. Default managed execution uses bubblewrap and the requested filesystem/network policy; caller-selected external enforcement is described in the [JSON reference](PROTOCOL.md#network-proxy-and-enforcement-selection). Namespace or policy failure never selects an unrestricted target or a different backend. Current source integration and upgrade review points are in [INTEGRATION.md](INTEGRATION.md).

| Host condition                                            | Previous integration                                      | Current behavior                                                                |
| --------------------------------------------------------- | --------------------------------------------------------- | ------------------------------------------------------------------------------- |
| Fresh namespace-local procfs                              | Supported                                                 | Supported                                                                       |
| Native procfs preflight selects inherited procfs          | Rejected at the target hook                               | Supported with the same namespace and policy enforcement                        |
| `pidfd_open` unavailable or denied                        | Startup fails                                             | Native control and direct-child waits remain available                          |
| `pidfd_send_signal` unavailable or denied                 | Retirement fails                                          | Native channel requests retirement; completion still requires the child wait    |
| Host subreaper unavailable                                | Startup fails                                             | No host subreaper requested                                                     |
| Host child lists or namespace PID discovery unavailable   | Cleanup depended on host child lists                      | No child-list or `NSpid` discovery                                              |
| No native child completion evidence                       | Could mistake a missing child-list file for an empty list | Nonzero failure and retained private storage                                    |
| Stopped namespace init, pidfds unavailable                | Startup already unsupported                               | Retirement deadline fails explicitly; private storage remains                   |
| Explicit Landlock filesystem/network backend              | Rejected                                                  | Direct exec, native policy checks, no process isolation or supervised lifecycle |
| Unavailable namespace operations with bubblewrap selected | Failure                                                   | Failure; no backend switch                                                      |

The comparison's tested baseline is x86_64 Ubuntu with kernel `6.8.0-139-generic`, not a claimed minimum. [REBASE.md](REBASE.md#validation-of-the-01540-reapplication) records later platform results. The constrained-host tests deny pidfd and subreaper syscalls in the runner and helpers; fault fixtures also withhold native wait status and stop namespace init.

## Host requirements

Bubblewrap execution needs mounted procfs, permission for the selected helper's namespace operations, and the requested seccomp/network enforcement capabilities. A fresh procfs mount is optional: the inherited view retains user, mount, and PID isolation, but exposes permitted host PIDs and metadata. Process tools that assume procfs PIDs match namespace PIDs may not work there. Caller-selected unrestricted filesystem access is supported with restricted, enabled, or managed-proxy networking. Namespace init remains active for setup, signals and ordinary retirement even with full network access. Writable host paths and procfs can weaken protection of helpers, supervisor authority, or other same-user processes; this is not limited to cleanup failure. Influencing an unsandboxed process through shared files can indirectly bypass network or other restrictions. These accepted limits do not change the native network policy or require fresh procfs. Restricted filesystem policies retain their stronger boundary.

This command probes namespace permissions:

```console
unshare --user --map-root-user --mount --net --pid --fork true
```

Ubuntu AppArmor can block setup even with `kernel.unprivileged_userns_clone=1` and nonzero `user.max_user_namespaces`. Errors such as `setting up uid map: Permission denied` or `loopback: Failed RTM_NEWADDR: Operation not permitted` can reflect that restriction. The repository's Linux CI setup enables namespaces and removes the restriction on its runner; containers also need permission for native namespace operations. A kernel version alone does not establish these capabilities. Failure never selects Landlock or an unsandboxed target automatically.

## Procfs evidence

The coordinated downstream source at `f21abe426014ba70ef2a1ab7419955396d385aab` contains [test_procfs.py](https://github.com/t-kalinowski/mcp-console/blob/f21abe426014ba70ef2a1ab7419955396d385aab/tests/boundaries/cli/sandbox/test_procfs.py): `test_fresh_procfs_preserves_policy_and_supervisor_isolation` and `test_inherited_procfs_preserves_policy_and_supervisor_isolation`. Its [procfs fixture](https://github.com/t-kalinowski/mcp-console/blob/f21abe426014ba70ef2a1ab7419955396d385aab/tests/fixtures/cli/sandbox/procfs.py) and [namespace setup](https://github.com/t-kalinowski/mcp-console/blob/f21abe426014ba70ef2a1ab7419955396d385aab/tests/support/linux_sandbox.py) define the actual probes. These are downstream tests, not part of this package's executable suite. The [validation record](https://github.com/t-kalinowski/mcp-console/blob/f21abe426014ba70ef2a1ab7419955396d385aab/docs/LINUX_COMPATIBILITY.md#validation-record) applies to the recorded runner/downstream revisions and hosts; it does not validate later artifacts or every kernel interface.

Before removing the procfs restriction, differential probes exercised the pinned native helper with fresh procfs and its supported inherited-procfs mode. They used disposable same-user processes, an explicitly ptraceable synthetic host fixture, synthetic environment/memory/file data, an open writable host file, a control pipe, and a loopback listener. The matrix covered root-readable and denied-sentinel filesystem policies, each with restricted and enabled networking.

Inherited procfs exposed host PIDs. Access through host `environ`, `root`, `cwd`, file descriptors, and memory remained denied. File writes, host control-pipe writes, signals, ptrace, process-memory syscalls, and network-namespace entry did not bypass the selected policy. Direct file reads and loopback connections followed the requested read and network permissions. Readable process metadata was evaluated against the declared read policy.

The downstream executable tests repeat the matrix against the supervised runner, including a real nested mount that makes the native procfs preflight choose its supported fallback. They also attempt to read the supervisor's launch environment, write its memory, acquire an intentionally inherited host control pipe, and signal it. Listing fd-directory names can succeed under the root-read policy; following those links and accessing the control endpoint remain denied. Changing the transport-looking environment variable inside the target does not change accepted policy.

## Unrestricted procfs observations

On 2026-09-11, the current implementation was exercised with fresh procfs and with an outer bubblewrap mount of read-only `/proc/sys` that makes the inner native preflight select inherited procfs. The host was x86_64 Linux `6.8.0-139-generic` in a privileged Ubuntu container. The eight launches combined root-readable restricted or unrestricted filesystem policies with restricted or enabled networking and ordinary private storage. Probes used disposable same-user processes, synthetic files/environment/memory, an explicitly ptraceable fixture, a private control pipe and a loopback listener; no unrelated host data was inspected.

Unrestricted direct writes succeeded in both procfs views. Direct network connections succeeded only with enabled networking. Inherited procfs exposed host PIDs and supervisor fd-directory metadata. The tested host/supervisor environment, memory and control-endpoint access, ptrace/process-memory operations and network-namespace entry remained denied. Fresh procfs hid the host identities. All eight launches completed without runner diagnostics; synthetic host memory and control pipes were unchanged.

These probes ran as container root, whose host capabilities differ from the sandbox target's dropped capabilities. A non-root attempt failed during native network-namespace setup with `Failed RTM_NEWADDR: Operation not permitted`, before the probe could run. The observations therefore do not establish the same procfs access boundary for every same-user, capability, AppArmor or kernel configuration. They do not establish protection against influencing unsandboxed processes through shared writable files, and do not make that stronger guarantee a prerequisite for unrestricted access.

## External enforcement

Without a managed proxy, `external-sandbox` uses upstream selection of no native sandbox. The runner still supplies stdio, target environment, signals, caller-death observation and private storage. Linux retirement signals the original process group and waits for the direct child; the outer sandbox must handle detached descendants. Normal shutdown performs cleanup, but the direct-child wait does not prove all detached processes have retired. With a proxy, upstream native routing and namespace lifecycle are used. Declaring external restricted networking without providing an outer sandbox does not restrict host connections.

## Landlock boundary

Explicit Landlock is a different user-selected backend with native direct-exec semantics. The tested kernel allowed same-user host signalling in that mode. It must not substitute for bubblewrap when process isolation or descendant retirement is required. The runner rejects private storage, caller-death observation, retirement SIGTERM, an explicit cleanup deadline, and managed proxy routing with Landlock. Native rejection of restricted-read policies is preserved. Restricted filesystem policies also require the native truncate capability (Landlock ABI 3 or later); older best-effort enforcement would leave file truncation unrestricted. The tested host provides ABI 4. Native device-ioctl restrictions depend on ABI 5 and are not part of this backend's portable contract.

Helper selection, verification, and static/GNU packaging are described in [README.md](README.md#build-and-validation).
