#!/usr/bin/env python3
"""Update the pinned runner and GitHub-provided archive digests; --check is offline."""
import argparse
import json
import os
from pathlib import Path
import re
import urllib.request

ROOT = Path(__file__).resolve().parent.parent


def check(manifest, dockerfile):
    version = manifest["version"]
    assert re.fullmatch(r"\d+\.\d+\.\d+", version), "invalid runner version"
    assert f"ARG RUNNER_VERSION={version}\n" in dockerfile, "Dockerfile version differs"
    assert f'test "${{RUNNER_VERSION}}" = "{version}";' in dockerfile, "Dockerfile checksum guard differs"
    for arch, checksum in manifest["sha256"].items():
        assert re.fullmatch(r"[0-9a-f]{64}", checksum), f"invalid {arch} digest"
        assert f"{arch}) RUNNER_SHA256={checksum} ;;" in dockerfile, f"Dockerfile {arch} digest differs"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="validate committed versions/digests without network")
    args = parser.parse_args()
    manifest_path = ROOT / "runner/version.json"
    docker_path = ROOT / "runner/Dockerfile"
    manifest = json.loads(manifest_path.read_text())
    dockerfile = docker_path.read_text()
    if args.check:
        check(manifest, dockerfile)
        print(f"Runner {manifest['version']}: version and architecture digests match")
        return
    headers = {"Accept": "application/vnd.github+json", "User-Agent": "runnerctl-update"}
    if os.environ.get("GITHUB_TOKEN"):
        headers["Authorization"] = "Bearer " + os.environ["GITHUB_TOKEN"]
    request = urllib.request.Request("https://api.github.com/repos/actions/runner/releases/latest", headers=headers)
    with urllib.request.urlopen(request, timeout=30) as response:
        release = json.load(response)
    version = release["tag_name"].removeprefix("v")
    if not re.fullmatch(r"\d+\.\d+\.\d+", version) or release.get("prerelease") or release.get("draft"):
        raise RuntimeError("expected a stable runner release")
    assets = {asset["name"]: asset.get("digest", "") for asset in release["assets"]}
    digests = {}
    for arch in ("x64", "arm64", "arm"):
        digest = assets[f"actions-runner-linux-{arch}-{version}.tar.gz"]
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
            raise RuntimeError(f"GitHub archive digest unavailable for {arch}; verify it before updating")
        digests[arch] = digest.removeprefix("sha256:")
    dockerfile = re.sub(r"(?m)^ARG RUNNER_VERSION=.*$", f"ARG RUNNER_VERSION={version}", dockerfile)
    dockerfile = re.sub(r'test "\$\{RUNNER_VERSION\}" = "[^"]+";', f'test "${{RUNNER_VERSION}}" = "{version}";', dockerfile)
    for arch, checksum in digests.items():
        dockerfile = re.sub(rf"{arch}\) RUNNER_SHA256=[0-9a-f]+ ;;", f"{arch}) RUNNER_SHA256={checksum} ;;", dockerfile)
    manifest = {"version": version, "published_at": release["published_at"], "sha256": digests}
    check(manifest, dockerfile)
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    docker_path.write_text(dockerfile)
    print(f"Pinned runner {version} with verified GitHub asset digests")


if __name__ == "__main__":
    main()
