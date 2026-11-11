"""Attribute warnings recorded by Rust to the public API's Python caller."""

import sys
import warnings
from types import FrameType


def warn_user(message: str) -> None:
    """Attribute a UserWarning to the first Python frame outside Terrio."""
    frame: FrameType | None = sys._getframe(1)
    stacklevel = 2
    while frame is not None:
        module = frame.f_globals.get("__name__", "")
        if not isinstance(module, str) or (module != "terrio" and not module.startswith("terrio.")):
            break
        stacklevel += 1
        frame = frame.f_back
    del frame
    warnings.warn(message, UserWarning, stacklevel=stacklevel)
