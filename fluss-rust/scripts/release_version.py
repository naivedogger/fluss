#!/usr/bin/env python3
# Licensed to the Apache Software Foundation (ASF) under one
# or more contributor license agreements.  See the NOTICE file
# distributed with this work for additional information
# regarding copyright ownership.  The ASF licenses this file
# to you under the Apache License, Version 2.0 (the
# "License"); you may not use this file except in compliance
# with the License.  You may obtain a copy of the License at
#
#   http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing,
# software distributed under the License is distributed on an
# "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
# KIND, either express or implied.  See the License for the
# specific language governing permissions and limitations
# under the License.

"""Prepare client versions and check them before the shared Fluss release tag."""

import sys

if sys.version_info < (3, 11):
    sys.exit("Release tooling requires Python 3.11 or newer (uses tomllib).")

import argparse
import re
import tomllib
import xml.etree.ElementTree as ET
from pathlib import Path

ROOT_DIR = Path(__file__).resolve().parent.parent
CORE_VERSION = r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
VERSION = re.compile(
    rf"{CORE_VERSION}(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)
TAG = re.compile(rf"v({CORE_VERSION})(?:-rc\.?[1-9][0-9]*)?")


def validate_version(version):
    """Require Cargo-compatible SemVer, not a Maven shorthand or a Git tag."""
    match = VERSION.fullmatch(version)
    if not match or (
        match.group(1)
        and any(
            part.isdigit() and len(part) > 1 and part.startswith("0")
            for part in match.group(1).split(".")
        )
    ):
        raise ValueError(
            f"Invalid version {version!r}; use SemVer, e.g. 1.1.0-SNAPSHOT."
        )
    return version


def read_toml(path):
    with path.open("rb") as stream:
        return tomllib.load(stream)


def mix_version(text):
    matches = re.findall(r'(?m)^[ \t]*@version[ \t]+"([^"]+)"', text)
    if len(matches) != 1:
        raise ValueError("Expected exactly one @version in bindings/elixir/mix.exs.")
    return matches[0]


def client_versions(root):
    workspace = read_toml(root / "Cargo.toml")["workspace"]
    return {
        "Cargo.toml [workspace.package].version": workspace["package"]["version"],
        "Cargo.toml [workspace.dependencies].fluss.version": workspace["dependencies"][
            "fluss"
        ]["version"],
        "bindings/elixir/mix.exs @version": mix_version(
            (root / "bindings/elixir/mix.exs").read_text()
        ),
    }


def replace_in_section(text, section, pattern, replacement):
    """Edit one known field without reformatting TOML or touching other versions."""
    header = re.search(rf"(?m)^\[{re.escape(section)}\][ \t]*(?:#.*)?$", text)
    if not header:
        raise ValueError(f"Missing [{section}] in Cargo.toml.")
    start = header.end()
    next_header = re.search(r"(?m)^\[", text[start:])
    end = start + next_header.start() if next_header else len(text)
    body, count = re.subn(pattern, replacement, text[start:end], flags=re.MULTILINE)
    if count != 1:
        raise ValueError(f"Expected exactly one version field in [{section}].")
    return text[:start] + body + text[end:]


def bump(root, current, next_version):
    """Update client manifests only; Cargo and audit tools own generated files."""
    validate_version(current)
    validate_version(next_version)
    for location, actual in client_versions(root).items():
        if actual != current:
            raise ValueError(
                f"{location}: expected {current}, found {actual}; no changes."
            )

    cargo_path = root / "Cargo.toml"
    mix_path = root / "bindings/elixir/mix.exs"
    originals = {path: path.read_text() for path in (cargo_path, mix_path)}
    cargo = replace_in_section(
        originals[cargo_path],
        "workspace.package",
        rf"""^([ \t]*version[ \t]*=[ \t]*)(["']){re.escape(current)}\2""",
        lambda match: f"{match[1]}{match[2]}{next_version}{match[2]}",
    )
    cargo = replace_in_section(
        cargo,
        "workspace.dependencies",
        r"""^(fluss[ \t]*=[ \t]*\{[^\n]*?\bversion[ \t]*=[ \t]*)(["'])"""
        rf"{re.escape(current)}\2",
        lambda match: f"{match[1]}{match[2]}{next_version}{match[2]}",
    )
    # Validate the edited TOML before writing either manifest.
    tomllib.loads(cargo)
    mix = re.sub(
        rf'(?m)^([ \t]*@version[ \t]+"){re.escape(current)}"',
        lambda match: f'{match[1]}{next_version}"',
        originals[mix_path],
    )
    written = []
    try:
        for path, contents in ((cargo_path, cargo), (mix_path, mix)):
            path.write_text(contents)
            written.append(path)
    except OSError:
        for path in written:
            path.write_text(originals[path])
        raise
    print(f"Client manifest versions: {current} -> {next_version}.")
    print(
        "Cargo.lock and dependency inventories were NOT edited. Regenerate them with "
        "Cargo and scripts/dependencies.py generate, including the Gateway workspace "
        "which depends on this client. See website/docs/release/create-release.md."
    )


def check(root, tag):
    """Read-only version preflight; the tag name may be checked before it exists."""
    match = TAG.fullmatch(tag)
    if not match:
        raise ValueError(
            "Expected vX.Y.Z, vX.Y.Z-rcN or vX.Y.Z-rc.N (N >= 1); "
            "snapshot, incubating and arbitrary v* tags are not release tags."
        )
    expected = match[1]
    errors = []

    def expect(location, actual):
        if actual != expected:
            errors.append(f"{location}: expected {expected}, found {actual!r}")

    for location, actual in client_versions(root).items():
        expect(f"fluss-rust/{location}", actual)

    workspace = read_toml(root / "Cargo.toml")["workspace"]
    names = []
    for member in workspace["members"]:
        # The unpublished protobuf generator has its own development version.
        if member == "crates/fluss/gen":
            continue
        package = read_toml(root / member / "Cargo.toml")["package"]
        if package.get("version") != {"workspace": True}:
            errors.append(
                f"fluss-rust/{member}/Cargo.toml: version must inherit the workspace"
            )
        names.append(package["name"])

    def check_lock(path, package_names):
        packages = read_toml(path)["package"]
        for name in package_names:
            local = [
                item
                for item in packages
                if item["name"] == name and "source" not in item
            ]
            location = f"{path.relative_to(root.parent)} ({name})"
            if len(local) != 1:
                errors.append(f"{location}: expected exactly one local package entry")
            else:
                expect(location, local[0]["version"])

    check_lock(root / "Cargo.lock", names)
    project = ET.parse(root.parent / "pom.xml").getroot()
    expect(
        "pom.xml project version",
        project.findtext("{http://maven.apache.org/POM/4.0.0}version"),
    )
    gateway = root.parent / "fluss-gateway"
    expect(
        "fluss-gateway/Cargo.toml package version",
        read_toml(gateway / "Cargo.toml")["package"]["version"],
    )
    check_lock(gateway / "Cargo.lock", ["fluss-gateway", "fluss-rs"])
    python_project = read_toml(root / "bindings/python/pyproject.toml")["project"]
    if (
        "version" not in python_project.get("dynamic", [])
        or "version" in python_project
    ):
        errors.append("bindings/python/pyproject.toml: version must be dynamic")
    if errors:
        raise ValueError("Release version preflight failed:\n- " + "\n- ".join(errors))
    print(
        f"Versions and local lockfile entries match {tag} (source version {expected})."
    )
    print(
        "This checks the working tree and proposed tag name only. It does not verify "
        "an existing Git tag, dependency resolution, builds, licenses "
        "or release approval."
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    bump_parser = commands.add_parser("bump", help="Update client manifest versions")
    bump_parser.add_argument("current")
    bump_parser.add_argument("next_version")
    check_parser = commands.add_parser(
        "check", help="Read-only shared-release preflight"
    )
    check_parser.add_argument(
        "--tag", required=True, help="Proposed shared release tag"
    )
    args = parser.parse_args()
    try:
        if args.command == "bump":
            bump(ROOT_DIR, args.current, args.next_version)
        else:
            check(ROOT_DIR, args.tag)
    except (ValueError, KeyError, OSError, ET.ParseError) as error:
        parser.exit(1, f"Error: {error}\n")


if __name__ == "__main__":
    main()
