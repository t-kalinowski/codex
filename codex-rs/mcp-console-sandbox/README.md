# Standalone native sandbox runner

`mcp-console-sandbox` extracts the native sandbox in this Codex release into a standalone executable. Start it as an ordinary child process and send one bootstrap frame on stdin. The requested command is an opaque process with inherited stdin, stdout, and stderr. Version 1 requires UTF-8 arguments, paths, and environment values.

The executable boundary is the integration contract. Callers do not link to or call Codex Rust crates. Inside the executable, the upstream filesystem and network policies, managed proxy, and ordinary `SandboxManager` own sandbox behavior. The private `src/codex.rs` facade contains every upstream import; `src/bootstrap.rs` contains the small local wire wrapper.

The runner validates one bootstrap, configures the native sandbox and optional proxy, launches one command, waits, and releases its native setup resources. Generation lifetime, parent monitoring, restarts, retirement, application temporary directories, and backend lifecycle management belong to the caller. There is no persistent control channel or target stream protocol.

## Invocation

The normal invocation takes no arguments. Write a four-byte unsigned big-endian payload length, the JSON payload, and then any target input. The maximum JSON payload is 1 MiB. Target input may follow immediately; there is no launch acknowledgment. The runner reads only the remaining frame bytes from raw fd 0 and transfers that same open file description to the command.

Stdout and stderr belong to the target. Configuration or launch failures use stderr and a nonzero exit. A completed native launch returns its exit code; a signal death maps to `128 + signal`, matching the native CLI. The caller chooses any cancellation, terminal ownership, or process-tree retirement policy.

See [PROTOCOL.md](PROTOCOL.md) for the complete request and a runnable caller.

## Platforms and packaging

The focused workflow has separate Linux and macOS jobs. macOS compatibility is validated; local Linux runtime validation is pending the host prerequisites recorded in [REBASE.md](REBASE.md). Windows compatibility remains outside scope. Both platform paths use the release's native sandbox behavior.

| Platform                    | Behavior                                                                                                                         |
| --------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| macOS                       | The ordinary Codex process Seatbelt profile through `/usr/bin/sandbox-exec`.                                                     |
| Linux                       | The existing Linux helper, bubblewrap, seccomp, and native proxy routing. The executable dispatches its own native helper stage. |
| Windows and other platforms | The executable reports unsupported on stderr and exits 1; no target is launched. No Windows compatibility guarantee is made.     |

Linux uses a suitable host `bwrap` from the target environment's `PATH` first, then the ordinary bundled helper search. One supported bundled layout places `bwrap` beside the runner. The existing `codex-resources` and install-context locations remain available. There is no runner-specific adjacent-helper check, required digest, patched bubblewrap option, or static-libcap requirement. Native Bazel helper digest verification remains unchanged.

The fixture binary is only a test target. It is not a runtime companion.

## Build and validation

Use the release's Rust 1.95.0 toolchain, workspace dependencies, and lockfile. On Linux, building the ordinary bundled bubblewrap requires a C toolchain, `pkg-config`, and libcap development headers and libraries. On Ubuntu, install these with:

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

Stage `$sandbox_target_dir/release/mcp-console-sandbox` and `$sandbox_target_dir/release/bwrap` together when supplying the fallback. The ordinary Cargo helper links to the system libcap; the destination needs its runtime library. On macOS, build only the runner with `cargo build --locked -p codex-mcp-console-sandbox --bin mcp-console-sandbox --release`.

Bazel builds the executable with `bazel build //codex-rs/mcp-console-sandbox:mcp-console-sandbox`. No workspace status configuration or source-revision stamp is required.

Build tools do not send application telemetry to OpenAI. Cargo, Bazel, rustup, and CI actions may download dependencies and tools when they are not cached. Once dependencies are available, a Cargo build can use `--locked --offline`.

For Linux Cargo tests, build the ordinary debug helper before running the executable suite:

```console
cargo build --locked -p codex-bwrap --bin bwrap
env "CARGO_BIN_EXE_bwrap=$sandbox_target_dir/debug/bwrap" \
  just test -p codex-mcp-console-sandbox --retries 0
```

The Bazel equivalent is `bazel test //codex-rs/mcp-console-sandbox:bootstrap-contract-test`; its test data supplies the runner, fixture, and ordinary bundled helper. The focused Linux CI job also runs the native sandbox suites and the entire executable suite against the release runner and release bubblewrap. The macOS job retains its existing coverage and two native test exclusions. [REBASE.md](REBASE.md) records local results, complete validation commands, and the rolling patch audit. Hosted CI results are separate from local validation.

## Runtime telemetry and network

This executable does not initialize OpenAI model clients, authentication, sessions, or an OpenTelemetry exporter. Telemetry is not enabled by default. Compiling or linking a telemetry-related transitive crate does not activate telemetry. There is therefore no runner-specific telemetry switch to disable.

With network denied (`"restricted"`) and no proxy, the runner itself performs no application egress. With a managed proxy, it listens and connects as required by the supplied Codex proxy policy. With network enabled, the opaque target may make network requests. Target traffic to OpenAI is target behavior, not runner telemetry.

The bootstrap uses upstream `RemoteNetworkProxyConfig`, the release's existing executor-local projection of `NetworkProxyConfig`. This directly preserves its network mode, domain and Unix-socket permission types, and local-binding controls. It chooses ephemeral loopback listeners and carries no MITM, credential injection, hooks, fixed listener addresses, or control service. The sandbox network permission remains a separate upstream field. A supplied proxy is enforced through the native sandbox projection.

The explicit environment map replaces the runner's environment for the target. Native platform or target-runtime initialization may add variables; for example, some macOS toolchains set `__CF_USER_TEXT_ENCODING` before target `main`, and Linux bubblewrap sets `PWD` to the command working directory. When a proxy is present, the upstream proxy adds or replaces these variables:

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

Linux's native routing rewrites proxy endpoint ports for the target network namespace. No CA, credential, or attribution variables are added by this executor-local proxy path. The native proxy removes stale `CODEX_NETWORK_PROXY_CREDENTIAL_BROKER_ACTIVE`, `CODEX_NETWORK_PROXY_BROKERED_CREDENTIALS`, and `CODEX_NETWORK_PROXY_ATTRIBUTION_TOKEN` values.

## Native limitations

Linux requires kernel 5.11 or newer for the bubblewrap path, user namespaces with UID/GID mappings, mount and PID namespaces, and network namespaces for restricted or proxy-managed networking. Host security policy must permit these operations. A basic prerequisite probe is:

```console
unshare --user --map-root-user --mount --net --pid --fork true
```

Ubuntu's AppArmor restrictions can block unprivileged namespace setup even when `kernel.unprivileged_userns_clone=1` and `user.max_user_namespaces` is nonzero. Errors such as `setting up uid map: Permission denied` or `loopback: Failed RTM_NEWADDR: Operation not permitted` can reflect that host restriction. The repository's shared Linux CI setup enables user namespaces and removes that restriction on its CI runner. Containers also need permission to perform native namespace operations.

There is no automatic legacy-Landlock or unsandboxed fallback. Filesystem policies must allow the executable and runtime files needed by the native sandbox path. Upstream policy kinds retain their upstream meaning. Native seccomp restrictions on Unix sockets also remain unchanged; inheriting a socket as a standard stream does not exempt its operations from those rules.

The ordinary macOS profile retains its existing sysctl, Mach-service, terminal, and filesystem restrictions. Applications that needed the removed custom allowances may now fail under those same native restrictions.

Proxy protocol support and local-network exceptions are those of the release; the runner adds no UDP routing, approval service, or policy extensions. Native helpers may retain standard streams or terminate descendants according to their own behavior. The runner waits for the native launch path and adds no general process-tree cleanup, signal forwarding, parent-death monitor, or stdin relay.
