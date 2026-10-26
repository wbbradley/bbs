#!/usr/bin/env python3
"""Rebuild the server's browser assets and check, or update, the committed copy."""

import argparse
from pathlib import Path
import shutil
import subprocess
import tempfile


def files(directory):
    return {
        path.relative_to(directory): path.read_bytes()
        for path in directory.rglob("*") if path.is_file()
    }


def build(update=False):
    root = Path(__file__).resolve().parent.parent
    destination = root / "crates/bbs-server/web/dist"
    with tempfile.TemporaryDirectory(prefix="bbs-web-build-") as temporary:
        output = Path(temporary) / "dist"
        subprocess.run(
            ["npm", "run", "build", "--", "--outDir", str(output)],
            cwd=root / "web", check=True,
        )
        generated = files(output)
        if not (output / "index.html").is_file():
            raise RuntimeError("frontend build did not produce index.html")
        if update:
            if destination.exists():
                shutil.rmtree(destination)
            shutil.copytree(output, destination)
            print("Updated server assets; commit them with the frontend change.")
        else:
            committed = files(destination)
            changed = sorted(
                str(path) for path in generated.keys() | committed.keys()
                if generated.get(path) != committed.get(path)
            )
            if changed:
                raise RuntimeError(
                    "server assets are stale: " + ", ".join(changed)
                    + "; run python3 scripts/build_web.py --update and commit the assets"
                )
            print("Server assets match a fresh frontend build.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--update", action="store_true", help="replace the committed server assets")
    build(parser.parse_args().update)
