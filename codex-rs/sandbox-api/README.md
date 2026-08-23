# Codex sandbox embedding API

`codex-sandbox-api` is a small, platform-agnostic facade over the sandbox implementations in this Codex fork. It lets another application launch a non-PTY child with an explicit filesystem policy, direct-network policy, complete environment, and separate raw standard streams.

This is a fork-maintained embedding interface, not an upstream-supported Codex API. The facade isolates consumers from Codex application, protocol, session, approval, model, MCP, app-server, and CLI types. Creating a runtime does not load Codex configuration, authenticate, start a Codex session, or read the user's normal `CODEX_HOME`.

## Dependency

Pin production builds to an immutable commit from the fork:

```toml
[dependencies]
codex-sandbox-api = { git = "https://github.com/<fork-owner>/codex", rev = "<immutable-commit-sha>", package = "codex-sandbox-api" }
tokio = { version = "1", features = ["io-util", "rt-multi-thread"] }
```

Do not depend on a rolling patch branch in a production build.

## Backends and capabilities

`BackendPreference::PlatformDefault` selects one native backend and returns an error if that backend cannot be prepared. There is no unsandboxed backend or fallback.

| Platform | Selected backend                             | Minimal reads | Read denies | Write restrictions | Network denial     | Interrupt | Process-tree termination              |
| -------- | -------------------------------------------- | ------------- | ----------- | ------------------ | ------------------ | --------- | ------------------------------------- |
| macOS    | Seatbelt                                     | Yes           | Yes         | Yes                | Yes                | Yes       | No; group signals require a live root |
| Linux    | bubblewrap with the Codex helper and seccomp | Yes           | Yes         | Yes                | x86_64 and aarch64 | Yes       | No; group signals require a live root |
| Windows  | restricted token with a job object           | No            | No          | Yes                | No                 | No        | Yes, by job object                    |

All three backends support unrestricted direct network access. Query `SandboxRuntime::capabilities()` instead of inferring support from the target OS. A policy that requests an unsupported feature returns `SandboxError::UnsupportedPolicy` before the target command starts.

The current Linux path selects bubblewrap. `SandboxBackend::LinuxLandlock` is reserved for a release where the Codex native default selects the retained Landlock path; this version does not select it. The Windows facade uses the unelevated restricted-token backend. It does not use the elevated managed network backend.

## Policy semantics

`FileSystemBase::PlatformMinimal` permits the platform and runtime roots needed to launch ordinary programs, then applies explicit rules. It is supported by the macOS and Linux backends. It is not silently widened to host reads on Windows.

`FileSystemBase::HostReadOnly` permits host filesystem reads and limits writes to paths granted with `PathAccess::Write`, apart from native runtime mounts the backend must keep writable. Linux always supplies a fresh writable `/dev` and a fresh `/proc`; macOS retains required device access. A conflicting explicit rule is rejected instead of being silently overridden. `HostReadOnly` is the required base for the current Windows backend.

Rules apply to an absolute path and its descendants:

- `Read` permits reads without permitting writes.
- `Write` permits reads and writes.
- `Deny` denies reads and writes.

Rule paths must be absolute. By default, every path must exist while the policy is prepared. `PathRule::ignore_if_missing()` omits a missing rule instead. The facade never substitutes a writable parent for a missing writable root.

More-specific rules follow the existing Codex backend semantics. Some nested combinations cannot be represented by a native backend and are rejected. Seatbelt rejects nested allows under a deny and a deny at filesystem root. Bubblewrap rejects a read allow under a deny, any explicit rule overlapping its fresh `/proc`, read-only rules overlapping its writable `/dev`, and read/deny paths that cross a symlink the child could replace. The Windows backend rejects a writable child reopened below a read-only carveout of a broader writable root. Explicit deny rules are never discarded.

Canonicalization and symlink treatment otherwise come from the selected Codex sandbox implementation; the facade does not define a competing path-security model. Observable behavior can differ where the operating systems resolve links differently. Rule paths and the command working directory must be absolute. The command program must also be an absolute path; the facade never searches `PATH` for it.

Only `NetworkPolicy::Denied` and `NetworkPolicy::Unrestricted` are supported. There is no destination-filtered or proxy-only mode. If direct network denial is unavailable, `Denied` is rejected rather than changed to `Unrestricted`.

For example:

```rust
let policy = SandboxPolicy::platform_minimal()
    .read_only(runtime_root)
    .read_write(cache_root)
    .deny(home_directory)
    .network_denied();
```

## Linux helper bootstrap

Linux enters the sandbox through the existing Codex Linux helper. Choose one of these packaging modes through `SandboxRuntimeConfig::linux.helper`:

- `LinuxHelper::External(path)` uses an explicitly installed or vendored executable built from the same immutable Codex commit as the facade. The path must be absolute, valid UTF-8, and executable.
- `LinuxHelper::CurrentExecutable` re-executes the embedding application through a private `codex-linux-sandbox` alias. This is the default.

With `CurrentExecutable`, the embedding binary must call the dispatch hook before it creates threads or a Tokio runtime:

```rust
fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    codex_sandbox_api::dispatch_embedded_helper();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run())
}
```

Normal startup returns immediately. When the executable is invoked through the reserved alias, the hook dispatches to the existing Codex Linux helper and does not return. Constructing a `CurrentExecutable` runtime without first calling the hook is an error.

The runtime also selects one compatible bubblewrap executable. It first checks the Codex resource layouts adjacent to the resolved helper executable: `codex-resources/bwrap` beside or one level above the helper, then `bwrap` beside it. If no packaged copy is usable, it checks the embedding application's startup `PATH`, excluding candidates below the application's working directory. An `External` deployment must therefore package the matching helper and bubblewrap resources together, or provide a compatible system `bwrap` on the application's startup `PATH`. The selected path and launcher kind are pinned when the runtime is created and revalidated for each spawn; a child-supplied `PATH` cannot replace them.

Before the outer helper executes, Linux marks every inherited descriptor above standard error as close-on-exec with `close_range(CLOSE_RANGE_CLOEXEC)`. If the kernel cannot perform that operation, spawning fails before the helper or target runs. This prevents an application-owned file or socket descriptor from bypassing the requested filesystem or network policy.

`SandboxRuntime` creates a private helper directory and synthetic-mount registry below the caller's `state_dir`, grants those paths the reads needed by the outer sandbox, and retains them for the lifetime of all children and moved stream handles. Keep `state_dir` outside target writable and denied roots. A policy that would make backend bookkeeping writable or unreadable is rejected. A missing or unusable helper or bubblewrap executable is an error; execution never falls back to the host.

## Windows state directory

The restricted-token backend uses the caller's `state_dir` for per-spawn capability SID and runtime state. The facade does not call `find_codex_home()`, write sandbox logs, or initialize the user's normal Codex home. Keep `state_dir` outside target writable roots. Windows launch preparation preserves the supplied environment exactly and treats ACL preparation failures as fatal. The embedding-only token admits writes through the generated capability SIDs, not through broad host groups such as Everyone. Temporary capability ACEs are serialized across embedding sessions and removed after the child and all stream handles release the native session.

The current Windows backend supports `HostReadOnly`, explicit writable roots, and read-only subpaths below writable roots. It rejects `PlatformMinimal`, read deny rules, direct network denial, and a writable child below a read-only carveout of a broader writable root, including carveouts reached through a directory junction or symlink. It launches on a private desktop and uses a non-breakaway job object for termination of the process tree.

## Streams and lifecycle

`SandboxedChild` exposes stdin, stdout, and stderr as separate raw-byte streams. Each stream can move independently into a Tokio task. `take_stdin()` returns `None` unless the request used `stdin_open()`. Each `take_*` method succeeds at most once. The facade does not decode UTF-8, merge output streams, or buffer a complete process output; callers should drain stdout and stderr concurrently.

macOS uses Codex's fork-safe descriptor sweep before `sandbox-exec`. If its fixed descriptor buffer cannot prove that enumeration was complete, spawning fails before `sandbox-exec` or the target runs.

`wait()` returns a normalized `SandboxExitStatus`. Ordinary exit codes remain ordinary codes. Unix signal termination remains a signal and is not converted to `128 + signal`. `try_status()` is a best-effort nonblocking inspection; `None` means a completed status is not currently available.

On macOS and Linux, `interrupt()` sends `SIGINT` to the child process group and `terminate()` kills that group while the root process is still running. The facade does not retain authority over Unix descendants after the root exits, so these backends report `process_tree_termination: false`. On Windows, `interrupt()` returns `UnsupportedPolicy`, while `terminate()` terminates the restricted process job. The runtime is retained internally until the child and all moved stream handles are released. The embedding application still owns higher-level restart, timeout, and service supervision.

## End-to-end example

This example launches the service executable supplied as the first argument, opens stdin, drains stdout and stderr independently, and writes the exact bytes back to the host streams. It uses the portable policy supported by all three current backends: host reads, one writable directory, and unrestricted network.

```rust
use codex_sandbox_api::CommandSpec;
use codex_sandbox_api::SandboxError;
use codex_sandbox_api::SandboxPolicy;
use codex_sandbox_api::SandboxRequest;
use codex_sandbox_api::SandboxRuntime;
use codex_sandbox_api::SandboxRuntimeConfig;
use codex_sandbox_api::SandboxedOutput;
use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::OsString;
use std::io;
use std::io::Write;
use std::path::PathBuf;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

fn main() -> Result<()> {
    // Required for LinuxHelper::CurrentExecutable; a no-op on other platforms.
    codex_sandbox_api::dispatch_embedded_helper();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    let service = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("pass the service executable as argument 1"))?
        .canonicalize()?;
    let app_root = std::env::current_dir()?.canonicalize()?;
    let state_dir = app_root.join(".sandbox-state");
    let cache_dir = app_root.join(".sandbox-cache");
    std::fs::create_dir_all(&cache_dir)?;

    let runtime = SandboxRuntime::new(SandboxRuntimeConfig::new(state_dir))?;

    // This is the complete child environment. The facade does not merge it
    // with either the host environment or Codex configuration.
    // Linux rejects LD_* because those keys would affect the outer helper
    // before isolation. macOS rejects DYLD_* because sandbox-exec strips them.
    let child_env = std::env::vars_os()
        .filter(|(key, _)| {
            let key = key.as_encoded_bytes();
            !(cfg!(target_os = "linux") && key.starts_with(b"LD_"))
                && !(cfg!(target_os = "macos") && key.starts_with(b"DYLD_"))
        })
        .collect::<BTreeMap<OsString, OsString>>();
    let command = CommandSpec::new(service, &app_root, child_env).arg("--stdio");
    let policy = SandboxPolicy::host_read_only()
        .read_write(&cache_dir)
        .network_unrestricted();
    let request = SandboxRequest::new(command, policy).stdin_open();

    let mut child = runtime.spawn(request).await?;
    let stdout_task = tokio::spawn(collect(
        child.take_stdout().expect("stdout is always piped"),
    ));
    let stderr_task = tokio::spawn(collect(
        child.take_stderr().expect("stderr is always piped"),
    ));

    let mut stdin = child.take_stdin().expect("stdin was requested open");
    stdin.write_all(b"request\0\xff\n").await?;
    stdin.close().await?;

    let status = child.wait().await?;
    let stdout = stdout_task.await??;
    let stderr = stderr_task.await??;
    std::io::stdout().write_all(&stdout)?;
    std::io::stderr().write_all(&stderr)?;

    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "service exited with code {:?}, signal {:?}",
            status.code(),
            status.signal()
        ))
        .into())
    }
}

async fn collect(mut output: SandboxedOutput) -> std::result::Result<Vec<u8>, SandboxError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = output.read_chunk().await? {
        bytes.extend(chunk);
    }
    Ok(bytes)
}
```

## Current limits

- Only the native platform default can be selected.
- PTYs, approval, escalation, sessions, persistent output buffering, and an external command protocol are outside this facade.
- The network policy is binary: denied or unrestricted.
- Linux direct network denial depends on the helper's seccomp support and is currently reported only on x86_64 and aarch64.
- Nested allow-under-deny policies can be rejected when the native backend cannot express them without widening access.
- The program and all policy paths must be absolute. The facade does not use the child environment to resolve a bare program name.
- Linux command components and Windows command and environment components must be valid UTF-8. All current backend policy paths and command working directories must be valid UTF-8. Invalid data is rejected without lossy conversion.
- Linux rejects child environment keys beginning with `LD_`; macOS rejects keys beginning with `DYLD_`. Accepted environment entries otherwise replace the inherited environment exactly.
- Linux requires the kernel `close_range` operation with close-on-exec support; an unavailable operation rejects the spawn.
- macOS rejects a spawn when its 1,024-record fork-safe descriptor sweep cannot enumerate the complete open-descriptor table.
- Windows does not currently support minimal reads, read deny rules, direct network denial, interrupts, or write-read-write nested carveouts.
- Process-tree termination is a backend capability, not a promise of durable service supervision after the embedding application exits.

See [REBASE.md](REBASE.md) for the public compatibility contract and the release-refresh procedure.
