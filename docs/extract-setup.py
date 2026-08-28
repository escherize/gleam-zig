#!/usr/bin/env python3
"""Print the setup shell commands exactly as the homepage shows them.

The page's getting-started block is the path a new reader takes. It is worth
testing, but only if the test runs the same text the reader sees; a copy would
drift from the page within a week. So this extracts the block rather than
restating it.

Usage:
  python3 docs/extract-setup.py           # the first setup block
  python3 docs/extract-setup.py --all     # every shell block on the page

Output is shell, so it can be piped straight into a container:
  python3 docs/extract-setup.py | docker run -i --rm ubuntu:24.04 bash -s
"""

from __future__ import annotations

import html as html_mod
import re
import sys
from pathlib import Path

INDEX = Path(__file__).resolve().parent / "index.html"

# The getting-started block is the one that clones the workspace.
SETUP_MARKER = "gleam-zig-workspace"


def shell_blocks(page: str) -> list[str]:
    """Every <pre class="block"> on the page, as plain shell."""
    out = []
    for raw in re.findall(r'<pre class="block">(.*?)</pre>', page, re.S):
        text = re.sub(r"<[^>]+>", "", raw)
        text = html_mod.unescape(text)
        out.append(text.strip("\n"))
    return out


def main() -> int:
    if not INDEX.exists():
        print(f"index.html not found at {INDEX}", file=sys.stderr)
        return 1

    blocks = shell_blocks(INDEX.read_text(encoding="utf-8"))
    if not blocks:
        print("no shell blocks found; has the markup changed?", file=sys.stderr)
        return 1

    if "--all" in sys.argv:
        print("\n\n".join(blocks))
        return 0

    setup = [b for b in blocks if SETUP_MARKER in b]
    if not setup:
        print(
            f"no block mentions {SETUP_MARKER}; the setup block moved or was renamed",
            file=sys.stderr,
        )
        return 1

    print(setup[0])
    return 0


if __name__ == "__main__":
    sys.exit(main())
