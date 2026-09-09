# Bootstrap protocol version 2

Two explicit input modes are supported. Both accept one immutable configuration and use the same supervisor.

## Environment configuration

Invoke `mcp-console-sandbox --config-env NAME -- command [args...]`. `NAME` must select UTF-8 JSON already present in the child environment. The JSON has the request fields below, excluding `command`, `cwd`, and `environment`; those three fields are rejected in this mode. The target uses the ordinary arguments, current working directory, and environment, with the selected variable removed and the documented private-directory and native proxy overrides applied. The selected variable cannot also name a private-directory export.

Configuration is copied once before runtime or native setup. There is no file, path reference, automatic file fallback, or reload. Changing the caller's environment after process creation cannot change the child's request. The maximum JSON size is 1 MiB, subject to the operating system's smaller exec argument/environment limits. As with any executable, the caller must trust the environment used to load the supervisor itself, including dynamic-loader settings. Use descriptor mode when the target environment must be distinct from that initial environment or the request exceeds exec limits.

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

In this mode configuration becomes fixed at request acceptance, not process creation. The caller may prepare the request after spawning the supervisor. The accepted request is owned memory; the descriptor closes before setup, and further writes or changes to its backing resource cannot alter policy.

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

| Field                              | Type and meaning                                                                                                                                                                          |
| ---------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `version`                          | Unsigned integer; must equal 2.                                                                                                                                                           |
| `command`                          | Nonempty UTF-8 string array. The first item names the executable; later items are arguments. The native sandbox executable does not insert a shell. Native executable lookup rules apply. |
| `cwd`                              | Upstream `AbsolutePathBuf`; a host-local absolute working directory. It is also the sandbox policy's working-directory context.                                                           |
| `environment`                      | Complete map of UTF-8 names and values, passed with the native proxy overrides documented in the README.                                                                                  |
| `filesystem`                       | Upstream `RawFileSystemSandboxPolicy`, converted through its existing `TryFrom` into `FileSystemSandboxPolicy` and `PermissionProfile`.                                                   |
| `network`                          | Upstream `NetworkSandboxPolicy`: `"restricted"` or `"enabled"`.                                                                                                                           |
| `proxy`                            | Optional upstream `RemoteNetworkProxyConfig`. Omit or use `null` for no proxy. A supplied configuration must have `enabled: true`.                                                        |
| `macos_seatbelt_profile_extension` | Optional trusted SBPL string appended to the native macOS Seatbelt profile. Omit or use `null` to leave the native profile unchanged. A supplied string is rejected on Linux.             |
| `lifecycle`                        | Optional object described in [LIFECYCLE.md](LIFECYCLE.md): parent observation, signal behavior, private storage, and cleanup deadline.                                                    |

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

The runner owns launch, descendant retirement, native signal restoration, optional private storage, and proxy lifetime. An application-level fork-and-continue manager and sandbox-target signal wrapper are unnecessary. Large requests may retain the private descriptor mode. No MCP Console integration is included in this patch; its source, tests, and snapshots remain unchanged.
