# Rolling native sandbox extraction

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

Stdin tests queue `[length][JSON][sentinel]` together for sentinel sizes from 1 byte through 1 MiB, with additional 4 KiB and 8 KiB boundary sizes. They also exercise a maximum-size JSON frame and launch while the caller retains an open stdin. Replacing the raw `File` reader with `BufReader<File>` makes the sentinel test fail because the target loses the queued byte.

Linux tests exercise host selection and the ordinary bundled fallback through an executable probe of the native helper selection, including a host without `--argv0`. The leaf keeps its actual executable path in `argv[0]` and dispatches the native helper's leading `--sandbox-policy-cwd` option. This supplies the release's legacy-bwrap re-exec path without a temporary command alias or a change to native selection. macOS checks the ordinary profile by allowing `hw.ncpu` and denying `kern.boottime`. Environment tests compare the complete target result with a direct native launch, including platform runtime additions such as `__CF_USER_TEXT_ENCODING`.

The focused workflow tests macOS and checks a release executable. macOS is the compatibility gate; Linux and Windows compatibility are outside this task's final scope. There is no special hash staging, static-libcap check, discovery call, or lifecycle protocol smoke test.

## Unchanged native test failures

On the development macOS host, the full native sandbox suite has 92 passing tests and two failures:

- `create_seatbelt_args_with_read_only_git_and_codex_subpaths`
- `create_seatbelt_args_with_read_only_git_pointer_file`

Both failures reproduce in a clean worktree at the release base. The operation is denied, but `assert_seatbelt_denied` expects `bash: <path>: Operation not
permitted`, while the installed bash prints `bash: line 1: <path>: Operation
not permitted`. The runner patch does not change these tests or their policy. The workflow retains its existing exclusions for those two native tests; all runner contract tests execute.

## Validation of the correction

On macOS, all 15 executable contract tests passed through `just test`, with retries disabled, and all 15 passed through Bazel. The locked offline Cargo build also passed. Nextest reported no leaks in the final run. An earlier concurrent run had a Nextest `LEAK` label on the test that retains the caller's stdin; its assertions passed, and an isolated run and the final full run did not report it. No supervision or timeout workaround was added.

Before the compatibility scope was narrowed to macOS, the retained Linux path passed 17 executable contracts, 207 native sandbox tests, and focused Clippy. Those results do not expand this patch's macOS compatibility commitment.

`just fix -p codex-mcp-console-sandbox`, `just fmt`, the workspace Rust format check, focused Clippy with all targets and features and warnings denied, the argument-comment lint, and the focused workflow's Actionlint check passed. `just bazel-lock-update` completed without changing `MODULE.bazel.lock`; the Cargo lockfile changes no external dependency versions. `git diff --check` passed.

## Updating to another release

Start a new release branch and review the native interfaces before carrying forward the leaf and workflow. Keep the two workspace changes separate from leaf behavior. Regenerate Cargo and Bazel lock state after dependencies are settled, inspect every existing-file modification, and run the native and executable checks on macOS. Other platform compatibility needs separate work.

```console
git diff --name-status 90854393966b21e9ebfd21b122334eb09a20c93d...HEAD
cd codex-rs
just test -p codex-mcp-console-sandbox
just test -p codex-sandboxing
cargo clippy -p codex-mcp-console-sandbox --all-targets --all-features -- -D warnings
cd ..
just bazel-lock-update
bazel test //codex-rs/mcp-console-sandbox:bootstrap-contract-test
just fix -p codex-mcp-console-sandbox
just fmt
cd codex-rs
cargo fmt --all -- --check
cd ..
git diff --check
```

On Linux, also build and test `codex-bwrap` and test `codex-linux-sandbox`. Put the built debug bwrap on `PATH` for native tests: one unchanged test uses the host executable as a bundled fixture, which requires bundled capabilities. The runner contracts separately exercise suitable, unsuitable, missing, and no-`--argv0` host executables. Do not carry native policy relaxations or lifecycle machinery into the next release. A native limitation should remain explicit rather than acquire a second implementation in the leaf.
