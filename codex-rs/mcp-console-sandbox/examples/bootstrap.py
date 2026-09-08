"""Run /bin/cat with direct standard streams and a separate bootstrap pipe."""

import json
import os
import subprocess
import sys


def launch(executable: str) -> int:
    request = {
        "version": 2,
        "command": ["/bin/cat"],
        "cwd": "/tmp",
        "environment": {},
        "filesystem": {
            "kind": "restricted",
            "entries": [
                {
                    "path": {"type": "special", "value": {"kind": "root"}},
                    "access": "read",
                }
            ],
        },
        "network": "restricted",
        "proxy": None,
        "macos_seatbelt_profile_extension": None,
    }
    payload = json.dumps(request).encode("utf-8")
    assert 0 < len(payload) <= 1024 * 1024
    read_fd, write_fd = os.pipe()
    child: subprocess.Popen[bytes] | None = None
    try:
        with os.fdopen(read_fd, "rb") as reader, os.fdopen(write_fd, "wb") as writer:
            # Start before writing: a large frame may exceed the pipe's capacity.
            # Only bootstrap is inherited beyond stdio; stdin is already attached.
            child = subprocess.Popen(
                [executable, "--bootstrap-fd", str(reader.fileno())],
                pass_fds=(reader.fileno(),),
                close_fds=True,
            )
            reader.close()
            writer.write(len(payload).to_bytes(4, "big"))
            writer.write(payload)
            writer.close()
            return child.wait()
    except BaseException:
        # Closing a partial bootstrap cancels setup. Reap our child on failure;
        # applications own any broader cancellation and descendant supervision.
        if child is not None:
            child.kill()
            child.wait()
        raise


if __name__ == "__main__":
    assert len(sys.argv) == 2, (
        "usage: bootstrap.py /absolute/path/to/mcp-console-sandbox"
    )
    raise SystemExit(launch(sys.argv[1]))
