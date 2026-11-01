"""Build the prebuilt image archives of a release (OPS-02) using only the standard library.

For each architecture the helper builds the CraftCloud image from the release
source (the same git tree that went into the source archive), saves it with
`docker save`, compresses it to `craftcloud-<version>-image-<arch>.tar.zst`
and adds the SHA-256 of the result to `SHA256SUMS` next to the source archive.
Nothing is pushed anywhere: no registry, no release page.
"""

import hashlib
import os
import pathlib
import shutil
import subprocess
import sys
import tarfile
import tempfile

IMAGE_REPOSITORY = "craftcloud"
KNOWN_ARCHITECTURES = ("amd64", "arm64")


def image_tag(version):
    """Docker tags cannot contain '+'; the installer applies the same rule."""
    return version.replace("+", "_", 1)


def image_reference(version):
    return f"{IMAGE_REPOSITORY}:{image_tag(version)}"


def image_archive_name(version, arch):
    return f"craftcloud-{version}-image-{arch}.tar.zst"


def parse_architectures(text, available):
    """Turns "amd64,arm64" or "" (default) into the list to build.

    The default is amd64 plus every other known architecture the local Docker
    builder can run. Explicit names must be known; an explicit name the builder
    cannot run is an error instead of a silent skip.
    """
    if not text:
        return [arch for arch in KNOWN_ARCHITECTURES if arch == "amd64" or arch in available]
    wanted = [part.strip() for part in text.split(",") if part.strip()]
    if not wanted or len(set(wanted)) != len(wanted):
        raise ValueError(f"invalid architecture list {text!r}")
    for arch in wanted:
        if arch not in KNOWN_ARCHITECTURES:
            raise ValueError(f"unknown architecture {arch!r}; use amd64 or arm64")
        if arch not in available:
            raise ValueError(f"the local Docker builder cannot build linux/{arch}")
    return wanted


def available_architectures():
    """Architectures listed by `docker buildx ls` (empty if buildx is missing)."""
    result = subprocess.run(
        ["docker", "buildx", "ls"], capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        return {"amd64"}
    found = {arch for arch in KNOWN_ARCHITECTURES if f"linux/{arch}" in result.stdout}
    return found or {"amd64"}


def compress_zstd(source, destination):
    """Zstandard compression with Python 3.14's module or the zstd program."""
    try:
        from compression import zstd  # Python 3.14 or newer
    except ImportError:
        zstd = None
    if zstd is not None:
        with open(source, "rb") as raw, zstd.open(destination, "wb", level=19) as packed:
            shutil.copyfileobj(raw, packed, 1024 * 1024)
        return
    program = shutil.which("zstd")
    if program is None:
        raise RuntimeError(
            "image archives need Python 3.14 or newer (compression.zstd) or the zstd program"
        )
    subprocess.run([program, "-19", "-T0", "-q", "-f", "-o", str(destination), str(source)], check=True)


def sha256_of(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def register_checksum(output, filename):
    """Appends `<sha256>  <filename>` to SHA256SUMS; refuses a second entry."""
    sums = output / "SHA256SUMS"
    existing = sums.read_text(encoding="ascii") if sums.exists() else ""
    if any(line.split()[-1:] == [filename] for line in existing.splitlines()):
        raise FileExistsError(f"checksum entry for {filename} already exists")
    updated = existing
    if updated and not updated.endswith("\n"):
        updated += "\n"
    updated += f"{sha256_of(output / filename)}  {filename}\n"
    fd, temporary = tempfile.mkstemp(prefix=".SHA256SUMS-", dir=output)
    try:
        with os.fdopen(fd, "w", encoding="ascii", newline="") as manifest:
            manifest.write(updated)
            manifest.flush()
            os.fsync(manifest.fileno())
        os.replace(temporary, sums)
    except BaseException:
        pathlib.Path(temporary).unlink(missing_ok=True)
        raise


def extract_context(raw_tar, destination, version):
    """Unpacks the git archive and returns the source directory."""
    with tarfile.open(raw_tar, "r:") as source:
        source.extractall(destination, filter="data")
    context = destination / f"craftcloud-{version}"
    if not (context / "deploy/compose/Dockerfile").is_file():
        raise RuntimeError("the release source has no deploy/compose/Dockerfile")
    return context


def docker(*args, **kwargs):
    subprocess.run(["docker", *args], check=True, **kwargs)


def build_image_archive(context, staging, output, version, commit, arch):
    reference = image_reference(version)
    filename = image_archive_name(version, arch)
    if (output / filename).exists():
        raise FileExistsError(f"{filename} already exists in dist")
    print(f"release: building {reference} for linux/{arch}", flush=True)
    docker(
        "buildx", "build", "--load", "--platform", f"linux/{arch}",
        "--build-arg", f"CRAFTCLOUD_VERSION={version}",
        "--build-arg", f"CRAFTCLOUD_COMMIT={commit}",
        "-f", "deploy/compose/Dockerfile", "-t", reference, ".",
        cwd=context,
    )
    raw = staging / f"image-{arch}.tar"
    try:
        docker("save", "-o", str(raw), reference)
        packed = staging / filename
        compress_zstd(raw, packed)
    finally:
        raw.unlink(missing_ok=True)
        # The image was only a build product; the archive is the artifact.
        subprocess.run(["docker", "rmi", reference], check=False, capture_output=True)
    os.replace(packed, output / filename)
    try:
        register_checksum(output, filename)
    except BaseException:
        (output / filename).unlink(missing_ok=True)
        raise
    print(f"release: created {output / filename}", flush=True)


def main(args):
    if len(args) != 7:
        raise SystemExit(
            "usage: release_image.py <source.tar> <staging-dir> <output-dir> <version> <commit> <architectures|''>"
        )
    raw_tar, staging, output = (pathlib.Path(args[i]) for i in (1, 2, 3))
    version, commit, requested = args[4], args[5], args[6]
    architectures = parse_architectures(requested, available_architectures())
    context = extract_context(raw_tar, staging / "image-context", version)
    for arch in architectures:
        build_image_archive(context, staging, output, version, commit, arch)
    skipped = [arch for arch in KNOWN_ARCHITECTURES if arch not in architectures]
    if skipped:
        print(f"release: no image archive for {', '.join(skipped)} (not requested or not buildable here)")


if __name__ == "__main__":
    main(sys.argv)
