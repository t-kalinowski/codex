# Rolling-release maintenance

This crate is a rolling patch over an exact Codex release. Reimplement it against each new release's native sandbox internals instead of merging the old patch branch.

## Patch identity

- `SANDBOX_API_VERSION`: `1`
- Upstream base SHA: `2161ec272a7d6b775c9c721e6206f4fe63e383f2`
- Upstream release tag: none found at the exact base commit
- Cargo package: `codex-sandbox-api`
- Rust library: `codex_sandbox_api`

Keep `SANDBOX_API_VERSION` at `1` while the downstream contract below remains source compatible. Change it only for an intentional downstream API break.

## Public compatibility contract

The public surface is owned by this crate. It must not expose `codex_protocol`, `PathUri`, `NetworkProxy`, `WindowsSandboxLevel`, or other Codex application and protocol types.

The following signature inventory records the version 1 contract; it is not a standalone Rust module. Its constants, trait implementations, types, variants, fields, constructors, methods, and `#[non_exhaustive]` markers must remain source compatible.

```rust
pub const SANDBOX_API_VERSION: u32 = 1;

pub struct SandboxRuntime { /* opaque, Send + Sync */ }

impl SandboxRuntime {
    pub fn new(config: SandboxRuntimeConfig) -> Result<Self, SandboxError>;
    pub fn capabilities(&self) -> SandboxCapabilities;
    pub async fn spawn(
        &self,
        request: SandboxRequest,
    ) -> Result<SandboxedChild, SandboxError>;
}

pub fn dispatch_embedded_helper();

#[non_exhaustive]
pub enum BackendPreference {
    PlatformDefault,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SandboxBackend {
    MacosSeatbelt,
    LinuxBubblewrap,
    LinuxLandlock,
    WindowsRestrictedToken,
    WindowsElevated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SandboxCapabilities {
    pub backend: SandboxBackend,
    pub minimal_read_policy: bool,
    pub denied_read_paths: bool,
    pub denied_write_paths: bool,
    pub network_denial: bool,
    pub network_unrestricted: bool,
    pub interrupt: bool,
    pub process_tree_termination: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxRuntimeConfig {
    pub state_dir: PathBuf,
    pub backend: BackendPreference,
    #[cfg(target_os = "linux")]
    pub linux: LinuxOptions,
    #[cfg(target_os = "windows")]
    pub windows: WindowsOptions,
}

impl SandboxRuntimeConfig {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self;
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LinuxHelper {
    External(PathBuf),
    CurrentExecutable,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinuxOptions {
    pub helper: LinuxHelper,
}

#[cfg(target_os = "linux")]
impl Default for LinuxOptions;

#[cfg(target_os = "windows")]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct WindowsOptions {}

#[cfg(target_os = "windows")]
impl Default for WindowsOptions;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
}

impl CommandSpec {
    pub fn new(
        program: impl Into<OsString>,
        cwd: impl Into<PathBuf>,
        env: BTreeMap<OsString, OsString>,
    ) -> Self;
    pub fn arg(self, arg: impl Into<OsString>) -> Self;
    pub fn args(
        self,
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FileSystemBase {
    PlatformMinimal,
    HostReadOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathAccess {
    Read,
    Write,
    Deny,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MissingPathBehavior {
    Error,
    Ignore,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathRule {
    pub path: PathBuf,
    pub access: PathAccess,
    pub missing: MissingPathBehavior,
}

impl PathRule {
    pub fn new(path: impl Into<PathBuf>, access: PathAccess) -> Self;
    pub fn ignore_if_missing(self) -> Self;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileSystemPolicy {
    pub base: FileSystemBase,
    pub rules: Vec<PathRule>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NetworkPolicy {
    Denied,
    Unrestricted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxPolicy {
    pub filesystem: FileSystemPolicy,
    pub network: NetworkPolicy,
}

impl SandboxPolicy {
    pub fn platform_minimal() -> Self;
    pub fn host_read_only() -> Self;
    pub fn rule(self, rule: PathRule) -> Self;
    pub fn read_only(self, path: impl Into<PathBuf>) -> Self;
    pub fn read_write(self, path: impl Into<PathBuf>) -> Self;
    pub fn deny(self, path: impl Into<PathBuf>) -> Self;
    pub fn network_denied(self) -> Self;
    pub fn network_unrestricted(self) -> Self;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxRequest {
    pub command: CommandSpec,
    pub policy: SandboxPolicy,
    pub stdin_open: bool,
}

impl SandboxRequest {
    pub fn new(command: CommandSpec, policy: SandboxPolicy) -> Self;
    pub fn stdin_open(self) -> Self;
    pub fn stdin_closed(self) -> Self;
}

pub struct SandboxedChild { /* opaque, Send */ }
pub struct SandboxedStdin { /* opaque, Send */ }
pub struct SandboxedOutput { /* opaque, Send */ }

impl SandboxedChild {
    pub fn take_stdin(&mut self) -> Option<SandboxedStdin>;
    pub fn take_stdout(&mut self) -> Option<SandboxedOutput>;
    pub fn take_stderr(&mut self) -> Option<SandboxedOutput>;
    pub async fn wait(&mut self) -> Result<SandboxExitStatus, SandboxError>;
    pub fn try_status(&self) -> Option<SandboxExitStatus>;
    pub fn interrupt(&self) -> Result<(), SandboxError>;
    pub fn terminate(&self) -> Result<(), SandboxError>;
    pub fn backend(&self) -> SandboxBackend;
}

impl SandboxedStdin {
    pub async fn write_all(&mut self, bytes: &[u8]) -> Result<(), SandboxError>;
    pub async fn close(self) -> Result<(), SandboxError>;
}

impl SandboxedOutput {
    pub async fn read_chunk(&mut self) -> Result<Option<Vec<u8>>, SandboxError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SandboxExitStatus { /* opaque */ }

impl SandboxExitStatus {
    pub fn code(self) -> Option<i32>;
    pub fn signal(self) -> Option<i32>;
    pub fn success(self) -> bool;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SandboxFeature {
    MinimalReadPolicy,
    DeniedReadPaths,
    DeniedWritePaths,
    NestedAllowUnderDeny,
    NetworkDenial,
    NetworkUnrestricted,
    Interrupt,
    ProcessTreeTermination,
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SandboxError {
    UnsupportedPlatform {
        platform: String,
    },
    BackendUnavailable {
        backend: Option<SandboxBackend>,
        message: String,
    },
    UnsupportedPolicy {
        backend: SandboxBackend,
        feature: SandboxFeature,
        message: String,
    },
    InvalidCommand {
        message: String,
    },
    InvalidPath {
        path: PathBuf,
        message: String,
    },
    Preparation {
        backend: SandboxBackend,
        message: String,
        source: Option<Box<dyn Error + Send + Sync>>,
    },
    Spawn {
        backend: SandboxBackend,
        message: String,
        source: Option<Box<dyn Error + Send + Sync>>,
    },
    Io {
        operation: &'static str,
        source: io::Error,
    },
}
```

The behavioral contract is also part of version 1:

- The command environment is complete and replaces inherited values.
- The program path, working directory, and policy paths are absolute. Program lookup through `PATH` is not part of the contract.
- Arguments and all three streams preserve raw bytes. A backend that requires UTF-8 rejects non-UTF-8 input without lossy conversion.
- Nonstandard inherited file descriptors do not reach the target. Linux uses `close_range(CLOSE_RANGE_CLOEXEC)` and rejects the spawn if it is unavailable. macOS rejects the spawn when Codex's fixed fork-safe descriptor enumeration cannot prove that the complete table was inspected.
- Linux rejects environment keys beginning with `LD_`; macOS rejects keys beginning with `DYLD_`. Accepted entries are otherwise preserved exactly.
- Standard output and standard error remain separate and independently movable.
- Unsupported policies and backend preparation errors occur before the target command runs. There is no unsandboxed fallback.
- Missing path behavior is deterministic. A missing writable root is never replaced by a writable parent.
- Unix exits preserve native signal status. Windows reports the ordinary exit code exposed by its native transport.
- The selected helper or native session and application state remain alive while any child or stream handle needs them.
- `SandboxRuntime` is `Send + Sync`; `SandboxedChild`, `SandboxedStdin`, and `SandboxedOutput` are `Send`.

`tests/public_contract.rs` imports only `codex_sandbox_api` and is the source compatibility test. `tests/runtime_contract.rs` covers the public process and policy behavior against a real backend where the host permits it.

## Internal Codex APIs adapted

The facade currently adapts these private dependencies:

- `codex_sandboxing::seatbelt::create_seatbelt_command_args`, `CreateSeatbeltCommandArgsParams`, and `MACOS_PATH_TO_SEATBELT_EXECUTABLE` generate and launch the existing macOS Seatbelt policy. `restricted_platform_defaults_overlap_writes` exposes the smallest policy-compatibility query needed for fail-closed translation.
- `codex_sandboxing::landlock::create_linux_sandbox_command_args_for_permission_profile` and `CODEX_LINUX_SANDBOX_ARG0` construct the current Linux helper invocation.
- `codex_sandboxing::{bwrap_has_namespace_access, find_system_bwrap_in_search_path}` validate application-selected bubblewrap without consulting a target command's environment.
- `codex_linux_sandbox::run_main` dispatches the embedded Linux helper. `prepare_embedding_bwrap` pins the packaged or system bubblewrap launcher; `first_writable_symlink_component_in_path` and `read_rule_overlaps_implicit_writable_dev` expose narrow fail-closed policy checks already owned by the Linux backend.
- `codex_protocol::permissions` filesystem and network policy types and `codex_protocol::models::PermissionProfile` are private translation targets. None appears in a public facade signature.
- `codex_utils_absolute_path::AbsolutePathBuf` is used only at Codex-internal path seams.
- `codex_utils_pty::process_group` supplies Unix process-group setup, interrupt, and termination. The facade owns `tokio::process::Child` directly on Unix so native signal status is retained.
- `codex_utils_pty::pty::close_inherited_fds_except_checked` supplies the fail-closed macOS descriptor sweep before `sandbox-exec`.
- `codex_utils_pty::JobObject` supplies non-breakaway Windows process-tree termination.
- `codex_windows_sandbox::spawn_windows_sandbox_session_for_embedding`, `WindowsSandboxEmbeddingRequest`, and the opaque embedding process handle form the native Windows seam. That seam reuses Codex capability SID, ACL, restricted-token, private-desktop, and process-launch primitives while preserving exact split streams and an application-owned state directory.

Reinspect every symbol on a release refresh. Preserve behavior through the facade rather than preserving a particular Codex-internal type or call graph.

## Linux helper packaging

`LinuxHelper::External` validates and retains an absolute helper executable built from the same immutable patch commit. `LinuxHelper::CurrentExecutable` creates a private symlink named `codex-linux-sandbox` in a temporary directory below the application-owned `state_dir`. The runtime retains that directory, the alias, the synthetic-mount registry, and the resolved current executable for every child and moved stream handle. The private directory is inserted as an internal read root before policy translation; policies that would make any of its effective descendants writable or unreadable are rejected.

The embedding binary calls `dispatch_embedded_helper()` at the beginning of its synchronous `main`, before threads or an async runtime. The function returns on ordinary startup. When `argv[0]` has the reserved helper basename, it calls `codex_linux_sandbox::run_main` and does not return. An absent helper, unusable alias, denied helper path, or helper preparation error stops the spawn.

At runtime construction, the facade first searches the helper's supported Codex resource layouts for a bundled `bwrap`, then searches the embedding application's startup `PATH` while excluding candidates below its working directory. The exact path and system/bundled launcher kind are pinned and revalidated on every spawn. The helper receives those values and the private registry through hidden embedding-only arguments, so the target environment cannot replace the launcher or redirect helper bookkeeping. `External` packaging must ship a helper and matching resource layout from the same commit, or arrange a compatible system `bwrap` on application startup `PATH`.

The current helper arguments select bubblewrap rather than legacy Landlock. The helper mounts a fresh `/proc` and writable `/dev`; conflicting explicit rules fail during facade policy preparation. Network denial is reported only on Linux x86_64 and aarch64, and only when the pinned launcher can create the network namespace.

## Windows assumptions

Version 1 uses an embedding-only unelevated restricted-token session with a private desktop and non-breakaway job object. The application-owned `state_dir` replaces Codex home at the embedding seam and holds per-spawn capability SID and runtime state. The facade does not call `find_codex_home()`, write sandbox logs, or initialize normal user Codex state.

The extracted embedding path differs from the existing Codex path only where the facade contract requires it: it preserves the exact environment, applies and later revokes capability ACEs under a serialized lease, leaves writable `.git`, `.codex`, and `.agents` directories writable when the facade explicitly grants them, uses only generated capability SIDs as write restrictions, and rejects network restriction. Logon and Everyone remain in the token's default DACL for child-created objects but are not restricting SIDs, so host paths writable through those broad groups do not bypass the facade policy. Existing Codex call sites retain their previous token, ACL, environment, and job-object behavior.

The current backend supports host reads, write restrictions, unrestricted network, and job-object process-tree termination. It does not support minimal reads, read deny rules, direct network denial, interrupt delivery, or reopening a writable child below a read-only carveout of a broader writable root.

## Known unsupported capabilities

- Backend selection other than `PlatformDefault`.
- Destination-filtered, proxy-only, or managed-proxy network access.
- Windows minimal-read policy, read deny rules, direct network denial, and interrupts; Windows also rejects write-read-write nested carveouts, including alias-based carveouts.
- Linux network denial on architectures other than x86_64 and aarch64.
- Linux kernels without `close_range(CLOSE_RANGE_CLOEXEC)` support.
- macOS processes whose open-descriptor table does not fit the 1,024-record fork-safe enumeration buffer.
- Linux rules that overlap fresh `/proc`, read-only rules overlapping writable `/dev`, and read/deny paths crossing a child-writable symlink.
- macOS root denies, read-only rules overlapping required writable runtime paths, and nested allows below a deny.
- Nested allow-under-deny combinations that a selected backend cannot express without widening permissions.
- Relative program paths and non-absolute working directories or policy paths.
- PTY transport, approvals, escalation, sessions, output aggregation, and persistent service supervision.
- Non-UTF-8 values at Codex-internal string seams. They are rejected, never converted lossily.
- Linux `LD_*` child environment keys and macOS `DYLD_*` child environment keys, because they would affect the pre-sandbox helper or be stripped by the native launcher.

## Pre-existing files modified outside this crate

- `codex-rs/Cargo.toml`: adds the workspace member and dependency. It also uses the existing immutable `tokio-tungstenite` and `tungstenite` fork revisions directly so this package's graph resolves when Codex is consumed as a Git dependency; a dependency's root Cargo patches do not propagate to its consumer.
- `codex-rs/Cargo.lock`: locks the new package and direct dependency-source declarations above.
- `codex-rs/network-proxy/Cargo.toml`: declares three Rama macro/runtime crates used by its source so the Git dependency graph does not rely on a consumer's resolver accident.
- `codex-rs/sandboxing/src/bwrap.rs`: exposes strict namespace probing and caller-supplied `PATH` lookup while preserving the existing Codex probe's permissive treatment of indeterminate failures.
- `codex-rs/sandboxing/src/bwrap_tests.rs`: covers the strict embedding probe and supplied search path.
- `codex-rs/sandboxing/src/lib.rs`: exports those two narrow Linux queries.
- `codex-rs/sandboxing/src/seatbelt.rs`: exports a query for Seatbelt's mandatory writable runtime roots so incompatible read-only rules can be rejected before launch.
- `codex-rs/utils/pty/src/pty.rs`: exposes a checked form of the existing macOS fork-safe descriptor sweep so an incomplete enumeration stops facade launch without changing existing Codex call sites.
- `codex-rs/linux-sandbox/src/bundled_bwrap.rs`: resolves a packaged bubblewrap relative to an explicit helper and revalidates its digest.
- `codex-rs/linux-sandbox/src/bwrap.rs`: exports the existing writable-symlink check and a query for the implicit writable `/dev` mount.
- `codex-rs/linux-sandbox/src/launcher.rs`: exposes crate-private launcher seams and consults the embedding-selected launcher before the unchanged normal Codex selection path.
- `codex-rs/linux-sandbox/src/lib.rs`: exports the narrow embedding launcher and policy-query surface.
- `codex-rs/linux-sandbox/src/linux_run_main.rs`: activates flattened hidden embedding options and consults the application-owned registry. Normal helper invocation follows its existing path.
- `codex-rs/linux-sandbox/src/linux_run_main_tests.rs`: covers parsing and the all-or-nothing requirement for the hidden arguments.
- `codex-rs/windows-sandbox-rs/src/desktop.rs`: adds an embedding-only private desktop preparation method that grants access to the per-spawn capability SIDs; ordinary desktop preparation is unchanged.
- `codex-rs/windows-sandbox-rs/src/lib.rs`: exports the opaque embedding request, process, handle, and launcher.
- `codex-rs/windows-sandbox-rs/src/token.rs`: gives the embedding-only token module crate-private access to existing default-DACL and privilege helpers; the existing token construction path is unchanged.
- `codex-rs/windows-sandbox-rs/src/unified_exec/backends/mod.rs`: registers the embedding-only backend module.
- `codex-rs/windows-sandbox-rs/src/unified_exec/mod.rs`: defines the narrow embedding request and launch entry point.
- `codex-rs/windows-sandbox-rs/src/unified_exec/tests.rs`: registers the embedding tests without changing existing tests.

The following additive Windows adapter files live beside the native backend because they require crate-private token, ACL, desktop, and process-launch primitives. Keeping them there avoids exporting those primitives or changing existing Codex callers:

- `codex-rs/windows-sandbox-rs/src/embedding_acl.rs`
- `codex-rs/windows-sandbox-rs/src/embedding_acl_mutex.rs`
- `codex-rs/windows-sandbox-rs/src/embedding_token.rs`
- `codex-rs/windows-sandbox-rs/src/unified_exec/backends/embedding.rs`
- `codex-rs/windows-sandbox-rs/src/unified_exec/embedding_process.rs`
- `codex-rs/windows-sandbox-rs/src/unified_exec/embedding_spawn.rs`
- `codex-rs/windows-sandbox-rs/src/unified_exec/embedding_tests.rs`

They implement strict temporary ACL leases, an embedding-only capability restricted token, exact split streams, acknowledged stdin writes, private desktop launch, and job-backed lifecycle. The public facade and most new code remain in `codex-rs/sandbox-api/`.

Two additive Linux adapter files live beside the helper's private launcher and registry implementation:

- `codex-rs/linux-sandbox/src/embedding.rs`
- `codex-rs/linux-sandbox/src/embedding_tests.rs`

They own the pinned-launcher selection and revalidation, hidden helper options, application registry override, and their focused tests. The pre-existing large launcher and helper-entry modules retain only small wiring changes.

No existing crate depends on `codex-sandbox-api`, and no existing Codex caller is routed through it.

## Validation commands

Run from `codex-rs` unless a command says otherwise:

```sh
just test -p codex-sandbox-api
just test -p codex-sandboxing
just test -p codex-linux-sandbox
just test -p codex-windows-sandbox
just test -p codex-network-proxy
just test -p codex-utils-pty
cargo clippy -p codex-sandbox-api --all-targets --all-features -- -D warnings
just fix -p codex-sandbox-api
just fix -p codex-sandboxing
just fix -p codex-linux-sandbox
just fix -p codex-windows-sandbox
just fix -p codex-network-proxy
just fix -p codex-utils-pty
just fmt
```

Run from the repository root after a dependency change:

```sh
just bazel-lock-update
bazel test \
    //codex-rs/sandbox-api:sandbox-api-public_contract-test \
    //codex-rs/sandbox-api:sandbox-api-runtime_contract-test
```

The current cross-compile checks are:

```sh
OPENSSL_DIR=/opt/homebrew/opt/openssl@3 cargo zigbuild \
    -p codex-sandbox-api --lib --target x86_64-unknown-linux-gnu
bazel build \
    --platforms=//:windows_x86_64_gnullvm \
    --@rules_rust//rust/settings:extra_rustc_flag=-Dwarnings \
    //codex-rs/sandbox-api:sandbox-api \
    //codex-rs/sandbox-api:sandbox-api-fixture
```

Also compile the facade for macOS, Linux, and Windows in CI. Run the native smoke tests on their matching hosts; a cross-compile does not validate Seatbelt, bubblewrap/seccomp, restricted tokens, ACLs, private desktops, or job objects.

## Reimplementation procedure

1. Create a fresh branch from the next exact Codex release commit.
2. Inspect the prior facade's public API, tests, `README.md`, and this file.
3. Inspect how the new Codex release implements the native sandbox on macOS, Linux, and Windows, including process and raw-stream adapters.
4. Reimplement the facade against the new release's internals. Make the smallest visibility extraction needed by the leaf crate.
5. Do not merge, cherry-pick, or rebase the old release branch into the new release branch.
6. Preserve `SANDBOX_API_VERSION` unless intentionally changing the downstream contract.
7. Run the public-contract, policy translation, command, native platform, existing touched-crate, Cargo, and Bazel tests on all applicable platforms.
8. Produce one logical commit directly on top of the new release.

Use these trailers in that commit message, substituting the new base SHA if this is a later refresh:

```text
MCP-Console-Patch-Base: 2161ec272a7d6b775c9c721e6206f4fe63e383f2
MCP-Console-Sandbox-API: 1
```
