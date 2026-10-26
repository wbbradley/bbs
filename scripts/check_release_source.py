#!/usr/bin/env python3
"""Reject changed release sources before packaging the workspace."""

from pathlib import Path
import subprocess


def check(root):
    changes = subprocess.check_output(
        ["git", "status", "--porcelain", "--untracked-files=all"], cwd=root,
    )
    if changes:
        raise RuntimeError("release sources are not clean:\n" + changes.decode())


if __name__ == "__main__":
    check(Path(__file__).resolve().parent.parent)
