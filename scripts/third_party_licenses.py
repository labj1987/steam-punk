#!/usr/bin/env python3
"""Writes the notices of the Rust crates built into the binary: third_party_licenses.py OUT.txt

Lists every crate in the release build for x86_64 Linux (`cargo metadata --locked`, normal and
build dependencies of the app, not dev-dependencies) with its version, licence and source, then
the licence texts found in the crates' own sources (LICENSE*, LICENCE*, COPYING*, NOTICE*,
UNLICENSE*), each printed once with the crates it came from. Run after the crates are fetched
(any cargo build does that). Fails when a crate declares no licence, and with --self-test checks
the grouping on made-up data.
"""
import hashlib
import json
import os
import subprocess
import sys

TARGET = "x86_64-unknown-linux-gnu"
NAMES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE", "COPYRIGHT")


def built_packages(meta):
    """The packages reachable from the app through normal and build dependencies."""
    nodes = {node["id"]: node for node in meta["resolve"]["nodes"]}
    root = meta["resolve"]["root"]
    seen, todo = set(), [root]
    while todo:
        node = nodes[todo.pop()]
        for dep in node["deps"]:
            kinds = {kind["kind"] for kind in dep["dep_kinds"]}
            if kinds == {"dev"} or dep["pkg"] in seen:
                continue
            seen.add(dep["pkg"])
            todo.append(dep["pkg"])
    packages = {package["id"]: package for package in meta["packages"]}
    return sorted((packages[i] for i in seen), key=lambda p: (p["name"].lower(), p["version"]))


def licence_files(package):
    folder = os.path.dirname(package["manifest_path"])
    found = []
    try:
        names = sorted(os.listdir(folder))
    except OSError:
        return found
    for name in names:
        if name.upper().startswith(NAMES) and os.path.isfile(os.path.join(folder, name)):
            with open(os.path.join(folder, name), encoding="utf-8", errors="replace") as f:
                found.append(f.read().strip())
    return found


def group_texts(crate_texts):
    """[(crate, [text...])] -> [(text, [crate...])], each distinct text once, in first-seen order."""
    order, users = [], {}
    for crate, texts in crate_texts:
        for text in texts:
            key = hashlib.sha256(" ".join(text.split()).encode()).hexdigest()
            if key not in users:
                users[key] = (text, [])
                order.append(key)
            if crate not in users[key][1]:
                users[key][1].append(crate)
    return [users[key] for key in order]


def render(packages):
    missing = [p["name"] for p in packages if not (p.get("license") or p.get("license_file"))]
    if missing:
        sys.exit(f"error: no licence declared by: {', '.join(missing)}")
    out = [
        "Steam Punk is built with the open-source Rust crates listed here. Each line gives the",
        "crate, its version, its licence (SPDX) and where its source is. The licence texts found",
        "in the crates' sources follow the list.",
        "",
    ]
    crate_texts = []
    for p in packages:
        name = f"{p['name']} {p['version']}"
        where = p.get("repository") or "https://crates.io/crates/" + p["name"]
        out.append(f"{name}: {p.get('license') or 'see its licence file'} ({where})")
        crate_texts.append((name, licence_files(p)))
    for text, crates in group_texts(crate_texts):
        out += ["", "=" * 78, "Licence text of: " + ", ".join(crates), "=" * 78, "", text]
    return "\n".join(out) + "\n"


def self_test():
    grouped = group_texts([("a 1", ["MIT text\n", "Apache"]), ("b 2", ["MIT  text"]), ("c 3", [])])
    assert grouped == [("MIT text\n", ["a 1", "b 2"]), ("Apache", ["a 1"])], grouped
    meta = {
        "resolve": {
            "root": "app",
            "nodes": [
                {"id": "app", "deps": [
                    {"pkg": "x", "dep_kinds": [{"kind": None}]},
                    {"pkg": "t", "dep_kinds": [{"kind": "dev"}]},
                    {"pkg": "b", "dep_kinds": [{"kind": "build"}]}]},
                {"id": "x", "deps": [{"pkg": "y", "dep_kinds": [{"kind": None}]}]},
                {"id": "y", "deps": []}, {"id": "t", "deps": []}, {"id": "b", "deps": []},
            ],
        },
        "packages": [{"id": i, "name": i, "version": "1"} for i in ("app", "x", "y", "t", "b")],
    }
    assert [p["name"] for p in built_packages(meta)] == ["b", "x", "y"]
    print("third_party_licenses self-test ok")


def main():
    if sys.argv[1:] == ["--self-test"]:
        return self_test()
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    meta = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", TARGET]))
    text = render(built_packages(meta))
    with open(sys.argv[1], "w", encoding="utf-8") as f:
        f.write(text)


if __name__ == "__main__":
    main()
