# Bootstrap protocol version 2

Invoke `mcp-console-sandbox --bootstrap-fd <N>`. Supply exactly that option and one decimal descriptor number greater than 2. The descriptor must already be open and readable in the child; its number need not be 3.

```text
fd 0: original target stdin
fd 1: target stdout
fd 2: target stderr
fd N: [4-byte unsigned big-endian JSON length][UTF-8 JSON]
```

This is a breaking private protocol change. Only version 2 is accepted. The no-argument invocation is rejected, and stdin is never read to discover configuration or select a protocol. There is no compatibility mode or environment-variable fallback.

The launcher normally creates an anonymous pipe, inherits its read end into the executable, closes its own read end, and retains the writer until it sends configuration. Start the executable before writing a potentially pipe-sized frame. There is no descriptor transfer after process creation.

The JSON payload must contain 1 through 1,048,576 bytes. The native sandbox executable validates the descriptor before opening internal files or creating its runtime, adopts it once, and uses `File::read_exact` for the header and payload. It reads exactly the declared frame. A complete valid request releases startup without waiting for EOF; withholding the frame keeps the target and managed proxy from starting. Closing the writer before completing the frame cancels startup.

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

The wrapper rejects unknown top-level fields. Nested upstream types retain the release's own serialization and validation rules. Version 2 requires UTF-8 command arguments, paths, and environment values; OS strings with other byte encodings are outside this protocol. Native argument and environment size and NUL restrictions still apply.

`macos_seatbelt_profile_extension` is trusted caller configuration. The native sandbox executable appends a newline and the string to the profile prepared by the native Seatbelt backend, then launches the same `/usr/bin/sandbox-exec -p` command. It verifies that backend and command shape before appending. It does not initialize a second sandbox. Invalid SBPL fails through the native launcher before the target starts.

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

The example leaves target stdin directly attached and sends configuration independently. An application can delay the write until its manager is ready. Cancellation, descendant retirement, application temporary directories, terminal ownership, and signal delivery remain the caller's responsibilities.

The outer invocation still marks unrelated inherited descriptors above stderr close-on-exec before starting the runtime. The bootstrap descriptor is explicitly closed before native setup. Native helper re-execs retain their own temporary setup descriptors until the upstream code releases them.

## Downstream handoff

MCP Console should update its immutable source pin and protocol pin to version 2, inherit the bootstrap pipe read descriptor, pass `--bootstrap-fd <N>`, and leave the target's original stdin attached from process creation. Remove the SCM_RIGHTS stdin handoff and send the framed configuration when the manager is ready.

Keep any still-needed wrapper code that restores the target's original signal mask while the waiting native executable retains blocked signals. This transport change does not change signal masks or dispositions and does not account for that wrapper responsibility. MCP Console continues to own signal delivery and supervision. Native Linux helpers can also retain standard streams or manage descendants; their existing behavior is unchanged.
