# Bootstrap protocol version 2

Two explicit input modes are supported. Both accept one immutable configuration and use the same supervisor.

## Environment configuration

Invoke `mcp-console-sandbox --config-env NAME -- command [args...]`. `NAME` selects a UTF-8 JSON value already present in this child's launch environment, never a filename. The JSON requires `version`, `filesystem`, and `network` from the request below. `command` and `cwd` are rejected: supply the command after `--` and select the working directory when launching the child. The optional `environment` object contains target-only overrides. `inherit_environment` defaults to `true`; set it to `false` to start with an empty target environment. Omitting both fields inherits the ordinary launch environment without requiring its serialization. Private-directory exports and managed proxy values override ordinary target settings. No application policy is merged into an explicit configuration.

The OS copies the launch environment during process creation. The runner reads the selected value once into owned validated state before runtime or native setup. Changing the parent's environment after successful launch, even before the runner parses JSON, cannot alter that copy. There is no file discovery, path reference, include, reload, or overflow file. Both input modes reject conflicting invocation options and duplicate top-level fields.

The selected name and reserved `MCP_CONSOLE_SANDBOX_CONFIG` are removed from all helper and target environments, even if an explicit target map tries to reintroduce them. Neither name may be a private-directory export. The standalone runner also removes them from its own environment before creating threads, so helper libraries cannot interpret a transport value as another setting. Removal prevents accidental propagation; it is neither authentication nor secure erasure. Native sandbox enforcement protects the accepted policy and host-side control state. The macOS runner explicitly denies `process-info-pidinfo` queries outside the sandbox, including supervisor launch-environment reads through `KERN_PROCARGS2`, while preserving same-sandbox process inspection.

Target settings are installed after native setup and enforcement. They never configure the host supervisor, helper lookup, loader, temporary setup resources, or proxy policy. On Linux, helper selection uses the trusted launch `PATH`; the target's `PATH` is used for target execution only. Native proxy endpoints take precedence, including their namespace-local translations on Linux. As with any executable, trust the environment used to load the runner itself, including loader settings, and choose a transport name that is not a loader setting. Callers must construct a child-specific environment rather than temporarily mutating a multithreaded parent's environment.

The JSON cap is 1 MiB, subject to the OS's argument/environment limits, including per-string limits on Linux. An oversized initial exec fails in the caller with `E2BIG`, before runner diagnostics can run. An oversized helper or target exec reports the OS error on stderr and returns nonzero. Parse diagnostics report a category and location without echoing configuration values or entire environments. Use the explicit private descriptor mode for configurations too large for environment transport; it cannot bypass the final target's argument/environment limits.

## Framed descriptor configuration

Invoke `mcp-console-sandbox --bootstrap-fd <N>`. Supply exactly that option and one decimal descriptor number greater than 2. The descriptor must already be open and readable in the child; its number need not be 3.

```text
fd 0: original target stdin
fd 1: target stdout
fd 2: target stderr
fd N: [4-byte unsigned big-endian JSON length][UTF-8 JSON]
```

This is a breaking private protocol change. Only version 2 is accepted. The no-argument invocation is rejected, and stdin is never read to discover configuration or select a protocol. There is no compatibility mode or environment-variable fallback.

The launcher normally creates an anonymous pipe, inherits its read end into the executable, closes its own read end, and retains the writer until it sends configuration. Start the executable before writing a potentially pipe-sized frame. There is no descriptor transfer after process creation.

The JSON payload must contain 1 through 1,048,576 bytes. The executable validates the descriptor before opening internal files or creating its runtime, adopts it once, and reads exactly the declared frame while observing cancellation signals. A complete valid request releases startup without waiting for EOF; withholding the frame keeps the target and managed proxy from starting. Closing the writer before completing the frame cancels startup.

In this mode configuration becomes fixed at request acceptance, not process creation. The trusted caller must retain exclusive control of the bytes and descriptor writers until the complete frame is accepted. An anonymous pipe does not authenticate a writer, and a mutable descriptor source can change while being read. The caller may prepare the request after spawning the supervisor. The accepted request is owned memory; the descriptor closes before setup, and further writes or changes to its backing resource cannot alter policy. Neither the caller nor the runner spills configuration to mutable files.

The bootstrap descriptor is closed after parsing and validation, before runtime, proxy, or native setup. It is not a persistent control channel. Empty or truncated frames, invalid lengths, malformed requests, unsupported versions, and invalid invocation arguments return a nonzero status and an error on stderr without launching the target. Remaining request fields retain their native validation.

Stdin, stdout, and stderr are attached directly. The target inherits stdin's original open file description, including its offset, seekability, terminal identity, and binary contents. This executable drops its owned stdin from the launch command immediately after spawning, before waiting for the child. Rust startup supplies `/dev/null` when stdin was closed at invocation, matching the existing runtime behavior.

There are no later control messages or acknowledgments. No protocol output is written to stdout. Native launch failures and target exits retain the native status; signals map to `128 + signal`. Native Linux helper invocations beginning with `--sandbox-policy-cwd` dispatch separately, including upstream compatibility re-execs; they do not take or reread a bootstrap.

## Request

```json
{
  "version": 2,
  "command": ["/bin/cat"],
  "cwd": "/tmp",
  "environment": {},
  "filesystem": {
    "kind": "restricted",
    "entries": [
      {
        "path": { "type": "special", "value": { "kind": "root" } },
        "access": "read"
      }
    ]
  },
  "network": "restricted",
  "proxy": null,
  "macos_seatbelt_profile_extension": null
}
```

| Field                              | Type and meaning                                                                                                                                                                                                  |
| ---------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `version`                          | Unsigned integer; must equal 2.                                                                                                                                                                                   |
| `command`                          | Nonempty UTF-8 string array. The first item names the executable; later items are arguments. The native sandbox executable does not insert a shell. Native executable lookup rules apply.                         |
| `cwd`                              | Upstream `AbsolutePathBuf`; a host-local absolute working directory. It is also the sandbox policy's working-directory context.                                                                                   |
| `environment`                      | Descriptor mode: required complete target map. Environment mode: optional target override map. Values and names must be UTF-8 and NUL-free; names must be nonempty and contain no `=`.                            |
| `inherit_environment`              | Environment mode only: optional Boolean, default `true`. Rejected in descriptor mode, whose environment is always explicit.                                                                                       |
| `filesystem`                       | Upstream `RawFileSystemSandboxPolicy`, converted through its existing `TryFrom` into `FileSystemSandboxPolicy` and `PermissionProfile`.                                                                           |
| `network`                          | Upstream `NetworkSandboxPolicy`: `"restricted"` or `"enabled"`.                                                                                                                                                   |
| `proxy`                            | Optional upstream `RemoteNetworkProxyConfig`. Omit or use `null` for no proxy. A supplied configuration must have `enabled: true`.                                                                                |
| `macos_seatbelt_profile_extension` | Optional trusted SBPL string appended to the native macOS Seatbelt profile. Omit or use `null` to leave the native profile unchanged. A supplied string is rejected on Linux.                                     |
| `linux_backend`                    | Linux only: `"bubblewrap"` (default) or explicit `"landlock"`. Landlock uses direct-exec semantics and rejects supervised-lifetime and managed-proxy requests; see [Linux compatibility](LINUX_COMPATIBILITY.md). |
| `lifecycle`                        | Optional object described in [LIFECYCLE.md](LIFECYCLE.md): parent observation, signal behavior, private storage, and cleanup deadline.                                                                            |

The wrapper rejects unknown top-level fields. Nested upstream types retain the release's own serialization and validation rules. Version 2 requires UTF-8 command arguments, paths, and environment values; OS strings with other byte encodings are outside this protocol. Native argument and environment size and NUL restrictions still apply.

`macos_seatbelt_profile_extension` is trusted caller configuration. The native stage applies the upstream profile, parameters, and appended SBPL together using `sandbox_init_with_parameters`, then restores signals and execs the target. Invalid SBPL fails before the target starts.

The extension can grant permissions as well as restrict them. The native sandbox executable does not parse it or validate it as deny-only; the caller owns its interaction with the native filesystem, network, and platform rules. Do not populate this field from untrusted target input. The native sandbox executable contains no application-specific rules.

For example, append this entry to grant one writable directory:

```json
{ "path": { "type": "path", "path": "/absolute/workspace" }, "access": "write" }
```

A managed proxy configuration uses the release's existing camelCase wire type:

```json
{
  "enabled": true,
  "enableSocks5": true,
  "enableSocks5Udp": false,
  "allowUpstreamProxy": false,
  "dangerouslyAllowAllUnixSockets": false,
  "mode": "full",
  "domains": { "example.com": "allow", "blocked.example.com": "deny" },
  "unixSockets": null,
  "allowLocalBinding": false
}
```

`mode` is the upstream `"full"` or `"limited"` mode. Domain and Unix-socket permissions use the upstream map types. The native sandbox executable starts the native managed proxy, obtains its sandbox context and target environment, and asks the default sandbox manager to enforce that context. The separate `network` field is never collapsed into a proxy-selection enum.

## Caller example

[examples/bootstrap.py](examples/bootstrap.py) is a runnable Python 3.10+ caller using `os.pipe` and `subprocess.Popen(pass_fds=...)`. It inherits only the bootstrap read descriptor beyond stdio, starts before writing the frame, closes unused pipe ends, and waits for the target through the native sandbox executable. Failure closes the pipe and kills and reaps the immediate child.

From this package directory:

```sh
printf 'target input\n' | python3 examples/bootstrap.py /absolute/path/to/mcp-console-sandbox
```

The example leaves target stdin directly attached and sends configuration independently. An application can delay the write until its request is ready. Lifecycle capabilities are selected in the same request.

The outer invocation still marks unrelated inherited descriptors above stderr close-on-exec before starting the runtime. The bootstrap descriptor is explicitly closed before native setup. Native helper re-execs retain their own temporary setup descriptors until the upstream code releases them.

## Downstream handoff

A thin frontend can exec the environment-mode invocation with its ordinary command arguments, cwd, and target environment. It should explicitly select its private-directory exports, parent PID if retirement on caller death is wanted, SIGTERM behavior, filesystem/network rules, and any trusted macOS extension.

The runner owns launch, descendant retirement, native signal restoration, optional private storage, and proxy lifetime. An application-level fork-and-continue manager and sandbox-target signal wrapper are unnecessary. Large requests may retain the private descriptor mode. The coordinated downstream integration and procfs probes are described in [Linux compatibility](LINUX_COMPATIBILITY.md).

The Linux native control channel is private to the runner and namespace init. It carries signals after one-shot setup, never configuration updates, and is close-on-exec in the workload. This does not change either caller transport. Explicit Landlock transports its target environment across trusted setup in a sealed anonymous file and closes that descriptor before target execution; no waiting supervisor remains.
