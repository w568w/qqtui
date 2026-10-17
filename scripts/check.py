#!/usr/bin/env python3
"""Run the same bounded development checks locally and in CI."""

import argparse
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def run(*command):
    subprocess.run(command, cwd=ROOT, check=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--terminal", action="store_true")
    parser.add_argument("--package", type=Path)
    args = parser.parse_args()
    run("cargo", "fmt", "--check")
    run("cargo", "test", "--locked")
    run("cargo", "clippy", "--all-targets", "--locked", "--", "-D", "warnings")
    run("node", "--test", "tests/native.test.cjs", "tests/journal.test.cjs", "tests/desktop.test.cjs")
    if sys.platform == "linux":
        run(sys.executable, "tests/loader.test.py")
    if args.terminal:
        run("cargo", "build", "--locked")
        for test in [
            "account-picker",
            "terminal-resize",
            "desktop-notifications",
            "tui-history",
        ]:
            run(
                sys.executable,
                f"tests/{test}.test.py",
                "--binary",
                str(ROOT / "target/debug/qqtui"),
            )
    if args.package:
        run(sys.executable, "tests/package.test.py", str(args.package.resolve()))

if __name__ == "__main__":
    main()
