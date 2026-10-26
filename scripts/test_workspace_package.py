#!/usr/bin/env python3
"""Verify clean core/CLI/server archives, including a server build without Node."""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import tomllib

from check_release_source import check as check_source


NAMES = ("bbs-core", "bbs-cli", "bbs-server")


def check():
    root = Path(__file__).resolve().parent.parent
    check_source(root)
    expected_revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    env = dict(os.environ)
    for key in ("BBS_WEB_DIST", "BBS_BUILD_REVISION", "BBS_BUILD_DIRTY"):
        env.pop(key, None)
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=root, env=env,
    ))
    target = Path(metadata["target_directory"])
    host = next(
        line.removeprefix("host: ")
        for line in subprocess.check_output(["rustc", "-vV"], text=True, env=env).splitlines()
        if line.startswith("host: ")
    )
    packages = {p["name"]: p for p in metadata["packages"] if p["name"] in NAMES}
    assert set(packages) == set(NAMES), "expected exactly the core, CLI and server packages"
    versions = {p["version"] for p in packages.values()}
    assert len(versions) == 1, "workspace package versions must match"
    subprocess.run(
        ["cargo", "package", "--workspace", "--locked", "--no-verify"],
        cwd=root, env=env, check=True,
    )
    with tempfile.TemporaryDirectory(prefix="bbs-workspace-package-") as temporary:
        directory = Path(temporary)
        sources = {}
        vcs = {}
        hashes = {}
        for name, package in packages.items():
            label = f"{name}-{package['version']}"
            archive_path = target / "package" / f"{label}.crate"
            hashes[archive_path] = hashlib.sha256(archive_path.read_bytes()).hexdigest()
            with tarfile.open(archive_path) as archive:
                archive.extractall(directory, filter="data")
            source = sources[name] = directory / label
            vcs[name] = json.loads((source / ".cargo_vcs_info.json").read_text())
            assert not vcs[name]["git"].get("dirty", False), f"{name} archive is dirty"
            assert vcs[name]["git"]["sha1"] == expected_revision, f"{name} has wrong revision"
            if name == "bbs-server":
                assert (source / "web/dist/index.html").is_file(), "server has no browser bundle"
                assert not (source / "web/package.json").exists(), "server must not require npm"
            else:
                assert not (source / "web").exists(), f"{name} contains browser files"
                assert not any(source.rglob("*.js")), f"{name} contains JavaScript assets"
        revisions = {entry["git"]["sha1"] for entry in vcs.values()}
        assert len(revisions) == 1, "archives come from different revisions"
        tools = directory / "no-node"
        tools.mkdir()
        for tool in ("node", "npm", "npx"):
            stub = tools / tool
            stub.write_text("#!/bin/sh\necho 'packaged Rust build invoked Node/npm' >&2\nexit 99\n")
            stub.chmod(0o755)
        env["PATH"] = str(tools) + os.pathsep + env["PATH"]
        env["CARGO_TARGET_DIR"] = str(directory / "target")
        # The new core version need not be published to verify a PR. Resolve
        # it from its actual extracted archive, never from the source checkout.
        # Cargo updates only the temporary extracted lock files for this patch;
        # the distributable archives and their recorded VCS metadata stay intact.
        patch = "patch.crates-io.bbs-core.path=" + json.dumps(str(sources["bbs-core"]))
        for name in NAMES:
            before = tomllib.loads((sources[name] / "Cargo.lock").read_text())
            original = {
                (p["name"], p["version"], p.get("source"))
                for p in before["package"] if p["name"] not in NAMES
            }
            subprocess.run(
                ["cargo", "build", "--offline", "--config", patch],
                cwd=sources[name], env=env, check=True,
            )
            after = tomllib.loads((sources[name] / "Cargo.lock").read_text())
            resolved = {
                (p["name"], p["version"], p.get("source"))
                for p in after["package"] if p["name"] not in NAMES
            }
            assert resolved <= original, f"{name} changed unrelated locked dependencies"
            if name != "bbs-core":
                executable = "bbs" if name == "bbs-cli" else "bbs-server"
                report = json.loads(subprocess.check_output(
                    [str(Path(env["CARGO_TARGET_DIR"]) / "debug" / executable), "version", "--manifest"],
                    env=env,
                ))
                assert report["version"] == packages[name]["version"]
                assert report["revision"] == vcs[name]["git"]["sha1"]
        cli = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--offline", "--filter-platform", host, "--format-version", "1", "--config", patch],
            cwd=sources["bbs-cli"], env=env,
        ))
        assert not any(p["name"] in ("axum", "bbs-server") for p in cli["packages"]), \
            "CLI depends on the web/server implementation"
        for name in ("bbs-core", "bbs-server"):
            graph = json.loads(subprocess.check_output(
                ["cargo", "metadata", "--offline", "--filter-platform", host, "--format-version", "1", "--config", patch],
                cwd=sources[name], env=env,
            ))
            forbidden = {"ratatui", "ratatui-textarea", "bbs-cli"}
            if name == "bbs-core":
                forbidden |= {"axum", "bbs-server"}
            assert not any(p["name"] in forbidden for p in graph["packages"]), \
                f"{name} depends on another application's implementation"
        core = next(p for p in cli["packages"] if p["name"] == "bbs-core")
        assert Path(core["manifest_path"]).parent == sources["bbs-core"]
        assert core["version"] == packages["bbs-core"]["version"]
        for archive, digest in hashes.items():
            assert hashlib.sha256(archive.read_bytes()).hexdigest() == digest, "archive changed during verification"
    print("Core and CLI omit browser assets; all three archives build without Node and retain clean release identity.")


if __name__ == "__main__":
    check()
