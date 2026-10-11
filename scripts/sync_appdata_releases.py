#!/usr/bin/env python3
"""Regenerates the <releases> list in the AppStream metadata from CHANGELOG.md.

The changelog has one `## <version> — <date>` heading per release, so it is the
in-tree source of truth (git tags are not reliably present in a shallow CI checkout).
Also verifies that the newest release is the version in Cargo.toml.

  sync_appdata_releases.py           rewrite the metadata in place
  sync_appdata_releases.py --check   exit 1 if it is out of date (for CI)
  sync_appdata_releases.py --self-test
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
APPDATA = next((ROOT / "data").glob("*.appdata.xml"))
HEADING = re.compile(r"^## (\d+\.\d+\.\d+) — (\d{4}-\d{2}-\d{2})\s*$", re.M)


def cargo_version() -> str:
    match = re.search(r'^version\s*=\s*"([^"]+)"', (ROOT / "Cargo.toml").read_text(), re.M)
    return match.group(1)


def releases():
    seen, out = set(), []
    for version, date in HEADING.findall((ROOT / "CHANGELOG.md").read_text()):
        if version not in seen:
            seen.add(version)
            out.append((version, date))
    # Newest first, by numeric version.
    return sorted(out, key=lambda r: tuple(int(p) for p in r[0].split(".")), reverse=True)


RELEASES = re.compile(r"  <releases>.*?</releases>", re.S)


def render(text: str, rel) -> str:
    """The metadata with its one <releases> block regenerated. Raises ValueError when there isn't
    exactly one such block: substituting into nothing would leave the file unchanged and let
    --check pass with no release list at all."""
    found = len(RELEASES.findall(text))
    if found != 1:
        raise ValueError(f"{APPDATA.name} must have exactly one indented `  <releases>...</releases>` block, found {found}")
    block = "  <releases>\n" + "".join(f'    <release version="{v}" date="{d}"/>\n' for v, d in rel) + "  </releases>"
    return RELEASES.sub(lambda _m: block, text)


def _self_test() -> None:
    rel = [("1.2.0", "2026-01-02"), ("1.0.0", "2025-01-01")]
    good = '<component>\n  <releases>\n    <release version="1.0.0" date="2025-01-01"/>\n  </releases>\n</component>\n'
    out = render(good, rel)
    assert '    <release version="1.2.0" date="2026-01-02"/>\n    <release version="1.0.0"' in out, out
    assert render(out, rel) == out
    for bad in ("<component>\n</component>\n", "<component><releases></releases></component>\n", good + good):
        try:
            render(bad, rel)
        except ValueError:
            continue
        raise AssertionError(f"accepted metadata without exactly one <releases> block: {bad!r}")
    print("sync_appdata_releases self-test ok")


def main() -> int:
    if "--self-test" in sys.argv[1:]:
        _self_test()
        return 0
    rel = releases()
    if not rel or rel[0][0] != cargo_version():
        print(f"newest CHANGELOG.md release ({rel[0][0] if rel else None}) is not the Cargo.toml version "
              f"{cargo_version()}: add its `## {cargo_version()} — YYYY-MM-DD` heading", file=sys.stderr)
        return 1
    current = APPDATA.read_text()
    try:
        updated = render(current, rel)
    except ValueError as e:
        print(e, file=sys.stderr)
        return 1
    if "--check" in sys.argv[1:]:
        if updated != current:
            print("appdata.xml <releases> is out of date; run scripts/sync_appdata_releases.py", file=sys.stderr)
            return 1
        print("AppStream releases: OK")
        return 0
    APPDATA.write_text(updated)
    return 0


if __name__ == "__main__":
    sys.exit(main())
