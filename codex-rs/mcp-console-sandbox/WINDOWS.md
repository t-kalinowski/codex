# Windows build and port assessment

The Windows executable exposes Codex's existing Windows sandbox launcher through
`--run-as-windows-sandbox`. This is a small adapter over
`codex_windows_sandbox::run_windows_sandbox_wrapper_main`, with no dependency on
`codex-core` or a running Codex application. It forwards stdin, stdout, stderr,
and the target exit code through the upstream Windows session implementation.

This native invocation has a different interface from the Linux/macOS runner.
`--config-env` and `--bootstrap-fd` fail before launching a target on Windows.
The native launcher consumes a serialized `PermissionProfile`, not the versioned
request in [PROTOCOL.md](PROTOCOL.md).

## Build

Install Rust 1.95.0 with the `x86_64-pc-windows-msvc` toolchain, Visual Studio C++
build tools, a Windows SDK, and CMake. From `codex-rs`:

```powershell
cargo build --locked --release -p codex-mcp-console-sandbox --bin mcp-console-sandbox
```

The artifact is `target/release/mcp-console-sandbox.exe`. The restricted-token
backend runs from this one executable without adjacent sandbox helper binaries.
It creates capability and log state under the caller-supplied `--codex-home`.
Bazel's corresponding build target is
`//codex-rs/mcp-console-sandbox:mcp-console-sandbox`.

## Native invocation

This Python example avoids differences in JSON argument quoting between
Windows PowerShell 5 and PowerShell 7. Run it from `codex-rs` after building:

```python
import json
import os
from pathlib import Path
import subprocess

runner = Path("target/release/mcp-console-sandbox.exe").resolve()
root = Path("target/windows-sandbox-example").resolve()
workspace = root / "workspace"
workspace.mkdir(parents=True, exist_ok=True)
profile = {
    "type": "managed",
    "file_system": {
        "type": "restricted",
        "entries": [
            {"path": {"type": "special", "value": {"kind": "root"}},
             "access": "read"},
            {"path": {"type": "path", "path": str(workspace)},
             "access": "write"},
        ],
    },
    "network": "enabled",
}
result = subprocess.run([
    str(runner), "--run-as-windows-sandbox",
    "--codex-home", str(root / "state"),
    "--command-cwd", str(workspace),
    "--permission-profile", json.dumps(profile),
    "--env-json", json.dumps({"SystemRoot": os.environ["SystemRoot"]}),
    "--windows-sandbox-level", "restricted-token",
    "--", str(Path(os.environ["SystemRoot"]) / "System32/cmd.exe"),
    "/d", "/c", "echo sandbox works>result.txt&type result.txt",
], check=True)
print("exit:", result.returncode)
```

Omit the write entry for read-only execution. Explicit workspace roots can also
be passed with repeated `--workspace-root` flags. Native Windows argument parsing
and permission validation remain owned by `windows-sandbox-rs/src/wrapper.rs`.
An empty read allowlist, external enforcement, and unrestricted managed
filesystem policies are not supported by this restricted-token path.

The example leaves its workspace and sandbox state under
`target/windows-sandbox-example`. The target still needs host read/traverse
permission. On the tested machine, Python 3.14's private temporary directory
granted access only through owner/admin/system ACL entries, and the restricted
token could not access its workspace. An ordinary directory inheriting the
current user's access worked; no machine-wide ACL changes were needed.
Use native Windows executable paths with backslashes for `cmd.exe`; its command
line parsing does not reliably accept a mixed-separator executable path.

Targets remain subject to host Application Control policy. On this machine,
that policy blocked a freshly compiled unsigned fixture with Windows error 4551.
The local smoke tests used the signed system `cmd.exe`; no security policy was
disabled to run them.

The restricted-token backend applies write restrictions with Windows tokens and
ACLs. It does not provide a read allowlist security boundary. Its restricted
network setting changes proxy and tool environment variables; it is not an
OS-enforced network isolation boundary. The example deliberately uses
`"network": "enabled"`.

Lifecycle behavior also comes from the upstream Windows session: Ctrl+C requests
termination, but normal root-process exit can preserve descendants. The native
mode does not promise the Unix runner's retire-all-descendants behavior, private
temporary-directory cleanup, or caller-death monitoring.

The upstream `elevated` backend supports provisioned sandbox identities and
firewall/WFP enforcement. It needs setup state and companion helpers, including
`codex-command-runner.exe` and `codex-windows-sandbox-setup.exe`. Those can be built
with `cargo build --locked --release -p codex-windows-sandbox --bins`. This adapter
does not provision accounts, install a service, configure firewall rules, or
establish that the elevated deployment works. Managed proxy enforcement requires
that backend and correctly prepared proxy identity/settings.

## Work needed for the shared runner contract

The original branch already compiled on Windows, but its Windows `main` only
reported an unsupported platform. Removing the platform gates would not port it:

| Area | Required Windows work |
| --- | --- |
| Request transport | Separate request validation from Unix descriptor I/O in `bootstrap.rs`; implement `--config-env` and a bounded inherited-handle or named-pipe transport. Keep configuration and excluded environment values out of the target. |
| Policy adaptation | Translate `profiles.rs` results into Windows session requests. Explicitly reject unrepresentable filesystem/read-deny policies and choose the backend needed for network enforcement. |
| Managed proxy | Connect the existing network proxy lifecycle to the elevated Windows backend, provisioning, loopback permissions, and restricting SID. |
| Launch and lifecycle | Replace Unix sockets, `fcntl`, process groups, signals, `waitpid`, and native setup handshakes with Windows handles, Job Objects, cancellation, and a target-release handshake. Define parent death and runner loss behavior and verify descendant retirement. |
| Private temporary storage | Use Windows ACLs and reparse-point-safe ownership/cleanup; establish deletion ordering after every target descendant exits. |
| Packaging | Decide whether provisioned accounts/helpers are acceptable or whether helper dispatch and setup should be embedded. Define the required first-run setup and state directory. |
| Validation | Add Windows equivalents of the transport, policy, network, startup, terminal, and lifecycle suites; add a Windows job to the focused release workflow. |

Local, uncommitted smoke tests covered explicit mode selection, stdio and exit
propagation, read-only write denial, selected writable roots, and policy rejection
before launch. These provisional tests are not included in this commit. They do
not establish parity with the Linux/macOS lifecycle contract or validate the
elevated backend. A complete shared-protocol
port is a separate implementation project, not a build-configuration fix.

## Local validation

Validated on Windows x64 with Rust 1.95.0, based on fork revision `2d0ad79721`:

- The original `cargo build --locked --release` succeeded but produced the
  unsupported-platform stub.
- With the adapter, the release executable built and the example above printed
  `sandbox works` with exit code 0.
- The local, uncommitted smoke suite passed all five Windows tests with none
  skipped, using `just test --release -p codex-mcp-console-sandbox --retries 0
  --test-threads 1`.
- `dumpbin /dependents` reported only Windows system DLLs.
- `just bazel-lock-update` succeeded without changing `MODULE.bazel.lock`;
  `bazel query` resolved the local, uncommitted Windows test target. A full
  Bazel build was not run.

Linux/macOS tests and the elevated Windows backend were not run in this local
Windows validation.
