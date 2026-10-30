"""Package a git archive and lockfile inventories using Python's stdlib."""

import hashlib
import json
import os
import pathlib
import sys
import tarfile
import tempfile
from io import BytesIO


def cargo_components(data):
    document = __import__("tomllib").loads(data.decode("utf-8"))
    for package in document.get("package", []):
        name = package.get("name")
        version = package.get("version")
        if name and version:
            yield component("cargo", name, version)


def npm_components(data):
    document = json.loads(data.decode("utf-8"))
    packages = document.get("packages")
    if not isinstance(packages, dict):
        raise ValueError("package-lock.json has no packages object")
    for path, package in packages.items():
        if not path or package.get("link") is True:
            continue
        name = package.get("name") or npm_name_from_path(path)
        version = package.get("version")
        if name and version:
            yield component("npm", name, version)


def npm_name_from_path(path):
    suffix = path.rsplit("node_modules/", 1)[-1]
    parts = suffix.split("/")
    if parts[0].startswith("@") and len(parts) > 1:
        return "/".join(parts[:2])
    return parts[0]


def component(ecosystem, name, version):
    purl_name = name.replace("@", "%40", 1) if ecosystem == "npm" and name.startswith("@") else name
    return {
        "type": "library",
        "name": name,
        "version": version,
        "purl": f"pkg:{ecosystem}/{purl_name}@{version}",
    }


def lockfile_bytes(source, member):
    try:
        handle = source.extractfile(member)
    except KeyError as error:
        raise ValueError(f"source archive lacks {member}") from error
    if handle is None:
        raise ValueError(f"source archive lacks {member}")
    with handle:
        return handle.read()


def make_archive(raw_tar, staging, output, version, commit, filename):
    root_name = f"craftcloud-{version}"
    archive_path = staging / filename
    sbom_path = staging / f"{root_name}.cdx.json"
    sums_path = staging / "SHA256SUMS.entry"
    metadata = json.dumps(
        {"version": version, "commit": commit},
        indent=2,
    ).encode("utf-8") + b"\n"
    with tarfile.open(raw_tar, "r:") as source:
        cargo_lock = lockfile_bytes(source, f"{root_name}/Cargo.lock")
        npm_lock = lockfile_bytes(source, f"{root_name}/apps/craftcloud-admin/package-lock.json")
        with tarfile.open(archive_path, "w:gz", format=tarfile.PAX_FORMAT) as target:
            for member in source:
                target.addfile(member, source.extractfile(member) if member.isfile() else None)
            info = tarfile.TarInfo(f"{root_name}/CRAFTCLOUD-RELEASE.json")
            info.size = len(metadata)
            info.mode = 0o644
            info.mtime = 0
            target.addfile(info, BytesIO(metadata))

    components = list(cargo_components(cargo_lock)) + list(npm_components(npm_lock))
    components.sort(key=lambda item: item["purl"])
    bom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "component": {
                "type": "application",
                "name": "craftcloud",
                "version": version,
            }
        },
        "components": components,
    }
    sbom_path.write_text(json.dumps(bom, indent=2) + "\n", encoding="utf-8")
    digest = hashlib.sha256(archive_path.read_bytes()).hexdigest()
    sums_path.write_text(f"{digest}  {filename}\n", encoding="ascii")
    publish_outputs(staging, output, root_name, filename)


def publish_outputs(staging, output, root_name, filename):
    output.mkdir(parents=True, exist_ok=True)
    archive = output / filename
    sbom = output / f"{root_name}.cdx.json"
    sums = output / "SHA256SUMS"
    placeholder = output / "SHA256SUMS.minisign.txt"
    if archive.exists() or sbom.exists():
        raise FileExistsError(f"release {root_name.removeprefix('craftcloud-')} already exists in dist")

    entry = (staging / "SHA256SUMS.entry").read_text(encoding="ascii")
    existing = sums.read_text(encoding="ascii") if sums.exists() else ""
    if any(line.split()[-1:] == [filename] for line in existing.splitlines()):
        raise FileExistsError(f"checksum entry for {filename} already exists")
    updated = existing
    if updated and not updated.endswith("\n"):
        updated += "\n"
    updated += entry
    fd, temporary = tempfile.mkstemp(prefix=".SHA256SUMS-", dir=output)
    temporary_path = pathlib.Path(temporary)
    published = []
    try:
        with os.fdopen(fd, "w", encoding="ascii", newline="") as manifest:
            manifest.write(updated)
            manifest.flush()
            os.fsync(manifest.fileno())
        if not placeholder.exists():
            placeholder.write_text(
                "# minisign signing is prepared but not enabled: no project signing key is configured.\n"
                "# After the project key is provisioned, sign SHA256SUMS with minisign -Sm SHA256SUMS.\n",
                encoding="utf-8",
            )
        os.replace(staging / filename, archive)
        published.append(archive)
        os.replace(staging / f"{root_name}.cdx.json", sbom)
        published.append(sbom)
        os.replace(temporary_path, sums)
    except BaseException:
        for path in published:
            path.unlink(missing_ok=True)
        temporary_path.unlink(missing_ok=True)
        raise


def main(args):
    if len(args) != 7:
        raise SystemExit("usage: release_archive.py <tar> <staging-dir> <output-dir> <version> <commit> <filename>")
    raw_tar = pathlib.Path(args[1])
    staging = pathlib.Path(args[2])
    output = pathlib.Path(args[3])
    version = args[4]
    commit = args[5]
    filename = args[6]
    make_archive(raw_tar, staging, output, version, commit, filename)


if __name__ == "__main__":
    main(sys.argv)
