#!/usr/bin/env python3
"""Fetches MSYS2 CLANG64 packages for building Lemma for Windows on another system.

    msys2.py DEST PACKAGE...

Resolves PACKAGE... and their dependencies from the CLANG64 repository's database, downloads
each package into DEST/pkgs (kept as a cache), checks it against the SHA-256 the database gives
and extracts it into DEST, so the libraries end up in DEST/clang64. Writes DEST/packages.txt
("name version" per line) and DEST/owners.txt ("path<TAB>package" per installed file, paths
relative to DEST), which `build.sh` uses to bundle the right licences.

MSYS2's own `pacman` does all of this on Windows; this is for cross builds (see README.md).
Needs `tar` with zstd support (GNU tar with zstd, or bsdtar).
"""

import hashlib
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request

REPO = "https://repo.msys2.org/mingw/clang64"
DB = "clang64.db"


def fetch(url, dest):
    """Downloads `url` to `dest` (through a temporary file), retrying a few times."""
    for attempt in range(4):
        try:
            tmp = dest + ".part"
            with urllib.request.urlopen(url, timeout=120) as response, open(tmp, "wb") as out:
                shutil.copyfileobj(response, out)
            os.replace(tmp, dest)
            return
        except OSError as e:
            if attempt == 3:
                sys.exit(f"msys2.py: cannot download {url}: {e}")
            time.sleep(2 * (attempt + 1))


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def read_db(path):
    """The packages of the repository database at `path`: name -> {FIELD: [values]}."""
    pkgs = {}
    with tempfile.TemporaryDirectory() as unpack:
        subprocess.run(["tar", "-xf", path, "-C", unpack], check=True)
        for entry in os.listdir(unpack):
            desc = os.path.join(unpack, entry, "desc")
            if not os.path.isfile(desc):
                continue
            with open(desc) as f:
                text = f.read()
            fields = {}
            for block in text.strip().split("\n\n"):
                lines = block.strip().split("\n")
                fields[lines[0].strip("%")] = lines[1:]
            pkgs[fields["NAME"][0]] = fields
    return pkgs


def resolve(pkgs, wanted):
    """`wanted` and everything they depend on, by package name."""
    provides = {}
    for name, fields in pkgs.items():
        for p in fields.get("PROVIDES", []):
            provides.setdefault(re.split("[<>=]", p)[0], name)

    def lookup(dep):
        n = re.split("[<>=]", dep)[0]
        return n if n in pkgs else provides.get(n)

    todo, seen = list(wanted), set()
    while todo:
        dep = todo.pop()
        name = lookup(dep)
        if name is None:
            sys.exit(f"msys2.py: no package provides {dep}")
        if name in seen:
            continue
        seen.add(name)
        todo.extend(pkgs[name].get("DEPENDS", []))
    return sorted(seen)


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    dest, wanted = os.path.abspath(sys.argv[1]), sys.argv[2:]
    cache = os.path.join(dest, "pkgs")
    os.makedirs(cache, exist_ok=True)
    db_path = os.path.join(cache, DB)
    fetch(f"{REPO}/{DB}", db_path)
    pkgs = read_db(db_path)
    names = resolve(pkgs, wanted)

    owners = []
    for name in names:
        fields = pkgs[name]
        filename, digest = fields["FILENAME"][0], fields["SHA256SUM"][0]
        archive = os.path.join(cache, filename)
        if not (os.path.exists(archive) and sha256(archive) == digest):
            fetch(f"{REPO}/{filename}", archive)
            if sha256(archive) != digest:
                sys.exit(f"msys2.py: {filename} does not match the repository's checksum")
        with tempfile.TemporaryDirectory(dir=dest) as unpack:
            subprocess.run(["tar", "-xf", archive, "-C", unpack], check=True)
            for root, _dirs, files in os.walk(unpack):
                for f in files:
                    src = os.path.join(root, f)
                    rel = os.path.relpath(src, unpack)
                    if rel.startswith("."):  # .PKGINFO, .BUILDINFO, .MTREE, .INSTALL
                        continue
                    target = os.path.join(dest, rel)
                    os.makedirs(os.path.dirname(target), exist_ok=True)
                    if os.path.lexists(target):
                        os.remove(target)
                    os.replace(src, target)
                    owners.append(f"{rel.replace(os.sep, '/')}\t{name}")
        print(f"{name} {fields['VERSION'][0]}")

    with open(os.path.join(dest, "packages.txt"), "w") as f:
        f.writelines(f"{n} {pkgs[n]['VERSION'][0]}\n" for n in names)
    with open(os.path.join(dest, "owners.txt"), "w") as f:
        f.writelines(line + "\n" for line in owners)


if __name__ == "__main__":
    main()
