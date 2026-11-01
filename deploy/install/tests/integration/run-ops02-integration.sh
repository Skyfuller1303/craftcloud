#!/bin/bash
# Host-side driver for the OPS-02 installation tests (Git Bash on Windows with
# Docker Desktop, or any bash with Docker). It never touches containers,
# volumes, networks or images it did not create: everything is named ops02-*.
#
#   run-ops02-integration.sh release <repo> <workdir> <v1> <v2>
#       Copies the working tree (tracked and unignored files) into a scratch Git
#       repository below <workdir>, commits it there only, and runs
#       `cargo xtask release` for both versions (amd64 image archives). The
#       repository itself is not committed to. Use it through the memory-limited
#       cargo launcher (CARGO_LAUNCHER, a command that takes cargo arguments).
#   run-ops02-integration.sh container <repo> <workdir>
#       Starts the debian:trixie test container `ops02-it` with the Docker socket,
#       installs the tools and shares /var/lib/docker/ops02-it with the Docker host.
#   run-ops02-integration.sh run <repo> <workdir> <v1> <v2> <case>...
#       Runs ops02-cases.sh cases inside the container, logs below <workdir>/logs.
#   run-ops02-integration.sh cleanup <workdir> <v1> <v2>
#       Removes the container, the images craftcloud:<v1|v2>, and /var/lib/docker/ops02-it.
set -euo pipefail
export MSYS_NO_PATHCONV=1
cmd=${1:?usage: see the header of this file}; shift

case $cmd in
    release)
        repo=$1; work=$2; v1=$3; v2=$4
        scratch=$work/relrepo
        rm -rf "$scratch"; mkdir -p "$scratch"
        native() { cygpath -w "$1" 2>/dev/null || printf '%s' "$1"; }
        python -I - "$(native "$repo")" "$(native "$scratch")" <<'PY'
import os, shutil, subprocess, sys
repo, scratch = sys.argv[1:3]
names = subprocess.run(["git", "-C", repo, "ls-files", "-co", "--exclude-standard", "-z"], check=True, capture_output=True).stdout.split(b"\0")
for raw in filter(None, names):
    name = raw.decode()
    source = os.path.join(repo, name)
    if not os.path.isfile(source):
        continue
    target = os.path.join(scratch, name)
    os.makedirs(os.path.dirname(target), exist_ok=True)
    shutil.copy2(source, target)
PY
        scratch_native=$(native "$scratch")
        git -C "$scratch_native" init -q
        git -C "$scratch_native" -c core.autocrlf=false add -A
        git -C "$scratch_native" -c user.name=ops02 -c user.email=ops02@example.invalid commit -q -m "OPS-02 test snapshot (scratch, not the repository)"
        for v in "$v1" "$v2"; do
            ( cd "$scratch" && ${CARGO_LAUNCHER:?set CARGO_LAUNCHER} xtask release "$v" --image-arch amd64 )
        done
        ls -l "$scratch/dist"
        ;;
    container)
        repo=$1; work=$2
        docker rm -f ops02-it >/dev/null 2>&1 || true
        docker run -d --name ops02-it --network host --memory 3g \
            -v /var/run/docker.sock:/var/run/docker.sock -v /var/lib/docker/ops02-it:/var/lib/docker/ops02-it \
            -v "$work/relrepo/dist:/dist:ro" -v "$repo:/src:ro" debian:trixie sleep infinity >/dev/null
        docker cp "$repo/deploy/install/tests/integration/ops02-cases.sh" ops02-it:/usr/local/bin/ops02-cases.sh
        docker exec -e V1=x -e V2=x ops02-it bash /usr/local/bin/ops02-cases.sh setup
        ;;
    run)
        repo=$1; work=$2; v1=$3; v2=$4; shift 4
        mkdir -p "$work/logs"
        docker cp "$repo/deploy/install/tests/integration/ops02-cases.sh" ops02-it:/usr/local/bin/ops02-cases.sh
        for c in "$@"; do
            echo "##### $c"
            docker exec -e V1="$v1" -e V2="$v2" -e OUT=/var/lib/docker/ops02-it/logs ops02-it bash /usr/local/bin/ops02-cases.sh "$c" 2>&1 | tee "$work/logs/$c.log"
            test "${PIPESTATUS[0]}" -eq 0
            docker exec ops02-it sh -c 'cat /var/lib/docker/ops02-it/logs/* 2>/dev/null' >/dev/null || true
        done
        docker cp ops02-it:/var/lib/docker/ops02-it/logs/. "$work/logs/" 2>/dev/null || true
        ;;
    cleanup)
        work=$1; v1=$2; v2=$3
        docker rm -f ops02-it >/dev/null 2>&1 || true
        docker run --rm -v /var/lib/docker/ops02-it:/var/lib/docker/ops02-it debian:trixie sh -c 'rm -rf /var/lib/docker/ops02-it/* /var/lib/docker/ops02-it/.[!.]* 2>/dev/null; ls -A /var/lib/docker/ops02-it | wc -l'
        docker rmi "craftcloud:$v1" "craftcloud:$v2" >/dev/null 2>&1 || true
        # The scratch repository and its dist directory (release archives and image archives).
        rm -rf "${work:?}/relrepo"
        ;;
    *) echo "unknown command $cmd" >&2; exit 2 ;;
esac
