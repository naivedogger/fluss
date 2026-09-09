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

"""Offline release-tool tests; all modified manifests live in temporary fixtures."""

import contextlib
import io
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import release_version


class ReleaseVersionTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="fluss-release-test-")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.root = self.repo / "fluss-rust"
        self.write(
            "fluss-rust/Cargo.toml",
            """[workspace.package]
version = "1.0.0"
edition = "2024"

[workspace]
members = ["crates/fluss", "bindings/cpp", "bindings/python",
           "bindings/elixir/native/fluss_nif", "crates/fluss/gen"]

[workspace.dependencies]
fluss = { package = "fluss-rs", version = "1.0.0", path = "crates/fluss" }
unrelated = { version = "1.0.0" }
similar = { version = "1x0x0" }
""",
        )
        members = {
            "crates/fluss": "fluss-rs",
            "bindings/cpp": "fluss-cpp",
            "bindings/python": "fluss_python",
            "bindings/elixir/native/fluss_nif": "fluss_nif",
        }
        for member, name in members.items():
            self.write(
                f"fluss-rust/{member}/Cargo.toml",
                f'[package]\nname = "{name}"\nversion.workspace = true\n',
            )
        self.write(
            "fluss-rust/crates/fluss/gen/Cargo.toml",
            '[package]\nname = "gen"\nversion = "0.1.0"\npublish = false\n',
        )
        self.write(
            "fluss-rust/bindings/elixir/mix.exs",
            'defmodule Fluss.MixProject do\n  @version "1.0.0"\nend\n',
        )
        self.write(
            "fluss-rust/bindings/python/pyproject.toml",
            '[project]\nname = "pyfluss"\ndynamic = ["version"]\n',
        )
        self.write(
            "fluss-rust/Cargo.lock",
            "version = 4\n"
            + "".join(
                f'[[package]]\nname = "{name}"\nversion = "1.0.0"\n'
                for name in members.values()
            )
            + '[[package]]\nname = "gen"\nversion = "0.1.0"\n',
        )
        self.write(
            "fluss-gateway/Cargo.toml",
            '[package]\nname = "fluss-gateway"\nversion = "1.0.0"\n',
        )
        self.write(
            "fluss-gateway/Cargo.lock",
            'version = 4\n[[package]]\nname = "fluss-gateway"\nversion = "1.0.0"\n'
            '[[package]]\nname = "fluss-rs"\nversion = "1.0.0"\n',
        )
        self.write(
            "pom.xml",
            '<project xmlns="http://maven.apache.org/POM/4.0.0">'
            "<parent><version>30</version></parent><version>1.0.0</version></project>",
        )
        scripts = self.root / "scripts"
        scripts.mkdir()
        for name in ("release_version.py", "bump-version.sh", "release.sh"):
            shutil.copyfile(Path(__file__).parent / name, scripts / name)

    def write(self, name, contents):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)
        return path

    def replace(self, name, old, new):
        path = self.repo / name
        path.write_text(path.read_text().replace(old, new))

    def snapshot(self):
        return {
            path.relative_to(self.repo): path.read_bytes()
            for path in self.repo.rglob("*")
            if path.is_file()
        }

    def check(self, tag="v1.0.0-rc1"):
        with contextlib.redirect_stdout(io.StringIO()):
            release_version.check(self.root, tag)

    def bump(self, current="1.0.0", next_version="1.1.0"):
        with contextlib.redirect_stdout(io.StringIO()):
            release_version.bump(self.root, current, next_version)

    def test_supported_tags_are_read_only(self):
        before = self.snapshot()
        for tag in ("v1.0.0", "v1.0.0-rc1", "v1.0.0-rc.2"):
            with self.subTest(tag=tag):
                self.check(tag)
        self.assertEqual(self.snapshot(), before)

    def test_invalid_tags_fail_without_changes(self):
        before = self.snapshot()
        for tag in (
            "1.0.0",
            "refs/tags/v1.0.0",
            "v1.0.0-incubating",
            "v1.0.0-SNAPSHOT",
            "v1.0.0-beta1",
            "v1.0.0+build",
            "v1.0.0-rc0",
            "v1.0.0-rc01",
            "v01.0.0",
            "v1.0.0-rc1\n",
        ):
            with (
                self.subTest(tag=tag),
                self.assertRaisesRegex(ValueError, "Expected v"),
            ):
                self.check(tag)
        self.assertEqual(self.snapshot(), before)

    def test_wrong_release_version_fails(self):
        with self.assertRaisesRegex(ValueError, "expected 1.0.1"):
            self.check("v1.0.1")

    def test_reports_all_mismatched_versions(self):
        self.replace("pom.xml", "<version>1.0.0", "<version>1.0-SNAPSHOT")
        self.replace("fluss-gateway/Cargo.toml", '"1.0.0"', '"1.0.0-SNAPSHOT"')
        self.replace("fluss-rust/bindings/elixir/mix.exs", '"1.0.0"', '"0.1.0"')
        with self.assertRaises(ValueError) as error:
            self.check()
        for location in ("pom.xml", "fluss-gateway/Cargo.toml", "mix.exs"):
            self.assertIn(location, str(error.exception))

    def test_stale_rust_lock_fails(self):
        self.replace(
            "fluss-rust/Cargo.lock",
            'name = "fluss-cpp"\nversion = "1.0.0"',
            'name = "fluss-cpp"\nversion = "0.1.0"',
        )
        with self.assertRaisesRegex(ValueError, r"Cargo.lock \(fluss-cpp\)"):
            self.check()

    def test_stale_gateway_client_lock_fails(self):
        self.replace(
            "fluss-gateway/Cargo.lock",
            'name = "fluss-rs"\nversion = "1.0.0"',
            'name = "fluss-rs"\nversion = "0.1.0"',
        )
        with self.assertRaisesRegex(
            ValueError, r"fluss-gateway/Cargo.lock \(fluss-rs\)"
        ):
            self.check()

    def test_missing_or_registry_lock_entry_fails(self):
        self.replace(
            "fluss-rust/Cargo.lock",
            'name = "fluss-cpp"',
            'source = "registry+https://example.invalid"\nname = "fluss-cpp"',
        )
        with self.assertRaisesRegex(ValueError, "exactly one local package"):
            self.check()

    def test_duplicate_local_lock_entry_fails(self):
        path = self.root / "Cargo.lock"
        path.write_text(
            path.read_text() + '[[package]]\nname = "fluss-cpp"\nversion = "1.0.0"\n'
        )
        with self.assertRaisesRegex(ValueError, "exactly one local package"):
            self.check()

    def test_binding_must_inherit_workspace_version(self):
        self.replace(
            "fluss-rust/bindings/cpp/Cargo.toml",
            "version.workspace = true",
            'version = "1.0.0"',
        )
        with self.assertRaisesRegex(ValueError, "must inherit"):
            self.check()

    def test_python_version_must_be_dynamic(self):
        self.replace(
            "fluss-rust/bindings/python/pyproject.toml",
            'dynamic = ["version"]',
            'version = "1.0.0"',
        )
        with self.assertRaisesRegex(ValueError, "must be dynamic"):
            self.check()

    def test_bump_changes_only_owned_manifest_fields(self):
        before = self.snapshot()
        self.bump()
        after = self.snapshot()
        changed = {str(path) for path in before if before[path] != after[path]}
        self.assertEqual(
            changed, {"fluss-rust/Cargo.toml", "fluss-rust/bindings/elixir/mix.exs"}
        )
        self.assertEqual(
            set(release_version.client_versions(self.root).values()), {"1.1.0"}
        )
        cargo = (self.root / "Cargo.toml").read_text()
        self.assertIn('unrelated = { version = "1.0.0" }', cargo)
        self.assertIn('similar = { version = "1x0x0" }', cargo)

    def test_bump_does_not_hide_stale_generated_files(self):
        self.bump()
        with self.assertRaisesRegex(ValueError, "Cargo.lock"):
            self.check("v1.1.0")

    def test_snapshot_version_is_allowed_for_next_development_cycle(self):
        self.bump(next_version="1.1.0-SNAPSHOT")
        self.assertEqual(
            set(release_version.client_versions(self.root).values()), {"1.1.0-SNAPSHOT"}
        )

    def test_bump_rejects_invalid_versions_before_writing(self):
        before = self.snapshot()
        for version in ("1.1-SNAPSHOT", "v1.1.0", "1.01.0", "1.1.0-01", "1.1.0/x"):
            with self.subTest(version=version), self.assertRaisesRegex(
                ValueError, "Invalid version"
            ):
                self.bump(next_version=version)
        self.assertEqual(self.snapshot(), before)

    def test_bump_wrong_current_version_does_not_change_files(self):
        before = self.snapshot()
        with self.assertRaisesRegex(ValueError, "no changes"):
            self.bump(current="0.1.0")
        self.assertEqual(self.snapshot(), before)

    def test_bump_validates_all_manifest_versions_before_writing(self):
        self.replace("fluss-rust/bindings/elixir/mix.exs", '"1.0.0"', '"0.1.0"')
        before = self.snapshot()
        with self.assertRaisesRegex(ValueError, "mix.exs"):
            self.bump()
        self.assertEqual(self.snapshot(), before)

    def test_bump_rejects_stale_self_dependency_before_writing(self):
        self.replace(
            "fluss-rust/Cargo.toml",
            'package = "fluss-rs", version = "1.0.0"',
            'package = "fluss-rs", version = "0.1.0"',
        )
        before = self.snapshot()
        with self.assertRaisesRegex(ValueError, "workspace.dependencies"):
            self.bump()
        self.assertEqual(self.snapshot(), before)

    def test_bump_restores_first_manifest_if_second_write_fails(self):
        before = self.snapshot()
        original_write = Path.write_text

        def fail_mix_write(path, *args, **kwargs):
            if path.name == "mix.exs":
                raise OSError("simulated write failure")
            return original_write(path, *args, **kwargs)

        with mock.patch.object(Path, "write_text", fail_mix_write):
            with self.assertRaisesRegex(OSError, "simulated"):
                self.bump()
        self.assertEqual(self.snapshot(), before)

    def test_bump_supports_single_quoted_toml_and_comments(self):
        self.replace("fluss-rust/Cargo.toml", '"1.0.0"', "'1.0.0'")
        self.replace(
            "fluss-rust/Cargo.toml",
            "[workspace.package]",
            "[workspace.package] # owned",
        )
        self.bump()
        self.assertEqual(
            set(release_version.client_versions(self.root).values()), {"1.1.0"}
        )

    def test_unsupported_dependency_layout_fails_without_writing(self):
        self.replace(
            "fluss-rust/Cargo.toml",
            'fluss = { package = "fluss-rs", version = "1.0.0", '
            'path = "crates/fluss" }',
            "",
        )
        cargo = self.root / "Cargo.toml"
        cargo.write_text(
            cargo.read_text()
            + '\n[workspace.dependencies.fluss]\npackage = "fluss-rs"\n'
            'version = "1.0.0"\npath = "crates/fluss"\n'
        )
        before = self.snapshot()
        with self.assertRaisesRegex(ValueError, "exactly one version"):
            self.bump()
        self.assertEqual(self.snapshot(), before)

    @unittest.skipUnless(
        shutil.which("cargo"), "Cargo is needed for lockfile smoke test"
    )
    def test_documented_cargo_refresh_updates_both_lockfiles(self):
        # Real Cargo, but only empty local crates: no registry, network or compiler.
        self.replace(
            "fluss-rust/Cargo.toml", 'unrelated = { version = "1.0.0" }\n', ""
        )
        self.replace(
            "fluss-rust/Cargo.toml", 'similar = { version = "1x0x0" }\n', ""
        )
        workspace = release_version.read_toml(self.root / "Cargo.toml")["workspace"]
        for member in workspace["members"]:
            self.write(
                f"fluss-rust/{member}/src/lib.rs", "// Local release test fixture.\n"
            )
        self.write("fluss-gateway/src/main.rs", "fn main() {}\n")
        gateway_manifest = self.repo / "fluss-gateway/Cargo.toml"
        gateway_manifest.write_text(
            gateway_manifest.read_text()
            + '\n[dependencies]\nfluss = { package = "fluss-rs", '
            'path = "../fluss-rust/crates/fluss" }\n'
        )

        def refresh():
            for manifest in (self.root / "Cargo.toml", gateway_manifest):
                result = subprocess.run(
                    [
                        "cargo",
                        "update",
                        "--workspace",
                        "--offline",
                        "--manifest-path",
                        str(manifest),
                    ],
                    cwd=self.repo,
                    env={**os.environ, "CARGO_HOME": str(self.repo / ".cargo")},
                    capture_output=True,
                    text=True,
                    timeout=30,
                )
                self.assertEqual(result.returncode, 0, result.stderr)

        refresh()
        self.check()
        self.bump(next_version="1.0.1")
        self.replace("pom.xml", "<version>1.0.0", "<version>1.0.1")
        self.replace("fluss-gateway/Cargo.toml", '"1.0.0"', '"1.0.1"')
        with self.assertRaisesRegex(ValueError, "Cargo.lock"):
            self.check("v1.0.1-rc1")
        refresh()
        self.check("v1.0.1-rc1")

    def test_wrapper_works_outside_workspace(self):
        result = subprocess.run(
            ["bash", str(self.root / "scripts/bump-version.sh"), "1.0.0", "1.0.1"],
            cwd=self.repo,
            env={**os.environ, "PYTHON": sys.executable},
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            set(release_version.client_versions(self.root).values()), {"1.0.1"}
        )

    def test_wrapper_rejects_missing_or_extra_arguments(self):
        before = self.snapshot()
        for arguments in ([], ["1.0.0"], ["1.0.0", "1.0.1", "extra"]):
            with self.subTest(arguments=arguments):
                result = subprocess.run(
                    ["bash", str(self.root / "scripts/bump-version.sh"), *arguments],
                    env={**os.environ, "PYTHON": sys.executable},
                    capture_output=True,
                    text=True,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("usage:", result.stderr)
        self.assertEqual(self.snapshot(), before)

    def test_retired_release_entry_point_has_no_side_effects(self):
        before = self.snapshot()
        for arguments in ([], ["1.0.0"]):
            result = subprocess.run(
                ["bash", str(self.root / "scripts/release.sh"), *arguments],
                cwd=self.repo,
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("No archive was created or signed", result.stderr)
            self.assertIn("create_source_release.sh", result.stderr)
        self.assertEqual(self.snapshot(), before)

    def test_preflight_cli_from_outside_workspace(self):
        before = self.snapshot()
        result = subprocess.run(
            [
                sys.executable,
                str(self.root / "scripts/release_version.py"),
                "check",
                "--tag",
                "v1.0.0",
            ],
            cwd=self.repo,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("does not verify", result.stdout)
        self.assertEqual(self.snapshot(), before)


if __name__ == "__main__":
    unittest.main()
