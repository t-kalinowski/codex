# Windows build and port assessment

The Windows executable reuses the existing Windows sandbox implementation without
`codex-core` or a running Codex application. `setup` provisions Console sandbox
accounts, `status` checks setup records and account availability, and `run`
forwards stdin, stdout, stderr, and the target exit code through the Windows
session implementation.

The Windows `run` command consumes native Windows options and a serialized
`PermissionProfile`, not the versioned request in [PROTOCOL.md](PROTOCOL.md).
`--config-env` and `--bootstrap-fd` remain unsupported.

## Build

Install Rust 1.95.0 with the `x86_64-pc-windows-msvc` toolchain, Visual Studio C++
build tools, a Windows SDK, and CMake. From `codex-rs`:

```powershell
cargo build --locked --release -p codex-mcp-console-sandbox -p codex-windows-sandbox --bin mcp-console-sandbox --bin mcp-console-sandbox-setup --bin mcp-console-sandbox-runner
just test --release -p codex-mcp-console-sandbox --retries 0
```

Distribute these three files from `target/release` together:

- `mcp-console-sandbox.exe`
- `mcp-console-sandbox-setup.exe`
- `mcp-console-sandbox-runner.exe`

The two helpers compile the existing setup and command-runner implementations
through small Console entrypoints. They use the same manifests and security code
as the original helpers. The restricted-token backend needs only the main
executable; the elevated backend requires all three.

Bazel targets are `//codex-rs/mcp-console-sandbox:mcp-console-sandbox`,
`//codex-rs/windows-sandbox-rs:mcp-console-sandbox-setup`, and
`//codex-rs/windows-sandbox-rs:mcp-console-sandbox-runner`.

## Installation and status

Run from an ordinary, non-administrator PowerShell session:

```powershell
.\target\release\mcp-console-sandbox.exe setup
.\target\release\mcp-console-sandbox.exe status
```

`setup` requests administrator approval through Windows UAC when provisioning is
needed. Repeating it reuses current setup records and enabled accounts. `status`
prints JSON and exits with code 0 when current setup records, enabled accounts,
and adjacent helpers are present, or 1 when setup is incomplete. This is a
readiness check, not an audit of all installed firewall rules.

State defaults to `%LOCALAPPDATA%\mcp-console`. Each command accepts
`--state-dir` with an absolute path. Use one stable state directory per Windows
user; accounts and network policy are machine resources, so separate state
directories are not independent installations. No provisioning service is needed.

The display names are **Console Sandbox Offline** and **Console Sandbox Online**.
Their login names are `ConsoleSandboxOff` and `ConsoleSandboxOn` because Windows
limits local account names to 20 characters. Setup uses `ConsoleSandboxUsers` and
separate Console firewall rules and WFP identifiers. Existing Codex resources keep
their names and identifiers. A state directory containing another product's
account records is rejected. There is no automatic migration of existing state.

## Native invocation

After setup, this Python example avoids differences in JSON argument quoting between
Windows PowerShell 5 and PowerShell 7. Run it from `codex-rs` after building:

```python
import json
import os
from pathlib import Path
import subprocess

runner = Path("target/release/mcp-console-sandbox.exe").resolve()
root = Path("target/windows-sandbox-example").resolve()
state = Path(os.environ["LOCALAPPDATA"]) / "mcp-console"
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
    str(runner), "run",
    "--state-dir", str(state),
    "--command-cwd", str(workspace),
    "--permission-profile", json.dumps(profile),
    "--env-json", json.dumps({"SystemRoot": os.environ["SystemRoot"]}),
    "--windows-sandbox-level", "elevated",
    "--", str(Path(os.environ["SystemRoot"]) / "System32/cmd.exe"),
    "/d", "/c", "echo sandbox works>result.txt&type result.txt",
], check=True)
print("exit:", result.returncode)
```

Omit the write entry for read-only execution. Explicit workspace roots can also
be passed with repeated `--workspace-root` flags. Native Windows argument parsing
and permission validation remain owned by `windows-sandbox-rs/src/wrapper.rs`.
The default backend is `elevated`. Pass `--windows-sandbox-level restricted-token`
to use the limited backend without account provisioning. An empty read allowlist,
external enforcement, and unrestricted managed filesystem policies are not
supported by the restricted-token path. The previous `--run-as-windows-sandbox`
invocation and `--codex-home` flag remain accepted as compatibility aliases.

The example leaves its workspace under `target/windows-sandbox-example` and
its persistent sandbox state under `%LOCALAPPDATA%\mcp-console`.
The target still needs host read/traverse
permission. On the tested machine, Python 3.14's private temporary directory
granted access only through owner/admin/system ACL entries, and the restricted
token could not access its workspace. An ordinary directory inheriting the
current user's access worked; no machine-wide ACL changes were needed.
Use native Windows executable paths with backslashes for `cmd.exe`; its command
line parsing does not reliably accept a mixed-separator executable path.

Targets remain subject to host Application Control policy. On this machine,
that policy blocked a freshly compiled unsigned fixture with Windows error 4551.
The executable tests use the signed system `cmd.exe`; no security policy was
disabled to run them.

The restricted-token backend applies write restrictions with Windows tokens and
ACLs. It does not provide a read allowlist security boundary. Its restricted
network setting changes proxy and tool environment variables; it is not an
OS-enforced network isolation boundary. The example uses `"network": "enabled"`, selecting the online account. Use
`"network": "restricted"` with the elevated backend for the offline account.

Lifecycle behavior also comes from the upstream Windows session: Ctrl+C requests
termination, but normal root-process exit can preserve descendants. The native
mode does not promise the Unix runner's retire-all-descendants behavior, private
temporary-directory cleanup, or caller-death monitoring.

## Setup and persistent state

A SID (security identifier) is the identifier Windows uses in access tokens and
filesystem permission entries. The restricted-token backend creates capability
SIDs and saves their workspace/path mappings in `cap_sid`; these identifiers do
not require new Windows user accounts. It initializes this state and applies
workspace ACLs during launch, so it needs no separate installation or administrator
setup step when the caller can modify the relevant permissions. Logs live in
`.sandbox`. ACL entries also persist on the actual filesystem objects; deleting
the state directory does not remove them. Reuse the same state directory across
launches to preserve those mappings.

The elevated backend provisions accounts, network restrictions, protected
credentials in `.sandbox-secrets`, and versioned setup records in `.sandbox`.
It copies its runner into `.sandbox-bin` as needed. Ordinary launches refresh
workspace ACLs without elevation. Missing accounts, upgrades, or changed network
settings can require administrator setup again; the shared backend handles this
repair path. Managed proxy enforcement still requires correctly prepared proxy
identity/settings and is not configured by the standalone `setup` command.

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
| Packaging | Decide whether provisioned accounts/helpers are acceptable or whether helper dispatch and setup should be embedded. The Windows bundle now has setup/status commands and a default state directory; single-file elevated packaging remains future work. |
| Validation | Add Windows equivalents of the transport, policy, network, startup, terminal, and lifecycle suites; add a Windows job to the focused release workflow. |

The `windows_native` executable tests cover default state-directory selection
without creating state and rejection of another product's account records before
setup or launch. Five additional local prototype tests cover mode selection,
stdio and exit propagation, write restrictions, and policy rejection. They are
not included in this change. These checks do not establish parity with the
Linux/macOS lifecycle contract or validate the elevated backend. A complete
shared-protocol port is a separate implementation project.

## Local validation

Validated on Windows x64 with Rust 1.95.0:

- All three release executables built successfully.
- All seven standalone Windows tests (including five local prototypes) passed,
  covering state-directory selection
  and rejection of another product's account records before setup or launch.
- The shared Windows suite initially reported 190 passes, two failures, one
  timeout, and three skipped tests. Its elevated integration test requires UAC
  approval to provision **Codex** accounts and was not approved. Its Python
  descendant-cleanup test passed when the actual Python executable directory was
  placed before the Windows app alias in PATH. Its batch deletion test supplies
  no `SystemRoot`; a separate reproduction confirmed that `cmd.exe` silently
  fails to execute a batch file without it on this machine and succeeds with it.
- All 19 setup-helper tests passed after the final helper changes.
- The Console helper rejected a payload naming Codex accounts before creating
  any setup state. Existing Codex account identifiers, enabled flags, and
  password timestamps were unchanged.
- `just bazel-lock-update` succeeded without changing `MODULE.bazel.lock`;
  `bazel query` resolved both new helper targets. A full Bazel build was not run.

Console account provisioning and elevated execution still require an interactive
administrator approval and have not been validated here. Linux/macOS tests were
not run in this local Windows validation. The shared-protocol and lifecycle
limitations described above still apply.
