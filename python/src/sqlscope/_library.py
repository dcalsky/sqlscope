"""Loads the sqlscope FFI library and calls into it through ctypes."""

from __future__ import annotations

import ctypes
import json
import os
import sys
import threading
from pathlib import Path
from typing import Any, Dict, Optional

LIBRARY_PATH_ENV = "SQLSCOPE_LIBRARY_PATH"
"""Environment variable that locates the library when :func:`load` gets no path."""

_ABI_VERSION = 1


class Error(Exception):
    """Base class of every sqlscope error."""


class InvalidArgumentError(Error):
    """An option or argument is invalid."""


class ParseError(Error):
    """The SQL text could not be parsed."""


class UnsupportedError(Error):
    """The statement shape is not supported, or the input exceeded a safety limit."""


class InternalError(Error):
    """sqlscope produced an invalid result (a bug)."""


class LibraryNotFoundError(Error, OSError):
    """The sqlscope FFI library could not be loaded."""


_ERRORS = {
    "invalid_argument": InvalidArgumentError,
    "parse": ParseError,
    "unsupported": UnsupportedError,
}


def library_file_name() -> str:
    """The platform's file name of the sqlscope shared library."""
    if sys.platform == "win32":
        return "sqlscope_ffi.dll"
    if sys.platform == "darwin":
        return "libsqlscope_ffi.dylib"
    return "libsqlscope_ffi.so"


class _Library:
    def __init__(self, path: str) -> None:
        try:
            lib = ctypes.CDLL(path)
        except OSError as error:
            raise LibraryNotFoundError(f"cannot load {path}: {error}") from error
        try:
            lib.sqlscope_abi_version.restype = ctypes.c_uint32
            lib.sqlscope_abi_version.argtypes = []
            lib.sqlscope_version.restype = ctypes.c_char_p
            lib.sqlscope_version.argtypes = []
            lib.sqlscope_call.restype = ctypes.c_void_p
            lib.sqlscope_call.argtypes = [ctypes.c_char_p, ctypes.c_char_p]
            lib.sqlscope_free.restype = None
            lib.sqlscope_free.argtypes = [ctypes.c_void_p]
        except AttributeError as error:
            raise LibraryNotFoundError(f"{path} is not a sqlscope library: {error}") from error
        abi = lib.sqlscope_abi_version()
        if abi != _ABI_VERSION:
            raise LibraryNotFoundError(f"{path} implements ABI {abi}, expected {_ABI_VERSION}")
        self.path = path
        self.version: str = lib.sqlscope_version().decode()
        self._lib = lib

    def call(self, operation: str, request: bytes) -> bytes:
        # ctypes releases the GIL for the duration of the call.
        response = self._lib.sqlscope_call(operation.encode(), request)
        try:
            return ctypes.string_at(response)
        finally:
            self._lib.sqlscope_free(response)


_loaded: Optional[_Library] = None
_lock = threading.Lock()


def _default_candidates() -> list[str]:
    path = os.environ.get(LIBRARY_PATH_ENV)
    if path:
        return [path]
    name = library_file_name()
    return [str(Path(__file__).resolve().parent / name), name]


def load(path: Optional[str] = None) -> None:
    """Load the sqlscope FFI library, the shared library published with each sqlscope release.

    Without ``path`` it tries, in order, the path in ``$SQLSCOPE_LIBRARY_PATH``,
    :func:`library_file_name` inside the ``sqlscope`` package directory, and
    :func:`library_file_name` on the system library search path. Nothing is
    ever downloaded.

    Calling ``load`` is optional: the first operation loads the library the
    same way. Once a library is loaded it stays loaded; loading a different
    path afterwards raises :class:`LibraryNotFoundError`.
    """
    global _loaded
    with _lock:
        if _loaded is not None:
            if path is None or os.fspath(path) == _loaded.path:
                return
            raise LibraryNotFoundError(f"sqlscope library already loaded from {_loaded.path}")
        if path is not None:
            _loaded = _Library(os.fspath(path))
            return
        failures = []
        for candidate in _default_candidates():
            try:
                _loaded = _Library(candidate)
                return
            except LibraryNotFoundError as error:
                failures.append(str(error))
        raise LibraryNotFoundError(
            f"cannot load {library_file_name()}; download it from a sqlscope release and pass its "
            f"path to sqlscope.load() or set {LIBRARY_PATH_ENV}: " + "; ".join(failures)
        )


def _library() -> _Library:
    if _loaded is None:
        load()
    assert _loaded is not None
    return _loaded


def library_version() -> str:
    """The version of the loaded sqlscope FFI library, loading it if needed."""
    return _library().version


def call(operation: str, request: Dict[str, Any]) -> Any:
    """Run one operation and return its decoded result, raising on error."""
    payload = json.dumps(request).encode()
    response = json.loads(_library().call(operation, payload))
    error = response.get("error")
    if error is not None:
        raise _ERRORS.get(error["kind"], InternalError)(error["message"])
    return response["ok"]
