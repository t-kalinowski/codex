# Bootstrap protocol version 1

Invoke `mcp-console-sandbox` without arguments. Stdin contains:

```text
[4-byte unsigned big-endian JSON length][UTF-8 JSON][ordinary target input]
```

The JSON payload must contain 1 through 1,048,576 bytes. The runner reads the header and payload directly from fd 0 with no buffered stdin abstraction and no reads beyond the remaining frame length. It deserializes exactly one JSON value, validates version 1 and a nonempty command, and transfers that same open file description to the native launch. Target input can be queued immediately. The caller does not need to close stdin before launch.

There are no later control messages, acknowledgments, success frames, stream handles, discovery methods, setup status, or lifecycle operations. Stdout and stderr are inherited directly. Configuration failures return 1 and an error on stderr. Native launch failures and target exits retain the native status; signals map to `128 + signal`. No runner output is written to stdout.

## Request

```json
{
  "version": 1,
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
  "proxy": null
}
```

| Field                              | Type and meaning                                                                                                                                                              |
| ---------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `version`                          | Unsigned integer; must equal 1.                                                                                                                                               |
| `command`                          | Nonempty UTF-8 string array. The first item names the executable; later items are arguments. The runner does not insert a shell. Native executable lookup rules apply.        |
| `cwd`                              | Upstream `AbsolutePathBuf`; a host-local absolute working directory. It is also the sandbox policy's working-directory context.                                               |
| `environment`                      | Complete map of UTF-8 names and values, passed with the native proxy overrides documented in the README.                                                                      |
| `filesystem`                       | Upstream `RawFileSystemSandboxPolicy`, converted through its existing `TryFrom` into `FileSystemSandboxPolicy` and `PermissionProfile`.                                       |
| `network`                          | Upstream `NetworkSandboxPolicy`: `"restricted"` or `"enabled"`.                                                                                                               |
| `proxy`                            | Optional upstream `RemoteNetworkProxyConfig`. Omit or use `null` for no proxy. A supplied configuration must have `enabled: true`.                                            |
| `macos_seatbelt_profile_extension` | Optional trusted SBPL string appended to the native macOS Seatbelt profile. Omit or use `null` to leave the native profile unchanged. A supplied string is rejected on Linux. |

The wrapper rejects unknown top-level fields. Nested upstream types retain the release's own serialization and validation rules. Version 1 requires UTF-8 command arguments, paths, and environment values; OS strings with other byte encodings are outside this protocol. Native argument and environment size and NUL restrictions still apply.

`macos_seatbelt_profile_extension` is trusted caller configuration. The runner appends a newline and the string to the profile prepared by the native Seatbelt backend, then launches the same `/usr/bin/sandbox-exec -p` command. It verifies that backend and command shape before appending. It does not initialize a second sandbox. Invalid SBPL fails through the native launcher before the target starts.

The extension can grant permissions as well as restrict them. The runner does not parse it or validate it as deny-only; the caller owns its interaction with the native filesystem, network, and platform rules. Do not populate this field from untrusted target input. The runner contains no application-specific rules.

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

`mode` is the upstream `"full"` or `"limited"` mode. Domain and Unix-socket permissions use the upstream map types. The runner starts the native managed proxy, obtains its sandbox context and target environment, and asks the default sandbox manager to enforce that context. The separate `network` field is never collapsed into a proxy-selection enum.

## Caller example

This Python caller queues binary input immediately after the JSON:

```python
import json
import struct
import subprocess

request = {
    "version": 1,
    "command": ["/bin/cat"],
    "cwd": "/tmp",
    "environment": {},
    "filesystem": {
        "kind": "restricted",
        "entries": [
            {"path": {"type": "special", "value": {"kind": "root"}}, "access": "read"}
        ],
    },
    "network": "restricted",
    "proxy": None,
}
payload = json.dumps(request).encode("utf-8")
assert 0 < len(payload) <= 1024 * 1024
sentinel = bytes(range(256)) * 64
result = subprocess.run(
    ["/absolute/path/to/mcp-console-sandbox"],
    input=struct.pack(">I", len(payload)) + payload + sentinel,
    capture_output=True,
    check=True,
)
assert result.stdout == sentinel
assert result.stderr == b""
```

The outer invocation marks inherited descriptors above stderr close-on-exec before starting the runtime. Native helper re-execs keep their own temporary setup descriptors until the upstream code releases them. The final target inherits only stdio and descriptors required by native sandbox operation.
