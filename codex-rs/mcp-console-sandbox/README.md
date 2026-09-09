# Standalone native sandbox executable

`mcp-console-sandbox` extracts the native sandbox in this Codex release into a standalone executable.
Start it with `--config-env NAME -- command [args...]`, or send one large request through `--bootstrap-fd <N>`.
Leave the target's original stdin attached from process creation.
The requested command is an opaque process with inherited stdin, stdout, and stderr.
Version 2 requires UTF-8 arguments, paths, and environment values.

The executable boundary is the integration contract.
Callers do not link to or call Codex Rust crates.
Inside the executable, the upstream filesystem and network policies, managed proxy, and ordinary `SandboxManager` prepare the native sandbox.
A trusted caller can supply additional macOS Seatbelt rules through the optional `macos_seatbelt_profile_extension` bootstrap field.
The private `src/codex.rs` facade contains every upstream import; `src/bootstrap.rs` contains the small local wire wrapper.

One application supervisor validates owned configuration, establishes descendant observation, configures the native sandbox and optional proxy, and releases the target through a private execution gate.
It retires descendants before removing optional private storage and reporting completion.
Parent-death retirement and SIGTERM retirement are explicit options; application restart and recovery remain caller responsibilities.
There is no persistent control channel or target stream protocol.

## Invocation

For an exec-style frontend, put JSON in one explicitly selected child environment variable:

```sh
SANDBOX_CONFIG='{"version":2,"filesystem":{"kind":"restricted","entries":[{"path":{"type":"special","value":{"kind":"root"}},"access":"read"}]},"network":"restricted","lifecycle":{"private_tmp":{"environment":["TMPDIR"]}}}' \
  mcp-console-sandbox --config-env SANDBOX_CONFIG -- /bin/cat
```

Arguments, cwd, and ordinary environment variables stay normal launch inputs.
The selected transport is consumed once and excluded from target setup.
There are no configuration files, automatic fallbacks, or later policy changes.

The private invocation is `mcp-console-sandbox --bootstrap-fd <N>`, where N is an open, readable inherited descriptor greater than 2.
The launcher normally supplies the read end of an anonymous pipe and retains the writer.
Start the executable before writing the four-byte unsigned big-endian length and JSON payload, which may be up to 1 MiB.
A complete valid frame releases startup without waiting for EOF; closing the writer before completing the frame cancels startup.

This is a breaking change to private protocol version 2.
The no-argument invocation is rejected.
In descriptor mode, configuration is read exclusively from the bootstrap descriptor, which is closed before native setup.
There is no descriptor transfer after process creation.
Target stdin keeps its original open file description; the waiting executable releases its input copy after spawning the child.
On Linux, the native helpers transfer that description through an extra inherited descriptor and release their input copies, so target-side closure is visible to the caller while the target is still running.

Stdout and stderr belong to the target.
Configuration or launch failures use stderr and a nonzero exit.
A completed native launch returns its exit code; a signal death maps to `128 + signal`, matching the native CLI.
The [lifecycle contract](LIFECYCLE.md) defines signal handling, retirement, terminal ownership, and failure reporting.

See [PROTOCOL.md](PROTOCOL.md) for the complete request and a runnable caller.

On macOS, `macos_seatbelt_profile_extension` appends trusted caller-supplied SBPL to the native profile in the same sandbox launch.
It can grant permissions as well as restrict them; the runner does not validate it as deny-only.
Omit the field or use `null` to preserve the native profile.
Linux rejects a supplied string.
The caller owns these rules; no application-specific profile is built into the runner.

## Platforms and packaging

Linux and macOS are supported and have separate jobs in the focused workflow.
Both use the release's native sandbox behavior.
See [REBASE.md](REBASE.md) for platform validation environments and results.
Windows compatibility remains outside scope.

| Platform                    | Behavior                                                                                                                         |
| --------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| macOS                       | The ordinary process Seatbelt profile, applied in a native stage that execs the target.                                          |
| Linux                       | The existing Linux helper, bubblewrap, seccomp, and native proxy routing. The executable dispatches its own native helper stage. |
| Windows and other platforms | The executable reports unsupported on stderr and exits 1; no target is launched. No Windows compatibility guarantee is made.     |

Linux uses a suitable host `bwrap` from the target environment's `PATH` first, then the ordinary bundled helper search.
One supported bundled layout places `bwrap` beside the runner.
The existing `codex-resources` and install-context locations remain available.
There is no runner-specific adjacent-helper check, required digest, patched bubblewrap option, or static-libcap requirement.
Native Bazel helper digest verification remains unchanged.

The fixture binary is only a test target.
It is not a runtime companion.

## Build and validation

Use the release's Rust 1.95.0 toolchain, workspace dependencies, and lockfile.
On Linux, building the ordinary bundled bubblewrap requires a C toolchain, `pkg-config`, and libcap development headers and libraries.
On Ubuntu, install these with:

```console
sudo apt-get install -y build-essential pkg-config libcap-dev
```

Build the Linux release pair and resolve Cargo's actual output directory:

```console
cd codex-rs
cargo build --locked --release \
  -p codex-mcp-console-sandbox --bin mcp-console-sandbox \
  -p codex-bwrap --bin bwrap
sandbox_target_dir="$(cargo metadata --locked --format-version=1 --no-deps |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
```

Stage `$sandbox_target_dir/release/mcp-console-sandbox` and `$sandbox_target_dir/release/bwrap` together when supplying the fallback.
The ordinary Cargo helper links to the system libcap; the destination needs its runtime library.
On macOS, build only the runner with `cargo build --locked -p codex-mcp-console-sandbox --bin mcp-console-sandbox --release`.

Bazel builds the executable with `bazel build //codex-rs/mcp-console-sandbox:mcp-console-sandbox`.
No workspace status configuration or source-revision stamp is required.

Build tools do not send application telemetry to OpenAI.
Cargo, Bazel, rustup, and CI actions may download dependencies and tools when they are not cached.
Once dependencies are available, a Cargo build can use `--locked --offline`.

For Linux Cargo tests, build the ordinary debug helper before running the executable suite:

```console
cargo build --locked -p codex-bwrap --bin bwrap
env "CARGO_BIN_EXE_bwrap=$sandbox_target_dir/debug/bwrap" \
  just test -p codex-mcp-console-sandbox --retries 0
```

The Bazel equivalent is `bazel test //codex-rs/mcp-console-sandbox:bootstrap-contract-test`; its test data supplies the runner, fixture, and ordinary bundled helper.
The focused Linux CI job also runs the native sandbox suites and the entire executable suite against the release runner and release bubblewrap.
The macOS job retains its existing coverage and two native test exclusions.
[REBASE.md](REBASE.md) records local results, complete validation commands, and the rolling patch audit.
Hosted CI results are separate from local validation.

## Runtime telemetry and network

This executable does not initialize OpenAI model clients, authentication, sessions, or an OpenTelemetry exporter.
Telemetry is not enabled by default.
Compiling or linking a telemetry-related transitive crate does not activate telemetry.
There is therefore no runner-specific telemetry switch to disable.

With network denied (`"restricted"`) and no proxy, the runner itself performs no application egress.
With a managed proxy, it listens and connects as required by the supplied Codex proxy policy.
With network enabled, the opaque target may make network requests.
Target traffic to OpenAI is target behavior, not runner telemetry.

The bootstrap uses upstream `RemoteNetworkProxyConfig`, the release's existing executor-local projection of `NetworkProxyConfig`.
This directly preserves its network mode, domain and Unix-socket permission types, and local-binding controls.
It chooses ephemeral loopback listeners and carries no MITM, credential injection, hooks, fixed listener addresses, or control service.
The sandbox network permission remains a separate upstream field.
A supplied proxy is enforced through the native sandbox projection.

The explicit environment map replaces the runner's environment for the target.
Native platform or target-runtime initialization may add variables; for example, some macOS toolchains set `__CF_USER_TEXT_ENCODING` before target `main`, and Linux bubblewrap sets `PWD` to the command working directory.
When a proxy is present, the upstream proxy adds or replaces these variables:

| Target-visible variables                                                                                                                                                            | Native proxy value                                                                                                                   |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| `HTTP_PROXY`, `HTTPS_PROXY`, `http_proxy`, `https_proxy`                                                                                                                            | Managed HTTP endpoint.                                                                                                               |
| `YARN_HTTP_PROXY`, `YARN_HTTPS_PROXY`, `npm_config_http_proxy`, `npm_config_https_proxy`, `npm_config_proxy`, `NPM_CONFIG_HTTP_PROXY`, `NPM_CONFIG_HTTPS_PROXY`, `NPM_CONFIG_PROXY` | Managed HTTP endpoint.                                                                                                               |
| `BUNDLE_HTTP_PROXY`, `BUNDLE_HTTPS_PROXY`, `PIP_PROXY`, `DOCKER_HTTP_PROXY`, `DOCKER_HTTPS_PROXY`                                                                                   | Managed HTTP endpoint.                                                                                                               |
| `WS_PROXY`, `WSS_PROXY`, `ws_proxy`, `wss_proxy`                                                                                                                                    | Managed HTTP endpoint.                                                                                                               |
| `ALL_PROXY`, `all_proxy`, `FTP_PROXY`, `ftp_proxy`                                                                                                                                  | Managed `socks5h` endpoint when SOCKS is enabled; otherwise the HTTP endpoint.                                                       |
| `NO_PROXY`, `no_proxy`, `npm_config_noproxy`, `NPM_CONFIG_NOPROXY`, `YARN_NO_PROXY`, `BUNDLE_NO_PROXY`                                                                              | Empty when local binding is disabled; otherwise `localhost,127.0.0.1,::1,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16`.                   |
| `CODEX_NETWORK_PROXY_ACTIVE`                                                                                                                                                        | `1`.                                                                                                                                 |
| `CODEX_NETWORK_ALLOW_LOCAL_BINDING`                                                                                                                                                 | `1` or `0`, from the supplied policy.                                                                                                |
| `ELECTRON_GET_USE_PROXY`                                                                                                                                                            | `true`.                                                                                                                              |
| `NODE_USE_ENV_PROXY`                                                                                                                                                                | `1`.                                                                                                                                 |
| `GIT_SSH_COMMAND` on macOS with SOCKS enabled                                                                                                                                       | `CODEX_PROXY_GIT_SSH_COMMAND=1 ssh -o ProxyCommand='nc -X 5 -x <SOCKS address> %h %p'`. An existing custom SSH command is preserved. |

Linux's native routing rewrites proxy endpoint ports for the target network namespace.
No CA, credential, or attribution variables are added by this executor-local proxy path.
The native proxy removes stale `CODEX_NETWORK_PROXY_CREDENTIAL_BROKER_ACTIVE`, `CODEX_NETWORK_PROXY_BROKERED_CREDENTIALS`, and `CODEX_NETWORK_PROXY_ATTRIBUTION_TOKEN` values.

## Native limitations

Linux requires kernel 5.11 or newer for the bubblewrap path, user namespaces with UID/GID mappings, mount and PID namespaces, and network namespaces for restricted or proxy-managed networking.
Host security policy must permit these operations.
A basic prerequisite probe is:

```console
unshare --user --map-root-user --mount --net --pid --fork true
```

Ubuntu's AppArmor restrictions can block unprivileged namespace setup even when `kernel.unprivileged_userns_clone=1` and `user.max_user_namespaces` is nonzero.
Errors such as `setting up uid map: Permission denied` or `loopback: Failed RTM_NEWADDR: Operation not permitted` can reflect that host restriction.
The repository's shared Linux CI setup enables user namespaces and removes that restriction on its CI runner.
Containers also need permission to perform native namespace operations.

There is no automatic legacy-Landlock or unsandboxed fallback.
Filesystem policies must allow the executable and runtime files needed by the native sandbox path.
Linux supervision requires a restricted filesystem policy and namespace-local procfs; full-disk write policies are rejected.
Restricted networking permits local Unix socket-pair I/O, shutdown, and socket inspection.
Connecting, binding, listening, creating IP sockets, and sends with an explicit destination remain denied; inherited connected descriptors retain the access supplied by their caller.

Without a caller-supplied extension, the ordinary macOS profile retains its existing sysctl, Mach-service, terminal, and filesystem restrictions.
Applications may need caller-owned compatibility rules for operations outside those native permissions.

Proxy protocol support and local-network exceptions are those of the release; the runner adds no UDP routing or approval service.
There is no watchdog or custom recovery after the supervisor crashes or receives SIGKILL.
Surviving workloads retain native restrictions, but descendant retirement, private-storage cleanup, and terminal restoration are then not guaranteed.
Darwin retirement covers observed process identities; see [LIFECYCLE.md](LIFECYCLE.md) for that boundary and other limits.

See the [handoff notes](PROTOCOL.md#downstream-handoff) and [upstream integration note](INTEGRATION.md).
