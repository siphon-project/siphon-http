#!/usr/bin/env python3
"""Guard: the siphon-sip SDK mock of the ``http`` namespace must not drift.

The siphon-sip SDK (``pip install siphon-sip``) ships a mock of the ``http``
namespace that this crate injects into siphon at runtime, so HTTP scripts can be
unit-tested and authored with type hints without binding a real listener. This
script derives the namespace surface from this repo's ``python/http.py`` and
asserts every exposed name — the ``route`` / ``middleware`` / ``on_startup``
decorators, the ``Request`` / ``Response`` / ``Client`` pyclasses, and their
public methods — is present on the installed SDK mock.

Run in CI after ``pip install siphon-sip``. Exits non-zero (listing the missing
names) if the mock is behind — update ``sdk/siphon_sdk/http.py`` in siphon-sip to
match, then land that before this repo's change.
"""

from __future__ import annotations

import ast
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HTTP_PY = ROOT / "python" / "http.py"

_FUNC = (ast.FunctionDef, ast.AsyncFunctionDef)


def parse_surface() -> tuple[set[str], dict[str, list[str]]]:
    """Return (top-level decorator/function names, {class: [public methods]})
    from ``python/http.py``."""
    tree = ast.parse(HTTP_PY.read_text())
    funcs: set[str] = set()
    classes: dict[str, list[str]] = {}
    for node in tree.body:
        if isinstance(node, _FUNC) and not node.name.startswith("_"):
            funcs.add(node.name)
        elif isinstance(node, ast.ClassDef):
            classes[node.name] = [
                child.name
                for child in node.body
                if isinstance(child, _FUNC) and not child.name.startswith("_")
            ]
    return funcs, classes


def main() -> int:
    try:
        from siphon_sdk import mock_module
    except ImportError:
        print(
            "ERROR: siphon-sip SDK not installed — run `pip install siphon-sip`",
            file=sys.stderr,
        )
        return 2

    mock_module.install()
    from siphon import http  # resolvable after install()

    funcs, classes = parse_surface()
    top = funcs | set(classes)
    if not top:
        print("ERROR: derived an empty http surface — parser out of date?",
              file=sys.stderr)
        return 2

    missing: list[str] = []
    for name in top:
        if not hasattr(http, name):
            missing.append(f"http.{name}")
    for cls_name, methods in classes.items():
        mock_cls = getattr(http, cls_name, None)
        if mock_cls is None:
            continue  # already reported as a missing top-level name
        for method in methods:
            if not hasattr(mock_cls, method):
                missing.append(f"http.{cls_name}.{method}")

    total = len(top) + sum(len(m) for m in classes.values())
    print(f"http namespace surface: {total} names checked")
    if missing:
        print(
            "\nMISSING from the siphon-sip SDK mock (sdk/siphon_sdk/http.py):",
            file=sys.stderr,
        )
        for name in sorted(missing):
            print(f"  - {name}", file=sys.stderr)
        print(
            "\nAdd them to sdk/siphon_sdk/http.py in siphon-sip and merge that first.",
            file=sys.stderr,
        )
        return 1

    print("OK — the SDK mock covers the full http runtime surface.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
