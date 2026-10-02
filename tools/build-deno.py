#!/usr/bin/env python3
"""Build Deno from a pinned commit with Slag as the JavaScript engine.

Deno selects its engine at the workspace root: the dependency named ``v8`` is
the ``deno_v8`` facade (``deno/libs/deno_v8``), and the facade's ``v8`` feature
(turned on by ``deno/cli/Cargo.toml``'s default) pulls in ``rusty_v8`` -- the
crates.io crate whose package name is literally ``v8``. This script redirects
that crate to Slag's implementation (``crates/v8``, which declares
``name = "v8"`` at exactly the version the facade requires) with a cargo
``[patch.crates-io]``.

The patch is handed to cargo through ``--config``, the same mechanism as Deno's
own ``.cargo/local-build.toml``, so the checkout's sources are never edited.

The checkout's ``Cargo.lock`` is reconciled once, on the first run. Slag's
dependency graph needs newer patch versions than the lock holds (it pulls
cranelift 0.134 alongside the checkout's 0.117, and unicode-normalization
0.1.25), and cargo will not move locked versions on its own. The full reconcile
also bumps ``locked-tripwire`` -- a crates.io trap that ``compile_error!``s in
every published version except the stub the lock pins -- so this script pins it
back afterwards.

Requires: git, a Rust toolchain (Deno's rust-toolchain.toml pins one), and
network access for the initial clone and the crate downloads.
"""

from __future__ import annotations

import argparse
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
V8_CRATE = REPO_ROOT / "crates" / "v8"

DENO_REPO = os.environ.get("DENO_REPO", "https://github.com/denoland/deno.git")
# `main` at the v2.9.7 release commit (v2.9.2-376-gabd22074e4), the revision the
# embedding work was validated against.
DEFAULT_PIN = "abd22074e47c6a5cd14e9e4e84743f084aa5a575"
CARGO = os.environ.get("CARGO", "cargo")

# The harmless version of the `locked-tripwire` trap the lock pins; every other
# published version of that crate fails to compile on purpose.
LOCKED_TRIPWIRE = "0.1.1"


def log(message: str) -> None:
    print(f"build-deno: {message}", file=sys.stderr)


def die(message: str) -> None:
    log(message)
    sys.exit(1)


def capture(command: list[str]) -> str:
    return subprocess.run(
        command, check=True, capture_output=True, text=True
    ).stdout.strip()


def cargo(deno_dir: Path, config: Path, args: list[str]) -> None:
    command = [CARGO, "--config", str(config), *args]
    try:
        subprocess.run(command, cwd=deno_dir, check=True)
    except subprocess.CalledProcessError as error:
        sys.exit(error.returncode)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "Environment:\n"
            "  DENO_DIR, DENO_PIN, DENO_REPO, CARGO\n"
            "\n"
            "Anything after `--` is forwarded to cargo."
        ),
    )
    parser.add_argument(
        "--deno-dir",
        default=os.environ.get("DENO_DIR", str(REPO_ROOT / "deno")),
        help="checkout location (default: <repo>/deno)",
    )
    parser.add_argument(
        "--pin",
        default=os.environ.get("DENO_PIN", DEFAULT_PIN),
        help=f"commit or tag to build (default: {DEFAULT_PIN})",
    )
    parser.add_argument(
        "--fetch",
        action="store_true",
        help="fetch origin before checking out the pin",
    )
    parser.add_argument(
        "--debug",
        action="store_true",
        help="build the dev profile (default: release)",
    )
    parser.add_argument(
        "--bin",
        default="deno",
        help="cargo package to build (default: deno)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the cargo invocation instead of running it",
    )
    return parser.parse_args(argv)


def ensure_checkout(deno_dir: Path, pin: str, fetch: bool) -> None:
    if not (deno_dir / ".git").is_dir():
        log(f"cloning {DENO_REPO} into {deno_dir}")
        deno_dir.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "clone", DENO_REPO, str(deno_dir)], check=True)

    if fetch:
        log("fetching origin")
        subprocess.run(
            ["git", "-C", str(deno_dir), "fetch", "--tags", "origin"], check=True
        )

    if capture(["git", "-C", str(deno_dir), "rev-parse", "HEAD"]) == pin:
        return

    # Moving the checkout: require a pristine tree so no local work is at risk.
    dirty = capture(
        ["git", "--no-optional-locks", "-C", str(deno_dir), "status", "--porcelain"]
    )
    if dirty:
        die(f"{deno_dir} has uncommitted changes; commit, stash, or pass another --deno-dir")

    log(f"checking out {pin}")
    subprocess.run(
        ["git", "-C", str(deno_dir), "checkout", "--detach", pin], check=True
    )


def write_patch_config() -> Path:
    """A temp cargo config redirecting crates.io `v8` to Slag's crate.

    The path is absolute and posix-form: absolute so it does not depend on the
    directory cargo runs in, and posix so it is a valid TOML basic string on
    Windows (backslashes would be escapes).
    """
    handle, name = tempfile.mkstemp(prefix="slag-v8-", suffix=".toml")
    os.close(handle)
    config = Path(name)
    config.write_text(
        f'[patch.crates-io]\nv8 = {{ path = "{V8_CRATE.as_posix()}" }}\n',
        encoding="utf-8",
    )
    return config


def lock_needs_reconcile(lock_path: Path) -> bool:
    """Whether the lock still resolves `v8` from the registry.

    A patched `v8` is a path dependency, so its lock entry has no `source`
    line; a registry one has `source = "registry+..."`.
    """
    if not lock_path.is_file():
        return True
    text = lock_path.read_text(encoding="utf-8", errors="replace")
    match = re.search(
        r'\[\[package\]\]\nname = "v8"\n(.*?)(?=\n\[\[package\]\]|\Z)',
        text,
        re.DOTALL,
    )
    return match is None or "source = " in match.group(1)


def main() -> None:
    argv = sys.argv[1:]
    extra: list[str] = []
    if "--" in argv:
        split = argv.index("--")
        extra = argv[split + 1 :]
        argv = argv[:split]
    args = parse_args(argv)

    if shutil.which("git") is None:
        die("git not found")
    if shutil.which(CARGO) is None:
        die("cargo not found (set CARGO)")
    if not (V8_CRATE / "Cargo.toml").is_file():
        die(f"missing {V8_CRATE / 'Cargo.toml'}")

    deno_dir = Path(args.deno_dir).resolve()
    ensure_checkout(deno_dir, args.pin, args.fetch)

    config = write_patch_config()
    try:
        log(f"engine: v8 -> {V8_CRATE}")
        log(f"deno:   {args.pin} at {deno_dir}")

        build = ["build", "-p", args.bin]
        profile = "debug"
        if not args.debug:
            build.append("--release")
            profile = "release"
        build += extra

        reconcile = lock_needs_reconcile(deno_dir / "Cargo.lock")

        if args.dry_run:
            if reconcile:
                log("(Cargo.lock would be reconciled first)")
            log(" ".join(shlex.quote(part) for part in [CARGO, "--config", str(config), *build]))
            return

        if reconcile:
            log("reconciling Cargo.lock for the patched engine (updates transitive versions)")
            cargo(deno_dir, config, ["update"])
            cargo(
                deno_dir,
                config,
                ["update", "-p", "locked-tripwire", "--precise", LOCKED_TRIPWIRE],
            )

        log(f"building {args.bin} ({profile}; first build compiles the whole workspace)")
        cargo(deno_dir, config, build)

        executable = ".exe" if os.name == "nt" else ""
        log(f"built {deno_dir / 'target' / profile / (args.bin + executable)}")
    finally:
        config.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
