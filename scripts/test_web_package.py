#!/usr/bin/env python3
"""Build the actual distributable web-feature crate without Node or npm."""

import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile

from check_release_source import check as check_source


def check():
    root = Path(__file__).resolve().parent.parent
    check_source(root)
    env = dict(os.environ)
    env.pop("BBS_WEB_DIST", None)
    env.pop("BBS_BUILD_REVISION", None)
    env.pop("BBS_BUILD_DIRTY", None)
    subprocess.run(
        # Cargo includes the ignored, generated bundle and therefore records
        # a dirty archive. Check source cleanliness separately above and keep
        # that honest archive provenance rather than overriding it.
        ["cargo", "package", "--locked", "--no-verify", "--allow-dirty"],
        cwd=root, env=env, check=True,
    )
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=root, env=env,
    ))
    target = Path(metadata["target_directory"])
    package = next(p for p in metadata["packages"] if p["name"] == "bbs-cli")
    name = f"bbs-cli-{package['version']}"
    archive = target / "package" / f"{name}.crate"
    with tempfile.TemporaryDirectory(prefix="bbs-web-package-") as temporary:
        directory = Path(temporary)
        with tarfile.open(archive) as crate:
            crate.extractall(directory, filter="data")
        source = directory / name
        assert (source / "web/dist/index.html").is_file(), "package has no web bundle"
        assert not (source / "web/package.json").exists(), "package must not need npm"
        assert not (source / "web/src").exists(), "package must not need frontend sources"
        tools = directory / "no-node"
        tools.mkdir()
        for tool in ("node", "npm", "npx"):
            stub = tools / tool
            stub.write_text("#!/bin/sh\necho 'packaged web build invoked Node/npm' >&2\nexit 99\n")
            stub.chmod(0o755)
        env["PATH"] = str(tools) + os.pathsep + env["PATH"]
        env["CARGO_TARGET_DIR"] = str(target)
        subprocess.run(
            ["cargo", "build", "--locked", "--features", "web"],
            cwd=source, env=env, check=True,
        )
        manifest = json.loads(subprocess.check_output(
            [str(target / "debug/bbs"), "version", "--json"], env=env,
        ))
        assert manifest["version"] == package["version"]
        vcs = json.loads((source / ".cargo_vcs_info.json").read_text())
        assert manifest["revision"] == vcs["git"]["sha1"]
        assert manifest["dirty"] == vcs["git"].get("dirty", False)
    print("Packaged web crate builds without Node/npm and retains its revision/dirty flag.")


if __name__ == "__main__":
    check()
