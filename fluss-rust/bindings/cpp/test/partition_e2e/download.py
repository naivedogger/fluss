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

"""Download and verify a CI-published partition runtime, then run it on ECS."""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile

OSS_ROOT = "oss://fluss-e2e-test-no-delete/benchmark-jars/cpp-sdk"
MAX_ARCHIVE = 512 * 1024**2
MAX_EXPANDED = 1536 * 1024**2
DISK_RESERVE = 2 * 1024**3


def sha256(path):
    hasher = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def validate_ref(ref):
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._/-]{0,127}", ref) or ".." in ref:
        raise ValueError("invalid SDK artifact ref")
    return ref


def host():
    if platform.system() != "Linux" or platform.machine() not in ("x86_64", "aarch64"):
        raise ValueError("runtime artifacts require native Linux x86_64 or aarch64")
    return "linux-" + platform.machine()


def glibc():
    name, version = platform.libc_ver()
    if name != "glibc" or not re.fullmatch(r"\d+\.\d+", version):
        raise ValueError("runtime artifacts require glibc Linux")
    return version


def oss_command(config):
    return ["ossutil"] + (["-c", str(config)] if config else [])


def read_json(path):
    if path.stat().st_size > 1024 * 1024:
        raise ValueError("artifact metadata too large")
    return json.loads(path.read_text())


def extract(archive, destination, expanded=None):
    """Extract only bounded regular files; never follow archive links or accept traversal."""
    with tarfile.open(archive, "r:gz") as bundle:
        entries = bundle.getmembers()
        if len(entries) > 4096 or sum(e.size for e in entries) > MAX_EXPANDED:
            raise ValueError("runtime artifact exceeds extraction limits")
        if expanded is not None and sum(e.size for e in entries) != expanded:
            raise ValueError("runtime expanded size differs from published index")
        names = set()
        for entry in entries:
            path = PurePosixPath(entry.name)
            if (not entry.isfile() or path.is_absolute() or ".." in path.parts
                    or str(path) != entry.name or entry.name in names
                    or "\\" in entry.name or entry.size < 0):
                raise ValueError("invalid runtime artifact entry: " + entry.name)
            names.add(entry.name)
        destination.mkdir()
        for entry in entries:
            target = destination / entry.name
            target.parent.mkdir(parents=True, exist_ok=True)
            with bundle.extractfile(entry) as source, target.open("xb") as output:
                shutil.copyfileobj(source, output)
            target.chmod(0o755 if entry.name.startswith(("bin/", "lib/")) else 0o644)


def runtime_metadata(runtime, ref):
    metadata = read_json(runtime / "build-metadata.json")
    revision = metadata.get("sdkRevision", "")
    if (metadata.get("artifactSchemaVersion") != 1 or not re.fullmatch("[a-f0-9]{40}", revision)
            or metadata.get("platform") != host()):
        raise ValueError("invalid SDK artifact revision/schema/platform")
    if ref not in (metadata.get("sdkRequestedRef"), revision):
        raise ValueError("SDK artifact does not match requested ref")
    minimum = metadata.get("glibcVersion", "")
    if not re.fullmatch(r"\d+\.\d+", minimum):
        raise ValueError("invalid artifact glibc version")
    if tuple(map(int, glibc().split("."))) < tuple(map(int, minimum.split("."))):
        raise ValueError("artifact requires glibc >= " + minimum + "; rebuild on the ECS baseline")
    partition_metadata(runtime, metadata)
    return metadata


def partition_metadata(runtime, metadata):
    partition = metadata.get("partitionE2e")
    if not isinstance(partition, dict):
        raise ValueError("runtime has no partition E2E; publish with NATIVE_PARTITION_E2E=ON")
    for name, key in (("bin/partition_e2e", "binarySha256"),
                      ("partition/run.py", "runnerSha256")):
        path = runtime / name
        if not path.is_file() or sha256(path) != partition.get(key):
            raise ValueError("partition runtime checksum mismatch: " + name)
    return partition


def prepare(ref, output, config, run):
    """Download a fresh ref index for every run; no same-filename or stale-cache fallback."""
    ref = validate_ref(ref)
    index_file = output / "artifact-index.json"
    run(oss_command(config) + ["cp", "-f", f"{OSS_ROOT}/{host()}/refs/{ref}.json", str(index_file)],
        output / "artifact-index.log", 300)
    index = read_json(index_file)
    digest = index.get("sha256", "")
    size = index.get("size")
    expanded = index.get("expandedBytes")
    if (index.get("schemaVersion") != 1 or index.get("sdkRef") != ref
            or not re.fullmatch("[a-f0-9]{64}", digest)
            or type(size) is not int or not 0 < size <= MAX_ARCHIVE
            or type(expanded) is not int or not 0 < expanded <= MAX_EXPANDED):
        raise ValueError("invalid OSS runtime index")
    if shutil.disk_usage(output).free < size + expanded + DISK_RESERVE:
        raise ValueError("insufficient space for runtime artifact plus 2 GiB reserve")
    archive = output / "runtime.tar.gz"
    run(oss_command(config) + ["cp", "-f",
        f"{OSS_ROOT}/{host()}/objects/{digest}.tar.gz", str(archive)],
        output / "artifact-download.log", 900)
    if archive.stat().st_size != size or sha256(archive) != digest:
        raise ValueError("OSS runtime artifact checksum/size mismatch")
    runtime = output / "runtime"
    extract(archive, runtime, expanded)
    archive.unlink()
    metadata = runtime_metadata(runtime, ref)
    metadata.update(artifactSha256=digest, artifactPublishedAt=index.get("publishedAt"))
    (output / "build-metadata.json").write_text(json.dumps(metadata, indent=2))
    return metadata


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sdk-ref", required=True)
    parser.add_argument("--oss-config", type=Path)
    parser.add_argument("--output", type=Path,
                        default=Path.cwd() / "target" / "partition-e2e")
    parser.add_argument("arguments", nargs=argparse.REMAINDER,
                        help="After --, pass partition runner options such as --config and --cases")
    args = parser.parse_args()
    options = args.arguments[1:] if args.arguments[:1] == ["--"] else args.arguments
    if not options:
        parser.error("pass runner options after --; --config and --server-version are required")
    if any(option.split("=", 1)[0] in ("--runtime", "--work-dir") for option in options):
        parser.error("runtime and work directory are selected by this downloader")
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    output = Path(tempfile.mkdtemp(prefix="run-", dir=args.output))
    tmp = output / "tmp"
    tmp.mkdir()
    os.environ["TMPDIR"] = str(tmp)

    def download(command, log, timeout):
        with log.open("w") as stream:
            subprocess.run(command, stdout=stream, stderr=subprocess.STDOUT,
                           timeout=timeout, check=True)

    print("OUTPUT " + str(output), flush=True)
    metadata = prepare(args.sdk_ref, output, args.oss_config, download)
    runtime = output / "runtime"
    print("SDK " + metadata["sdkRevision"], flush=True)
    # Replace the downloader so Ctrl-C reaches the runner that owns native cleanup.
    os.execv(sys.executable, [sys.executable, str(runtime / "partition/run.py"), "run",
                             *options, "--runtime", str(runtime), "--work-dir", str(output)])


if __name__ == "__main__":
    main()
