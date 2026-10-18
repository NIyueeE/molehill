# pyright: strict
"""Where the bench project, the checkout it measures and the user live."""

from __future__ import annotations

import contextlib
import os
import pwd
from pathlib import Path

#: The installed package (`bench/src/bench`).
PACKAGE_DIR = Path(__file__).resolve().parent
#: The uv project (`bench/`), which owns `pyproject.toml` and `uv.lock`.
PROJECT_ROOT = PACKAGE_DIR.parents[1]
#: The molehill checkout the model measures.
REPO_ROOT = PROJECT_ROOT.parent
#: The non-model utilities that ship with the project.
UTILS_DIR = PACKAGE_DIR / "utils"
#: The binary a run measures unless `--binary` names another build.
DEFAULT_BINARY = REPO_ROOT / "target" / "release" / "molehill"


def user_home() -> Path:
    """The *invoking* user's home directory.

    `sudo` sets `HOME=/root`, so a `sudo just bench` would drop its results in
    a directory the person who ran it cannot read. The results are theirs, so
    they land in their home.
    """
    user = os.environ.get("SUDO_USER")
    if user:
        with contextlib.suppress(KeyError, OSError):
            return Path(pwd.getpwnam(user).pw_dir)
    return Path.home()


#: Where the peer fetcher installs the reference tools, and where a run looks
#: for them unless `--peer-dir` says otherwise.
DEFAULT_PEER_DIR = user_home() / "tmp" / "bench-peers"
