# Standalone native sandbox runner

`mcp-console-sandbox` extracts the native sandbox in this release into a small standalone executable. A caller starts it as an ordinary child process and sends one bootstrap frame on stdin. The requested command is an opaque process with inherited stdin, stdout, and stderr.

The executable boundary is the integration contract. Callers do not link to or call the Rust sandboxing crates. Inside the executable, policy conversion, the optional managed proxy, and sandbox launch use the release's existing implementations and ordinary sandbox profile.

The runner owns bootstrap validation, native sandbox and proxy setup, one command launch, its wait result, and resources created by those native paths. Generation lifetime, parent monitoring, restarts, retirement, application temporary directories, and backend lifecycle management belong to the caller.

See [PROTOCOL.md](PROTOCOL.md) for the one-shot wire contract and [REBASE.md](REBASE.md) for the rolling patch audit.
