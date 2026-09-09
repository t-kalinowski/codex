# Rolling native sandbox extraction

The current branch's added capabilities are listed in [CAPABILITIES.md](CAPABILITIES.md), and its four native integration files are described in [INTEGRATION.md](INTEGRATION.md). The Linux socket-operation relaxation was removed; the native seccomp rules match the release. The sections below record the earlier extraction and descriptor-transport stages; statements about caller-owned supervision and unchanged native entry points describe those earlier stages.

This branch is based on `rust-v0.150.1`, commit `90854393966b21e9ebfd21b122334eb09a20c93d`. The one-shot correction starts after `b4de42be4e329fd6df06e6755b99f37f4a7ff5c6`. Earlier commits remain in the history; the correction is expressed entirely as additional commits.

## Existing upstream files

The final patch changes only these files that existed in the release base:

| Exact path            | Why it remains modified                                                                                             | Why the leaf cannot avoid it                                                                            | Protection                                                                   | Expected rebase risk                                              |
| --------------------- | ------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------- | ----------------------------------------------------------------- |
| `codex-rs/Cargo.toml` | Registers the standalone package as a workspace member.                                                             | Workspace builds, dependency inheritance, and repository checks need membership.                        | Locked Cargo builds and executable contract tests; Bazel package resolution. | Low: a member-list insertion may conflict.                        |
| `codex-rs/Cargo.lock` | Records the leaf package and refreshes workspace package versions from `0.0.0` to the release manifest's `0.150.1`. | Cargo resolves the whole workspace; the release lockfile itself predates those manifest version stamps. | `cargo metadata --offline`, locked builds, and `just bazel-lock-update`.     | Generated: resolve against the new release, then review the diff. |

No upstream visibility change is required. The release already exports `RawFileSystemSandboxPolicy`, its runtime conversion, `NetworkSandboxPolicy`, `PermissionProfile`, absolute paths, the default `SandboxManager`, the Linux helper entry point, and the managed proxy APIs needed by the executable.

The proxy bootstrap uses `RemoteNetworkProxyConfig`, the existing executor-local projection of `NetworkProxyConfig`. Its native constructor supplies static state without a custom reloader, credentials, sessions, listener negotiation, or duplicated policy types. `RemoteNetworkProxyLaunchConfig` and the native prepared sandbox context handle proxy launch and environment projection.

`src/codex.rs` is a private module, not a workspace crate or a public Rust API. The executable owns the bootstrap wrapper. All other added files are inside the leaf package or its focused workflow.

## Restored release behavior

The Linux seccomp deny rules, host-bwrap-first selection, ordinary bundled helper search, optional native digest verification, native synthetic-mount registry, vendored bubblewrap, and dynamic libcap build behavior match the release byte for byte. There is no private bubblewrap stream option.

The Seatbelt manager, profiles, SBPL, and compile-data list match the release. The application-specific profile was deleted. The workspace-status script, leaf build script, and `.bazelrc` stamping configuration were removed.

The runner has no persistent protocol, stream bridge, supervisor, process-tree tracker, parent monitor, generation state, or application-directory ownership. The only explicit Tokio features are `rt` and `process`; the current-thread runtime also drives the features required transitively by the native proxy.

## Executable contract coverage

The built-executable suite covers valid launch, all truncated header lengths, zero and oversized payload lengths, truncated payloads, invalid JSON, unknown versions, empty commands, cwd and environment propagation, write denials and grants, direct network denial and enablement, managed proxy allow/deny rules, binary stdout and stderr, exit codes, native signal mapping, launch errors, and target-visible descriptor inheritance with and without a proxy.

The current private protocol is version 2: `--bootstrap-fd <N>` supplies configuration independently of stdin. The tests cover prequeued binary input, including bytes resembling a version-1 frame, empty open stdin, regular-file offsets and shared seeks, PTY identity, null and closed stdin, fragmented and maximum-size frames, cancellation, exact frame consumption, malformed arguments, descriptor reuse, bootstrap-resource closure with and without a proxy, and independent concurrent launches. A full input pipe and target checkpoint verify that the waiting executable releases its own reader. This ownership test uses the native seccomp-only path on Linux to distinguish this executable from upstream helpers that can retain stdin; the other Linux contracts retain bubblewrap coverage.

Linux tests exercise host selection and the ordinary bundled fallback through an executable probe of the native helper selection, including a host without `--argv0`. The leaf keeps its actual executable path in `argv[0]` and dispatches the native helper's leading `--sandbox-policy-cwd` option. This supplies the release's legacy-bwrap re-exec path without a temporary command alias or a change to native selection. macOS checks the ordinary profile by allowing `hw.ncpu` and denying `kern.boottime`. Environment tests compare the complete target result with a direct native launch, including platform runtime additions such as `__CF_USER_TEXT_ENCODING`.

The focused workflow retains the macOS job and adds an Ubuntu 24.04 Linux job. Linux runs all executable contracts against debug and release runner/bubblewrap pairs, the native sandbox suites without exclusions, the Bazel executable contract, and focused lint checks. The shared CI setup supplies namespace prerequisites. Windows remains outside scope. There is no special hash staging, static-libcap check, discovery call, or lifecycle protocol smoke test.

The following validation sections describe earlier commits and protocol version 1. They are historical records, not results for the descriptor transport change.

## Unchanged native test failures

On the development macOS host, the full native sandbox suite has 92 passing tests and two failures:

- `create_seatbelt_args_with_read_only_git_and_codex_subpaths`
- `create_seatbelt_args_with_read_only_git_pointer_file`

Both failures reproduce in a clean worktree at the release base. The operation is denied, but `assert_seatbelt_denied` expects `bash: <path>: Operation not
permitted`, while the installed bash prints `bash: line 1: <path>: Operation
not permitted`. The runner patch does not change these tests or their policy. The workflow retains its existing exclusions for those two native tests; all runner contract tests execute.

## Validation of the correction

On macOS, all 15 executable contract tests passed through `just test`, with retries disabled, and all 15 passed through Bazel. The locked offline Cargo build also passed. Nextest reported no leaks in the final run. An earlier concurrent run had a Nextest `LEAK` label on the test that retains the caller's stdin; its assertions passed, and an isolated run and the final full run did not report it. No supervision or timeout workaround was added.

Before the compatibility scope was narrowed to macOS, the retained Linux path passed 17 executable contracts, 207 native sandbox tests, and focused Clippy. Those are preliminary results; they do not replace validation on the current Linux host.

`just fix -p codex-mcp-console-sandbox`, `just fmt`, the workspace Rust format check, focused Clippy with all targets and features and warnings denied, the argument-comment lint, and the focused workflow's Actionlint check passed. `just bazel-lock-update` completed without changing `MODULE.bazel.lock`; the Cargo lockfile changes no external dependency versions. `git diff --check` passed.

## Linux validation on 2026-09-07

The local host is Ubuntu 24.04.4 LTS, x86_64, kernel `6.8.0-139-generic`, glibc 2.39, and GCC 13.3.0. The workspace toolchain is Rust/Cargo 1.95.0, with Nextest 0.9.103, just 1.51.0, Bazel 9.0.0, and Actionlint 1.7.12. The host `/usr/bin/bwrap` is Ubuntu's bubblewrap 0.9.0; libcap is 2.66. Cargo metadata resolves the target directory to `/home/tomasz/github/t-kalinowski/codex/codex-rs/target`. Successful suite runs used the ordinary host user, UID 1000, outside a container.

The initial host had `kernel.unprivileged_userns_clone=1`, `user.max_user_namespaces=256081`, and `kernel.apparmor_restrict_unprivileged_userns=1`. A direct namespace probe failed writing `/proc/self/uid_map` with `Operation not permitted`. The ordinary Cargo bwrap build also reported missing libcap development headers/pkg-config metadata. After installing `libcap-dev` and setting `kernel.apparmor_restrict_unprivileged_userns=0`, the full `unshare --user --map-root-user --mount --net --pid --fork true` probe passed. No process seccomp filter was active before native sandbox launch.

The initial Bazel build passed, but 13 of 17 contracts failed during namespace setup, including `setting up uid map: Permission denied` and `loopback: Failed RTM_NEWADDR: Operation not permitted`. Those errors cleared after host setup without changing the runner or native policies. The existing 17 Cargo executable contracts and 207 native tests then passed before the test-harness correction below.

### Concurrent executable staging

Bazel's shared test process exposed a Linux test-harness race: a launch could fail with `Text file busy` when a concurrent fork inherited another thread's writable binary-copy descriptor. Close-on-exec does not release that descriptor until the child execs. The new public executable regression, `concurrent_launches_preserve_independent_input`, starts eight callers together and checks four distinct 8,193-byte inputs per caller. It failed in all five Bazel repetitions before the correction.

Linux test staging now runs each binary copy through `cp` and waits for that process to finish. Writable executable descriptors stay out of the test process and therefore cannot reach its concurrently forked launchers. This applies to the runner, ordinary bundled bwrap, and host-selection fixtures. The tests remain concurrent. No runtime code, native descriptor policy, or retry behavior changed.

| Local check                                                                | Result                                                                        |
| -------------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| Locked ordinary debug bwrap and release runner/bwrap builds                | Passed; both Cargo bwrap artifacts dynamically link `libcap.so.2`.            |
| Debug executable contracts                                                 | 18 passed, 0 skipped, retries disabled.                                       |
| Native `codex-sandboxing`, `codex-linux-sandbox`, and `codex-bwrap` suites | 207 passed, 0 skipped, retries disabled; no Linux exclusions.                 |
| Release runner plus release bwrap through all executable contracts         | 18 passed, 0 skipped, retries disabled.                                       |
| Bazel executable contract, uncached, five repetitions                      | All five passed all 18 contracts: 90 passing executions.                      |
| Release runner with system `PATH=/usr/bin:/bin`                            | Passed a 65,537-byte prequeued binary stdin round trip with host bwrap 0.9.0. |

The five-run regression check used `bazel test --cache_test_results=no --runs_per_test=5 //codex-rs/mcp-console-sandbox:bootstrap-contract-test`. The ordinary debug/release/native commands are listed below. Cargo's release check explicitly supplied both `CARGO_BIN_EXE_mcp-console-sandbox` and `CARGO_BIN_EXE_bwrap`. The critical stdin boundaries, maximum JSON frame, open-stdin launch, complete environment comparison including native `PWD`, proxy allow/deny behavior, and final-target descriptor checks all remain covered. Nextest reported no leaks in the final executable runs.

`just fix -p codex-mcp-console-sandbox`, `just fmt`, `cargo fmt --all -- --check`, focused Clippy with all targets/features and `-D warnings`, the argument-comment lint, Actionlint, and `git diff --check` passed locally. The packaged argument-comment linter emits unknown-lint warnings while checking upstream dependencies; it reports no leaf finding. The ordinary bwrap build retains GCC warnings in vendored C code. No native source or warning policy was changed.

The runtime source, bootstrap protocol, Bazel rules, and native policies remain unchanged. The only Linux code correction is in the leaf's executable test harness. The residual existing-upstream-file audit above still contains only workspace membership and generated Cargo lock state. No dependencies changed during the Linux work, so no lockfile regeneration is needed. The macOS job and its two native exclusions are preserved. Hosted CI has not run for these new commits; no push or workflow dispatch was performed.

## Caller-supplied macOS profile extension

The optional `macos_seatbelt_profile_extension` bootstrap field lets a trusted caller append SBPL to the profile prepared by the native Seatbelt backend. The leaf verifies the selected backend and `/usr/bin/sandbox-exec -p` command prefix before appending the rules to that same invocation. This preserves a single sandbox initialization; macOS rejects attempts to add another profile inside an already sandboxed target.

The caller owns the rules. They can grant permissions as well as restrict them, and the runner does not validate them as deny-only. Absent or null leaves the native profile unchanged. Linux rejects a supplied string before native setup. The runner contains no application-specific policy, and no upstream production file or dependency changed.

The public executable regressions first failed because the old bootstrap rejected the new field. The macOS suite now covers unchanged default and null behavior, blocking a pre-existing host PTY while preserving fresh PTY creation, name lookup and bidirectional input/output, granting an otherwise denied sysctl, and rejecting malformed SBPL before target launch. All 18 executable contracts passed through `just test -p codex-mcp-console-sandbox --retries 0` and the Bazel executable target. A Linux rejection regression is included but has not run on this macOS host.

`just fix -p codex-mcp-console-sandbox`, `just fmt`, the workspace Rust format check, focused Clippy with all targets/features and `-D warnings`, the argument-comment lint, and `git diff --check` passed. The argument-comment check retained its existing unknown-lint warnings in upstream dependencies and reported no leaf finding. Hosted CI has not run for this change.

## Dedicated bootstrap descriptor on 2026-09-08

The private entry point now accepts only `--bootstrap-fd <N>` with a version-2 frame on an inherited readable descriptor above stdio. Validation and ownership adoption precede descriptor enumeration and runtime creation, so a closed caller descriptor cannot be mistaken for a newly allocated internal descriptor. Ordinary `File::read_exact` framing remains bounded to 1 MiB and does not wait for EOF. The bootstrap file closes before runtime, proxy, helper, or target setup.

The target keeps its original stdin open file description. The executable drops its `Command` after spawn to release the owned input reader before waiting. The executable regression failed with that drop removed and passed with it restored. No stdin relay, descriptor passing, signal change, or lifecycle protocol was added. Rust startup's existing normalization of closed stdio to `/dev/null` remains in effect.

The native Linux helper dispatch stays ahead of top-level argument validation, preserving leading `--sandbox-policy-cwd` invocations and re-execs for host bubblewrap without `--argv0`. Helper setup descriptors retain their existing ownership. No upstream helper, policy, facade, or dependency changed. The focused macOS workflow now exercises the full release executable suite, matching Linux's existing debug/release coverage.

See [PROTOCOL.md](PROTOCOL.md) and [examples/bootstrap.py](examples/bootstrap.py) for the new contract and caller. Downstream must update the immutable source and protocol pins, inherit the bootstrap read descriptor, leave target stdin attached, and remove SCM_RIGHTS stdin handoff. Keep any wrapper code needed to restore the target's signal mask while the waiting native executable retains blocked signals. MCP Console continues to own manager readiness, supervision, descendant retirement, temporary directories, terminal ownership, and signal delivery. Adoption in MCP Console is a separate change.

Implementation commit: `cdce4a6dbd75802cc2747999f9ff1afa3329c6a3`, added after `093828701` on the existing branch. Validation used macOS 26.6.2 arm64 and an isolated worktree on the Ubuntu 24.04.4 x86_64 host `mule`, kernel `6.8.0-139-generic`. Both used the workspace Rust 1.95.0 and Bazel 9.0.0; Nextest was 0.9.118 on macOS and 0.9.103 on Linux. The Linux namespace prerequisite probe passed, with host bubblewrap 0.9.0 and libcap 2.66.

| Check                                                 | macOS                                          | Linux                                             |
| ----------------------------------------------------- | ---------------------------------------------- | ------------------------------------------------- |
| Locked Cargo debug and release builds                 | Passed, offline                                | Passed; release offline, including ordinary bwrap |
| Debug executable contracts, retries disabled          | 32 passed, 0 skipped; Nextest annotation below | 32 passed, 0 skipped                              |
| Release executable contracts, retries disabled        | 32 passed, 0 skipped                           | 32 passed, 0 skipped, with release bwrap          |
| Uncached Bazel executable contracts                   | 32 passed                                      | 32 passed                                         |
| Native sandbox suites, retries disabled               | 92 passed, 2 existing failures, 0 skipped      | 207 passed, 0 skipped                             |
| Python caller, debug and release                      | 1,048,832 binary input bytes preserved         | 1,048,832 binary input bytes preserved            |
| Focused Clippy, all targets/features, warnings denied | Passed                                         | Passed                                            |
| Argument-comment lint                                 | Passed                                         | Passed                                            |

The macOS native failures are the two bash stderr-matching cases already recorded above. One concurrent debug run marked the passing direct-network contract `LEAK`. Three further bounded full-suite repetitions passed all 96 executions; two repetitions also had one `LEAK` annotation each, on managed-proxy and launch-failure contracts. The source of these intermittent Nextest output-lifetime annotations was not isolated. The bootstrap-resource and waiting-parent stdin-closure regressions passed throughout. No timeout, assertion, signal, or native-helper workaround was added.

Linux Bazel initially found that two new argument-test loops reused a staging directory and tried to overwrite read-only copied executables. Giving each invocation its own staging directory corrected those failures; the subsequent complete Bazel suite passed. This preserves the existing separate-copy-process harness and native helper selection.

`just fix -p codex-mcp-console-sandbox --locked`, `just fmt`, `cargo fmt --all -- --check`, Actionlint, Python Ruff format/lint, Markdown formatting, and `git diff --check` passed. Functional tests preceded the prescribed final fix/format pass; Clippy made no fixes. The argument-comment tool retains its upstream unknown-lint warnings, Rust formatting warns about its nightly-only import option, and the ordinary Linux bwrap build retains vendored C warnings. No dependencies changed, so lockfile regeneration was unnecessary. Hosted CI was not run; no push or workflow dispatch was performed. Earlier validation records remain unchanged.

## Updating to another release

Start a new release branch and review the native interfaces before carrying forward the leaf and workflow. Keep the two workspace changes separate from leaf behavior. Regenerate Cargo and Bazel lock state after dependencies are settled, inspect every existing-file modification, and run the native and executable checks on both Linux and macOS. Windows compatibility remains outside scope.

For Linux, first establish the prerequisites in [README.md](README.md), then run the following from the repository root. Resolve the target directory instead of assuming `target/`:

```sh
git diff --name-status 90854393966b21e9ebfd21b122334eb09a20c93d...HEAD
cd codex-rs
cargo build --locked -p codex-bwrap --bin bwrap
sandbox_target_dir="$(cargo metadata --locked --format-version=1 --no-deps |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
env "CARGO_BIN_EXE_bwrap=$sandbox_target_dir/debug/bwrap" \
  just test -p codex-mcp-console-sandbox --retries 0
env "PATH=$sandbox_target_dir/debug:$PATH" \
  "CARGO_BIN_EXE_bwrap=$sandbox_target_dir/debug/bwrap" \
  just test -p codex-sandboxing -p codex-linux-sandbox -p codex-bwrap --retries 0
cargo build --locked --release \
  -p codex-mcp-console-sandbox --bin mcp-console-sandbox \
  -p codex-bwrap --bin bwrap
env "CARGO_BIN_EXE_mcp-console-sandbox=$sandbox_target_dir/release/mcp-console-sandbox" \
  "CARGO_BIN_EXE_bwrap=$sandbox_target_dir/release/bwrap" \
  just test -p codex-mcp-console-sandbox --retries 0
cd ..
bazel test //codex-rs/mcp-console-sandbox:bootstrap-contract-test
just argument-comment-lint -p codex-mcp-console-sandbox
cd codex-rs
just fix -p codex-mcp-console-sandbox
just fmt
cargo fmt --all -- --check
cargo clippy -p codex-mcp-console-sandbox --all-targets --all-features -- -D warnings
cd ..
git diff --check
```

After dependency changes, also run `just bazel-lock-update` from the repository root and inspect the generated lockfiles. On macOS, omit the Linux helper packages and retain exactly the two native test exclusions listed above. Run the complete executable suite against both debug and release native executables. Do not rerun functional tests merely because fix/format ran.

Put the built debug bwrap on `PATH` only for the native tests: one unchanged test uses the host executable as a bundled fixture, which requires bundled capabilities. The runner contracts separately exercise suitable, unsuitable, missing, and no-`--argv0` host executables. Do not carry application-specific native policy rules or lifecycle machinery into the next release. Preserve the caller-supplied extension only after verifying the new native Seatbelt command shape and rerunning its executable contracts.
