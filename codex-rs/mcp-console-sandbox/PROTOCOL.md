# Bootstrap protocol version 1

Stdin consists of a four-byte unsigned big-endian JSON byte count, that many UTF-8 JSON bytes, and then ordinary target input. The JSON payload must contain between 1 and 1,048,576 bytes. The caller may write all three parts immediately.

The runner reads the frame directly from fd 0, requesting only the remaining header or payload bytes. After one request it transfers that same stdin to the target. There are no later control messages, stream frames, acknowledgments, or success output. Stdout and stderr are inherited directly. Bootstrap and launch errors are written to stderr and return a nonzero exit status.

The bootstrap contains a version, a nonempty command argument vector, an absolute working directory, a complete environment map, the upstream raw filesystem policy, the upstream network sandbox policy, and optional upstream executor-local managed proxy configuration. Network permission and proxy configuration remain separate inputs.

Version 1 requires UTF-8 command arguments, paths, and environment values. It accepts no application lifecycle policy or passed target stream descriptors. The target inherits only stdio and descriptors required by the native sandbox.

The runner waits for the native launch result and preserves its exit code or signal mapping. It adds no process-tree supervision or retirement policy.
