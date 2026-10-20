"""The ``terrio`` command line: ``terrio info dem.tif``, or ``python -m terrio info dem.tif``.

The same command as the ``terrio`` binary of ``cargo install terrio-cli``, run from the extension
module, so that ``pip install terrio`` installs it without a Rust toolchain.
"""

import signal
import sys


def main() -> int:
    """Runs the ``terrio`` command line with the process's arguments.

    Returns:
        The exit code: 0 on success, 1 when the command fails, 2 for a usage error.
    """
    from terrio._terrio import cli_main

    # Behave as the native binary does: exit quietly when the reader of the output goes away
    # (`terrio formats | head`), and stop on Ctrl-C, which Python would only see once the command
    # returns.
    if hasattr(signal, "SIGPIPE"):
        signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    signal.signal(signal.SIGINT, signal.SIG_DFL)
    sys.stdout.flush()
    return cli_main(["terrio", *sys.argv[1:]])


if __name__ == "__main__":
    sys.exit(main())
