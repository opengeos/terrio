"""Attribute warnings recorded by Rust to the public API's Python caller."""

import sys
import warnings
from types import FrameType


def warn_user(message: str) -> None:
    """Skip Terrio wrapper frames on Python 3.10 and later."""
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
