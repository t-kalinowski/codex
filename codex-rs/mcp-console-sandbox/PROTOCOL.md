# Bootstrap protocol version 2

Two explicit input modes are supported. Both accept one immutable configuration and use the same permission-driven execution selection. Default execution uses the runner's supervisor; explicit Linux Landlock execution has no waiting supervisor, as described below.

## Environment configuration

Invoke `mcp-console-sandbox --config-env NAME -- command [args...]`. `NAME` selects a UTF-8 JSON value already present in this child's launch environment, never a filename. The JSON requires `version`, `filesystem`, and `network` from the request below. `command` and `cwd` are rejected: supply the command after `--` and select the working directory when launching the child. The optional `environment` object contains target-only overrides. `inherit_environment` defaults to `true`; set it to `false` to start with an empty target environment. Omitting both fields inherits the ordinary launch environment without requiring its serialization. Private-directory exports and managed proxy values override ordinary target settings. No application policy is merged into an explicit configuration.

The OS copies the launch environment during process creation. The runner reads the selected value once into owned validated state before runtime or native setup. Changing the parent's environment after successful launch, even before the runner parses JSON, cannot alter that copy. There is no file discovery, path reference, include, reload, or overflow file. Both input modes reject conflicting invocation options and duplicate top-level fields.

The selected name and reserved `MCP_CONSOLE_SANDBOX_CONFIG` are removed from all helper and target environments, even if an explicit target map tries to reintroduce them. Neither name may be a private-directory export. The standalone runner also removes them from its own environment before creating threads, so helper libraries cannot interpret a transport value as another setting. Removal prevents accidental propagation; it is neither authentication nor secure erasure. Protection of the accepted policy and host-side control state depends on the selected permissions; unrestricted and external modes have the limitations documented below. When native enforcement is selected, the macOS runner explicitly denies `process-info-pidinfo` queries outside the sandbox, including supervisor launch-environment reads through `KERN_PROCARGS2`, while preserving same-sandbox process inspection.

Target settings are installed after native setup and enforcement. They never configure the host supervisor, helper lookup, loader, temporary setup resources, or proxy policy. On Linux, helper selection uses the trusted launch `PATH`; the target's `PATH` is used for target execution only. Native proxy endpoints take precedence, including their namespace-local translations on Linux. As with any executable, trust the environment used to load the runner itself, including loader settings, and choose a transport name that is not a loader setting. Callers must construct a child-specific environment rather than temporarily mutating a multithreaded parent's environment.

The JSON cap is 1 MiB, subject to the OS's argument/environment limits, including per-string limits on Linux. An oversized initial exec fails in the caller with `E2BIG`, before runner diagnostics can run. An oversized helper or target exec reports the OS error on stderr and returns nonzero. Parse diagnostics report a category and location without echoing configuration values or entire environments. Use the explicit private descriptor mode for configurations too large for environment transport; it cannot bypass the final target's argument/environment limits.

## Framed descriptor configuration

Invoke `mcp-console-sandbox --bootstrap-fd <N>`. Supply exactly that option and one decimal descriptor number greater than 2. The descriptor must already be open and readable in the child; its number need not be 3.

```text
fd 0: original target stdin
fd 1: target stdout
fd 2: target stderr
fd N: [4-byte unsigned big-endian JSON length][UTF-8 JSON]
```

This is a breaking private protocol change. Only version 2 is accepted. The no-argument invocation is rejected, and stdin is never read to discover configuration or select a protocol. There is no compatibility mode or environment-variable fallback.

The launcher normally creates an anonymous pipe, inherits its read end into the executable, closes its own read end, and retains the writer until it sends configuration. Start the executable before writing a potentially pipe-sized frame. There is no descriptor transfer after process creation.

The JSON payload must contain 1 through 1,048,576 bytes. The executable validates the descriptor before opening internal files or creating its runtime, adopts it once, and reads exactly the declared frame while observing cancellation signals. A complete valid request releases startup without waiting for EOF; withholding the frame keeps the target and managed proxy from starting. Closing the writer before completing the frame cancels startup.

In this mode configuration becomes fixed at request acceptance, not process creation. The trusted caller must retain exclusive control of the bytes and descriptor writers until the complete frame is accepted. An anonymous pipe does not authenticate a writer, and a mutable descriptor source can change while being read. The caller may prepare the request after spawning the supervisor. The accepted request is owned memory; the descriptor closes before setup, and further writes or changes to its backing resource cannot alter policy. Neither the caller nor the runner spills configuration to mutable files.

The bootstrap descriptor is closed after parsing and validation, before runtime, proxy, or native setup. It is not a persistent control channel. Empty or truncated frames, invalid lengths, malformed requests, unsupported versions, and invalid invocation arguments return a nonzero status and an error on stderr without launching the target. Remaining request fields retain their native validation.

Stdin, stdout, and stderr are attached directly. The target inherits stdin's original open file description, including its offset, seekability, terminal identity, and binary contents. This executable drops its owned stdin from the launch command immediately after spawning, before waiting for the child. Rust startup supplies `/dev/null` when stdin was closed at invocation, matching the existing runtime behavior.

Neither caller transport accepts later control messages or emits acknowledgments. No protocol output is written to stdout. Native launch failures and target exits retain the native status; signals map to `128 + signal`. Native Linux helper invocations beginning with `--target-setup-fd` or `--sandbox-policy-cwd` dispatch separately, including upstream compatibility re-execs; they do not take or reread a bootstrap. Linux's internal control channel is described at the end of this document.

## Complete JSON reference

This section documents the production types in [bootstrap.rs](src/bootstrap.rs), [config.rs](src/config.rs), and their conversion in [codex.rs](src/codex.rs). Filesystem objects expose upstream `RawFileSystemSandboxPolicy`, `RawFileSystemSandboxEntry`, `RawFileSystemPath`, and `FileSystemSpecialPath`; network exposes `NetworkSandboxPolicy` from [permissions.rs](../protocol/src/permissions.rs). The runner converts filesystem input to `FileSystemSandboxPolicy` and then the canonical `PermissionProfile`. Proxy input is exactly upstream [`RemoteNetworkProxyConfig`](../network-proxy/src/remote_config.rs), with the permission maps from [network-proxy/config.rs](../network-proxy/src/config.rs). Transport, lifecycle, `linux_backend`, and the Seatbelt extension are runner-owned fields.

Only Linux and macOS execute this protocol. Other platforms return an unsupported-platform error. Names and enum strings are case-sensitive. JSON integers must fit the stated Rust integer type; floating-point spellings such as `2.0` are not integers here. Required fields reject both omission and `null`. Optional defaults apply only as stated below: a type's Rust `Default` implementation does not make a JSON field optional.

### Both top-level shapes

| JSON field                         | Type                              | Requiredness, omission and `null`                                                                                            | Meaning and constraints                                                                                                                                                                                                                                                                                                                              |
| ---------------------------------- | --------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `version`                          | `u32` integer                     | Required in both modes; no `null`                                                                                            | Must be `2`.                                                                                                                                                                                                                                                                                                                                         |
| `filesystem`                       | Object below                      | Required in both; no `null`                                                                                                  | Filesystem permissions and ownership of enforcement.                                                                                                                                                                                                                                                                                                 |
| `network`                          | String                            | Required in both; no `null`                                                                                                  | Exactly `"restricted"` or `"enabled"`; no aliases. Independent of filesystem access; see the execution matrix below.                                                                                                                                                                                                                                 |
| `command`                          | Array of strings                  | Required in descriptor mode; rejected in environment mode; no `null`                                                         | At least one element, with a nonempty executable name first. Remaining elements are literal arguments, including empty strings. No shell is inserted. An absolute executable path avoids target `PATH` lookup. Arguments containing NUL fail at process launch.                                                                                      |
| `cwd`                              | String                            | Required in descriptor mode; rejected in environment mode; no `null`                                                         | Upstream `AbsolutePathBuf`: an absolute native directory, not a URI or relative path. Must exist and be accessible at launch. It also supplies the policy's working-directory and single project-root context. Environment mode uses the runner's launch cwd.                                                                                        |
| `environment`                      | Object mapping strings to strings | Descriptor: required complete target environment. Environment mode: optional overrides, default `{}`. Neither accepts `null` | Names must be nonempty, NUL-free, and contain no `=`; values must be NUL-free. Both are UTF-8. Descriptor mode does not inherit ordinary launch variables. Environment mode merges overrides after optional inheritance. Private-storage exports and managed-proxy variables take precedence; transport variables are removed last.                  |
| `inherit_environment`              | Boolean                           | Environment mode only, default `true`; no `null`; rejected in descriptor mode                                                | `false` starts the target environment empty before applying `environment`. Does not change the trusted environment used to launch the runner or its helpers.                                                                                                                                                                                         |
| `proxy`                            | Object below                      | Optional; omission or `null` means no proxy                                                                                  | A supplied object must include all seven required fields below, including `enabled: true`. A managed proxy takes precedence over ordinary direct network access.                                                                                                                                                                                     |
| `lifecycle`                        | Object below                      | Optional, default `{}`; `null` rejected                                                                                      | Supervision, signals, caller-death observation, private storage and retirement deadline.                                                                                                                                                                                                                                                             |
| `linux_backend`                    | String                            | Optional; omitted or `null` uses upstream's ordinary bubblewrap path when native execution is selected                       | Linux only. `"bubblewrap"` explicitly selects that same path. `"landlock"` requests legacy direct execution; its narrower contract is below. No aliases. Any non-null value is rejected on macOS.                                                                                                                                                    |
| `macos_seatbelt_profile_extension` | String                            | Optional; omitted or `null` adds no SBPL                                                                                     | macOS only, and requires native enforcement. Appended trusted Seatbelt rules may grant permissions as well as deny them, including changing filesystem/network restrictions. Empty string is accepted. Invalid SBPL or embedded NUL fails before target execution. Non-null values are rejected on Linux and for external execution without a proxy. |

There are no other top-level fields. In particular, `excluded_environment` is internal and rejected on input. Environment mode takes the command from arguments after `--`, cwd from process creation, and environment from inheritance/overrides. Descriptor mode takes all three from the JSON. Native OS permissions and the executable's own requirements still apply; parsing a request does not establish that it can launch.

### Filesystem object and entries

| JSON field                                   | Type                   | Requiredness, omission and `null`                                       | Meaning                                                                                                                                                                                                                                                                                                                                                                       |
| -------------------------------------------- | ---------------------- | ----------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `filesystem.kind`                            | String                 | Required; no `null`                                                     | Exactly `"restricted"`, `"unrestricted"`, or `"external-sandbox"`. No aliases.                                                                                                                                                                                                                                                                                                |
| `filesystem.entries`                         | Array of entry objects | Optional, default `[]`; `null` rejected                                 | Ordered input to upstream policy resolution. An empty restricted policy supplies no caller-granted file access; native platform defaults are not a general workspace grant.                                                                                                                                                                                                   |
| `filesystem.glob_scan_max_depth`             | `usize` integer        | Optional; omission or `null` means no scan-depth cap                    | Nonnegative, up to the platform's `usize` maximum (64-bit on supported release artifacts). Canonical `PermissionProfile` conversion turns `0` into no cap. Positive values limit Linux startup glob scanning relative to each static search root, and can leave deeper matches unmasked. macOS does not scan and ignores this limit. Ignored for unrestricted/external kinds. |
| `filesystem.entries[].path`                  | Tagged object below    | Required; no `null`                                                     | A concrete path, glob, or special path.                                                                                                                                                                                                                                                                                                                                       |
| `filesystem.entries[].access`                | String                 | Required; no `null`                                                     | `"read"` allows reads; `"write"` allows reads and writes; `"deny"` allows neither. `"none"` is an accepted legacy alias for `"deny"`. A glob supports effective denial only; glob `read`/`write` parses but grants no access.                                                                                                                                                 |
| `filesystem.entries[].missing_path_behavior` | String                 | Optional; omission or `null` uses ordinary native missing-path handling | Only `"skip"` is accepted. This upstream marker is preserved, but these native Linux/macOS launch paths do not implement a general skip-missing pass. Do not rely on it to suppress deny/read masks or to create writable directories.                                                                                                                                        |

For `unrestricted` and `external-sandbox`, entries still deserialize and their raw paths still undergo conversion, so malformed entries can fail. Canonical permissions then discard entries and `glob_scan_max_depth`; they cannot narrow these kinds. Use `restricted` when entries should enforce restrictions. This also means unrestricted execution does not add the runner's private-directory write restrictions on macOS.

`path` is an internally tagged union. Each row is a complete nested shape; only its listed payload fields are required:

| `path.type`      | Exact object shape                                                    | Constraints and interpretation                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| ---------------- | --------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `"path"`         | `{"type":"path","path":"/absolute/path"}`                             | `path` is a required string, no `null`. The raw string first converts as a native absolute path, then through upstream's legacy path-to-URI conversion. Absolute native paths are usable; relative native paths, `~` and URI strings (including `file:///tmp`) fail conversion. No shell or environment expansion. Foreign/remote paths may convert but cannot grant host-native access when they cannot resolve on this executor. Use absolute host paths for portable runner requests. |
| `"glob_pattern"` | `{"type":"glob_pattern","pattern":"/absolute/directory/**/*.secret"}` | `pattern` is a required string, no `null`. Relative patterns resolve against policy cwd. No shell expansion. See platform limits below; not every string that parses can be enforced as a glob.                                                                                                                                                                                                                                                                                          |
| `"special"`      | `{"type":"special","value":{"kind":"root"}}`                          | `value` is a required object, no `null`, with the following `kind` union.                                                                                                                                                                                                                                                                                                                                                                                                                |

All special-path variants:

| `path.value.kind`             | Additional fields                                                                        | Meaning on this runner                                                                                                                                                                                                                                                                                                                                                                             |
| ----------------------------- | ---------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `"root"`                      | None                                                                                     | Host filesystem root `/`. `read` makes the root readable; `write` can produce full disk write access even with filesystem kind `restricted` when no narrower effective entries exist.                                                                                                                                                                                                              |
| `"minimal"`                   | None                                                                                     | Include upstream platform-default readable runtime paths when access is `read` or `write`; it does not grant general writes. Linux includes system binary/library/configuration roots such as `/usr`, `/bin`, `/lib`, `/etc` and Nix system roots, plus minimal devices. macOS uses its upstream platform defaults. It is not an assurance that an arbitrary interpreter or application can start. |
| `"project_roots"`             | Optional `subpath`: string or `null`, default absent                                     | This standalone runner has one project root: policy cwd. Absent/null refers to that directory. `subpath` is resolved against it with upstream path rules, not string concatenation. Use a relative subpath within cwd; the raw type also parses absolute/traversal strings, and this native adapter does not validate containment.                                                                 |
| `"current_working_directory"` | Same `subpath` field                                                                     | Accepted alias for `"project_roots"`.                                                                                                                                                                                                                                                                                                                                                              |
| `"tmpdir"`                    | None                                                                                     | Absolute, nonempty `TMPDIR` from the trusted helper/runner environment. Unset, empty, or relative values resolve to no location. Does not mean the target's override or private-storage export.                                                                                                                                                                                                    |
| `"slash_tmp"`                 | None                                                                                     | `/tmp` if it exists as a directory. Both supported platforms are Unix.                                                                                                                                                                                                                                                                                                                             |
| `"unknown"`                   | Required `path`: string, no `null`; optional `subpath`: string or `null`, default absent | Preserves a future special-path token as data but grants no concrete path. Other literal `kind` strings are rejected; the forward-compatible representation must explicitly use `"unknown"`.                                                                                                                                                                                                       |

For example, `{"type":"special","value":{"kind":"unknown","path":":future","subpath":null}}` is accepted and ignored as a path grant. No other aliases or special variants exist.

Concrete entries apply to a path and its descendants. More specific entries override ancestors; equally specific entries use `deny` over `write` over `read`, independently of array order. Upstream normalizes filesystem aliases and resolves symlinks when building native roots; an allowed spelling is not an unrestricted grant through arbitrary symlinks. Writable roots retain upstream protections for top-level `.git`, `.agents`, and `.codex` metadata and applicable Git pointer files. Explicit policy carveouts use the upstream resolution rules; unrestricted kinds discard those protections.

Missing paths retain native semantics. Linux skips absent writable roots instead of creating them. Read/deny carveouts under writable roots can mask the first missing component and prevent its later creation; native setup may create temporary mount placeholders. macOS can describe permissions for paths that do not exist yet. Neither an ordinary entry nor `missing_path_behavior: "skip"` promises the same missing-path result on every backend.

Globs describe git-style patterns (`*`, `?`, `**`, character classes and supported brace/escape forms); use a directory prefix and conventional `**/*.suffix` patterns. Linux expands existing file matches during setup, includes hidden/ignored files, does not recurse through symlink directories, and masks matches plus resolved symlink targets. It uses upstream ripgrep or its built-in walker when ripgrep is absent; other scan failures remain errors. The expansion is capped at 8,192 paths and requires a non-root static directory prefix and a glob metacharacter recognized by the native splitter (`*`, `?`, `[` or `]`). A literal string in `glob_pattern`, or `/*.secret`, can therefore parse and fail at Linux setup. Future files and matches beyond a selected depth are not dynamically denied by this mount snapshot. macOS translates its supported glob subset into Seatbelt read/write denies, with ancestor unlink protection, and applies it to subsequent accesses without scanning. Unsupported patterns need not have identical results across these native implementations. Landlock cannot enforce deny-read globs or other restricted-read policies; use concrete scoped policies or the default backend for those requirements.

### Network, proxy, and enforcement selection

| Filesystem kind    | No proxy, `network: "restricted"`                                 | No proxy, `network: "enabled"`                                    | Any supplied enabled proxy                                                                                                |
| ------------------ | ----------------------------------------------------------------- | ----------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `restricted`       | Native filesystem and network restrictions                        | Native filesystem restrictions; native full-network policy        | Native filesystem rules and managed proxy routing, with either network value                                              |
| `unrestricted`     | Full filesystem access with native network restrictions           | Full filesystem access with native full-network policy            | Full filesystem access with managed proxy routing, with either network value                                              |
| `external-sandbox` | Filesystem and network enforcement delegated to the outer sandbox | Filesystem and network enforcement delegated to the outer sandbox | Filesystem enforcement delegated; runner supplies native managed-network enforcement and proxy, with either network value |

Managed full access still launches through the native path for lifecycle support: Linux retains bubblewrap namespace init and its setup/control exchange even with full networking; macOS retains its native process profile. It does not mean unrestricted host devices, privileges, or every operating-system operation. Native restrictions, including capabilities and process/session behavior, remain those of the selected backend. Restricted networking is never changed to enabled merely because filesystem access is unrestricted.

External enforcement uses canonical `PermissionProfile::External`. Without a proxy, upstream automatic selection chooses no native sandbox. The runner supplies target environment, stdio, signal restoration/forwarding, ordinary process supervision, caller-death handling and optional private-storage cleanup. It does not create an outer sandbox, verify one exists, or enforce the declared restricted network itself. Linux retires the original process group and waits for its direct child; detached descendants belong to the outer sandbox's lifecycle contract. macOS retains its existing observation of descendants and live group members, with its documented observation limits. A managed proxy makes upstream selection require a native sandbox for routing. An explicit `linux_backend: "bubblewrap"` does not force native enforcement for external execution without a proxy; the legacy `"landlock"` override is rejected with external enforcement.

`linux_backend` is optional. Normal callers should omit it and specify policies. Explicit `"landlock"` preserves upstream legacy direct-exec capability: native Landlock/seccomp enforcement followed by replacement of the runner, with no waiting supervisor or PID namespace. It rejects proxy configuration, external enforcement, `parent_pid`, `private_tmp`, an explicit `cleanup_timeout_ms`, or `sigterm: "retire"`; empty/default lifecycle is accepted. It also rejects policies requiring restricted reads or finer denial than the legacy representation supports. Restricted writes require Landlock ABI 3 or newer for truncate enforcement. Filesystem-unrestricted Landlock execution needs no filesystem rules, but still applies the chosen native network policy. There is no automatic backend fallback after namespace or enforcement failure.

The complete `proxy` object uses camelCase field names. Unlike `NetworkProxyConfig`, `RemoteNetworkProxyConfig` has **no defaults for its Boolean or mode fields**:

| JSON field                             | Type                              | Requiredness, omission and `null`                                                          | Meaning and interactions                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| -------------------------------------- | --------------------------------- | ------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `proxy.enabled`                        | Boolean                           | Required; no `null`                                                                        | Must be `true`. `false` parses but is rejected by executor-local proxy setup. Use top-level omission/null for no proxy.                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `proxy.enableSocks5`                   | Boolean                           | Required; no `null`                                                                        | Starts the SOCKS5 listener in addition to HTTP when true.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `proxy.enableSocks5Udp`                | Boolean                           | Required; no `null`                                                                        | Enables upstream SOCKS5 UDP association support. No effect without SOCKS5; limited mode denies UDP. Linux's managed bridge carries TCP only, so true parses but does not supply usable UDP routing there. macOS UDP use remains subject to native Seatbelt permissions; this field alone does not grant the dynamic UDP relay ports.                                                                                                                                                                                                     |
| `proxy.allowUpstreamProxy`             | Boolean                           | Required; no `null`                                                                        | Allows the host proxy implementation to use an upstream proxy from the runner's trusted environment. Target environment overrides cannot configure this hop. Host/domain policy remains checked, but the upstream proxy controls its own resolution and onward connection.                                                                                                                                                                                                                                                               |
| `proxy.dangerouslyAllowAllUnixSockets` | Boolean                           | Required; no `null`                                                                        | Requests upstream access to all Unix-domain sockets instead of an allowlist. This can reach host services with authority outside the sandbox. macOS's native policy and HTTP-to-Unix support use it; it does not add Linux Unix-socket routing that upstream lacks.                                                                                                                                                                                                                                                                      |
| `proxy.mode`                           | String                            | Required; no `null`                                                                        | `"full"` permits all HTTP methods, HTTPS CONNECT tunnels and SOCKS5 TCP subject to policy. `"limited"` permits HTTP GET/HEAD/OPTIONS only. This runner has no MITM, so limited mode denies HTTPS CONNECT, SOCKS5 TCP and UDP. No aliases.                                                                                                                                                                                                                                                                                                |
| `proxy.domains`                        | Object mapping strings to strings | Optional; omission or `null` means no domain entries; `{}` likewise allows no destinations | Keys are host patterns; values are exactly `"allow"`, `"deny"`, or `"none"`. `none` adds neither allow nor deny; it is not a deny rule or an exemption. Null values and non-strings are rejected. Details below.                                                                                                                                                                                                                                                                                                                         |
| `proxy.unixSockets`                    | Object mapping strings to strings | Optional; omission or `null` means no allowed sockets; `{}` is empty                       | Keys are socket paths; values are exactly `"allow"` or `"deny"`. No aliases or null values. Allowed paths must be absolute; relative allowed paths fail proxy setup. These are path strings, not glob rules. Only allow entries are projected into the allowlist; deny entries do not override `dangerouslyAllowAllUnixSockets`. Platform filesystem permissions must also allow access.                                                                                                                                                 |
| `proxy.allowLocalBinding`              | Boolean                           | Required; no `null`                                                                        | Allows upstream local/private-network behavior. False allows explicitly allowlisted local literals but blocks hostnames resolving to non-public addresses, even if allowlisted. True removes that extra check but still requires domain allowlisting for proxied requests. On macOS it also permits native local binding, inbound/outbound loopback traffic, and DNS on port 53 when proxy listeners exist. Linux remains in its separate network namespace; this field does not expose host loopback or add direct host-network routes. |

Domain matching uses upstream normalized hostnames, not URLs or URL paths, and does not restrict destination ports. Exact `example.com` matches only that host; `*.example.com` matches subdomains at any depth but not the apex; `**.example.com` includes both apex and subdomains. `*` is accepted in the allowlist and rejected in the denylist at proxy setup. Host/pattern normalization trims whitespace, lowercases DNS names, strips trailing dots and host ports, and handles bracketed/normalized IP literals. Malformed glob syntax can fail setup. Explicit matching deny rules win over any allow rule, regardless of specificity. With no matching allow rule, the request is denied; no interactive approval service is provided. Local/private addresses have the additional rules above; an explicit local literal allow does not follow from a broad wildcard alone.

The proxy chooses ephemeral loopback listeners. This wire object exposes no listener addresses, fixed ports, MITM settings, credential injection, hooks, audit metadata, reload control, or remote policy-decider configuration. These settings are not nested extension points: unknown upstream fields are ignored as described below and cannot enable those capabilities.

### Lifecycle object

| JSON field                          | Type                    | Requiredness, omission and `null`                                       | Meaning and constraints                                                                                                                                                                                                                                                                                                                                  |
| ----------------------------------- | ----------------------- | ----------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `lifecycle.parent_pid`              | `i32` integer or `null` | Optional; omitted/null disables caller-death observation                | Must be greater than 1 and identify the runner's live direct parent at capture. An invalid/already-dead parent fails launch. Observed parent death requests retirement and exit 0.                                                                                                                                                                       |
| `lifecycle.sigterm`                 | String                  | Optional, default `"forward"`; `null` rejected                          | `"forward"` forwards according to inherited signal state. `"retire"` requests retirement and exit 0 even if TERM was inherited ignored or blocked. No aliases. Other forwarded signals are HUP, INT and QUIT.                                                                                                                                            |
| `lifecycle.cleanup_timeout_ms`      | `u64` integer or `null` | Optional; omitted/null means 1000                                       | Explicit values must be 1 through 60000 inclusive. Bounds retirement after exit/cancellation, not target execution or filesystem deletion.                                                                                                                                                                                                               |
| `lifecycle.private_tmp`             | Object or `null`        | Optional; omitted/null creates nothing                                  | Creates a mode-0700 private container and writable `data` child; exports exactly the requested names.                                                                                                                                                                                                                                                    |
| `lifecycle.private_tmp.parent`      | String or `null`        | Optional; omitted/null uses the supervisor's native temporary directory | If supplied, must be absolute. Parent directory must already exist and be writable; canonicalized before creation. Target TMPDIR overrides do not select it.                                                                                                                                                                                             |
| `lifecycle.private_tmp.environment` | Array of strings        | Required when private_tmp is an object; no `null`                       | Names receive the absolute data path, overriding ordinary target values. Empty list accepted; duplicate names are harmless repeated assignments. Names must be nonempty, NUL-free, and contain no `=`. The selected transport variable and reserved `MCP_CONSOLE_SANDBOX_CONFIG` are rejected as exports. Proxy projection can override colliding names. |

Ordinary execution, signal forwarding, configured shutdown and caller death retain the existing managed lifecycle. Private data is removed after retirement, including after a nonzero target exit. Detected retirement/deletion failures produce nonzero status and diagnostics; incomplete retirement retains private storage. With unrestricted filesystem access, a workload can alter shared writable files and interfere with helpers, the supervisor, or other same-user processes indirectly. Depending on OS/procfs/process protections it may reach additional authority. This can undermine network isolation or other restrictions through an unsandboxed process; residual files are not the only possible consequence. Native network enforcement remains installed for managed execution, but unrestricted files are not an adversarial isolation guarantee.

Crashes, SIGKILL, abrupt caller loss during startup, and malicious interference can leave processes or temporary files. Fresh procfs is not required to select unrestricted access, and inherited procfs can expose host process metadata and paths. Stronger supervisor/control protections tested with restricted filesystem policies do not transfer to unrestricted or externally enforced policies. The [lifecycle document](LIFECYCLE.md) details ordinary ordering and OS limits; [Linux compatibility](LINUX_COMPATIBILITY.md) records probes and backend prerequisites.

### Unknown and duplicate fields

Top-level objects, `lifecycle`, and `private_tmp` reject unknown fields and duplicate recognized fields. Upstream filesystem policy/entry structs, path/special tagged objects, and `proxy` accept and ignore unknown fields. They reject duplicate recognized struct fields and duplicate discriminators; fields belonging to other union variants are unknown for the selected variant. There is no catch-all for unrecognized enum values except the explicit special-path `unknown` representation above.

The `environment`, `domains`, and `unixSockets` maps accept arbitrary string keys subject to their value/path validation. Repeated identical JSON map keys keep the last value, including in domain maps; this is separate from deny-over-allow precedence between distinct matching patterns. Do not use duplicate keys to combine permissions. Different spellings that normalize to the same host can both contribute patterns, with a matching deny taking precedence.

### Complete examples

Each JSON block below is a complete payload. Environment examples can be placed verbatim in a child-only `SANDBOX_REQUEST` value and invoked with `mcp-console-sandbox --config-env SANDBOX_REQUEST -- /bin/echo 'sandbox ready'`. Descriptor examples include `command`, `cwd` and `environment`; encode them with the length prefix described above and use `--bootstrap-fd`. They assume `/tmp` and `/var/tmp` exist.

Environment mode, unrestricted files with restricted networking:

```json
{
  "version": 2,
  "filesystem": { "kind": "unrestricted" },
  "network": "restricted"
}
```

Environment mode, unrestricted files and enabled networking, with ordinary private-storage cleanup:

```json
{
  "version": 2,
  "filesystem": { "kind": "unrestricted" },
  "network": "enabled",
  "inherit_environment": false,
  "environment": { "PATH": "/usr/bin:/bin" },
  "lifecycle": {
    "sigterm": "retire",
    "private_tmp": { "parent": "/tmp", "environment": ["TMPDIR"] },
    "cleanup_timeout_ms": 1000
  }
}
```

Descriptor mode, root-readable filesystem, writable cwd, and an additional writable directory:

```json
{
  "version": 2,
  "command": ["/bin/echo", "sandbox ready"],
  "cwd": "/tmp",
  "environment": { "PATH": "/usr/bin:/bin" },
  "filesystem": {
    "kind": "restricted",
    "entries": [
      {
        "path": { "type": "special", "value": { "kind": "root" } },
        "access": "read"
      },
      {
        "path": { "type": "special", "value": { "kind": "project_roots" } },
        "access": "write"
      },
      { "path": { "type": "path", "path": "/var/tmp" }, "access": "write" }
    ]
  },
  "network": "restricted",
  "proxy": null
}
```

Environment mode, unrestricted files with an enforced managed proxy:

```json
{
  "version": 2,
  "filesystem": { "kind": "unrestricted" },
  "network": "restricted",
  "proxy": {
    "enabled": true,
    "enableSocks5": true,
    "enableSocks5Udp": false,
    "allowUpstreamProxy": false,
    "dangerouslyAllowAllUnixSockets": false,
    "mode": "full",
    "domains": {
      "**.example.com": "allow",
      "blocked.example.com": "deny",
      "unused.example.org": "none"
    },
    "unixSockets": {},
    "allowLocalBinding": false
  }
}
```

Descriptor mode, outer sandbox responsible for filesystem and restricted networking. Without such an outer restriction, this command has ordinary host network access:

```json
{
  "version": 2,
  "command": ["/bin/echo", "sandbox ready"],
  "cwd": "/tmp",
  "environment": {},
  "filesystem": { "kind": "external-sandbox" },
  "network": "restricted",
  "lifecycle": { "sigterm": "retire" }
}
```

Environment mode, explicit legacy Landlock on Linux, unrestricted files with native restricted networking and no supervised lifecycle:

```json
{
  "version": 2,
  "filesystem": { "kind": "unrestricted" },
  "network": "restricted",
  "linux_backend": "landlock"
}
```

## Network and target environment

Native platform or target initialization may add environment values: some macOS toolchains set `__CF_USER_TEXT_ENCODING` before target `main`, and Linux bubblewrap sets `PWD` to the command directory. When a proxy is present, upstream adds or replaces these target variables:

| Target-visible variables                                                                                                                                                                                                                      | Native proxy value                                                                                                               |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `HTTP_PROXY`, `HTTPS_PROXY`, `http_proxy`, `https_proxy`, `YARN_HTTP_PROXY`, `YARN_HTTPS_PROXY`, `npm_config_http_proxy`, `npm_config_https_proxy`, `npm_config_proxy`, `NPM_CONFIG_HTTP_PROXY`, `NPM_CONFIG_HTTPS_PROXY`, `NPM_CONFIG_PROXY` | Managed HTTP endpoint.                                                                                                           |
| `BUNDLE_HTTP_PROXY`, `BUNDLE_HTTPS_PROXY`, `PIP_PROXY`, `DOCKER_HTTP_PROXY`, `DOCKER_HTTPS_PROXY`, `WS_PROXY`, `WSS_PROXY`, `ws_proxy`, `wss_proxy`                                                                                           | Managed HTTP endpoint.                                                                                                           |
| `ALL_PROXY`, `all_proxy`, `FTP_PROXY`, `ftp_proxy`                                                                                                                                                                                            | Managed `socks5h` endpoint when SOCKS is enabled; otherwise HTTP.                                                                |
| `NO_PROXY`, `no_proxy`, `npm_config_noproxy`, `NPM_CONFIG_NOPROXY`, `YARN_NO_PROXY`, `BUNDLE_NO_PROXY`                                                                                                                                        | Empty when local binding is disabled; otherwise `localhost,127.0.0.1,::1,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16`.               |
| `CODEX_NETWORK_PROXY_ACTIVE`, `CODEX_NETWORK_ALLOW_LOCAL_BINDING`                                                                                                                                                                             | `1` for proxy active; `1` or `0` for the supplied local-binding policy.                                                          |
| `ELECTRON_GET_USE_PROXY`, `NODE_USE_ENV_PROXY`                                                                                                                                                                                                | `true` and `1`, respectively.                                                                                                    |
| `GIT_SSH_COMMAND` on macOS with SOCKS enabled                                                                                                                                                                                                 | `CODEX_PROXY_GIT_SSH_COMMAND=1 ssh -o ProxyCommand='nc -X 5 -x <SOCKS address> %h %p'`. An existing custom command is preserved. |

Linux rewrites endpoint ports for the target network namespace. This executor-local path adds no CA, credential, or attribution variables and removes stale `CODEX_NETWORK_PROXY_CREDENTIAL_BROKER_ACTIVE`, `CODEX_NETWORK_PROXY_BROKERED_CREDENTIALS`, and `CODEX_NETWORK_PROXY_ATTRIBUTION_TOKEN` values. The upstream [environment projection](../network-proxy/src/proxy.rs) defines the complete behavior.

Without a managed proxy, Linux restricted networking permits Unix socket pairs and descriptor `read`/`write`, but denies `sendto`, `shutdown`, `getsockname`, `getpeername`, `getsockopt`, and `setsockopt`, including on local pairs. Connecting, binding, listening, and creating IP sockets remain denied. Creating a local pair therefore does not establish that socket-specific I/O methods work. Application sidebands must use permitted descriptor I/O with explicit cancellation, or pipes with independently owned directions. The [historical sideband handoff](https://github.com/t-kalinowski/codex/blob/29b89360756250c94c81005e8902ab8882a8404d/codex-rs/mcp-console-sandbox/MCP_CONSOLE_HANDOFF.md) records the downstream design and acceptance criteria at its original revision; it is not a current task list.

## Caller example

[examples/bootstrap.py](examples/bootstrap.py) is a runnable Python 3.10+ caller using `os.pipe` and `subprocess.Popen(pass_fds=...)`. It inherits only the bootstrap read descriptor beyond stdio, starts before writing the frame, closes unused pipe ends, and waits for the target through the native sandbox executable. Failure closes the pipe and kills and reaps the immediate child.

From this package directory:

```sh
printf 'target input\n' | python3 examples/bootstrap.py /absolute/path/to/mcp-console-sandbox
```

The example leaves target stdin directly attached and sends configuration independently. An application can delay the write until its request is ready. Lifecycle capabilities are selected in the same request.

The outer invocation still marks unrelated inherited descriptors above stderr close-on-exec before starting the runtime. The bootstrap descriptor is explicitly closed before native setup. Native helper re-execs retain their own temporary setup descriptors until the upstream code releases them.

## Downstream handoff

A thin frontend can exec the environment-mode invocation with its ordinary command arguments, cwd, and target environment. It should explicitly select its private-directory exports, parent PID if retirement on caller death is wanted, SIGTERM behavior, filesystem/network rules, and any trusted macOS extension.

The runner owns launch, signal restoration, optional private storage, and proxy lifetime; descendant retirement follows the selected managed or external execution contract. An application-level fork-and-continue manager and sandbox-target signal wrapper are unnecessary. Large requests may retain the private descriptor mode. The coordinated downstream integration and procfs probes are described in [Linux compatibility](LINUX_COMPATIBILITY.md).

The Linux native control channel is private to the runner and namespace init. It carries signals after one-shot setup, never configuration updates, and is close-on-exec in the workload. This does not change either caller transport. Explicit Landlock transports its target environment across trusted setup in a sealed anonymous file and closes that descriptor before target execution; no waiting supervisor remains.
