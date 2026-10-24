#!/usr/bin/env python3
"""Check or build celld against Slag's ``v8`` bridge.

celld is a second Rust host on the bridge (``v8 = "152.1"``), and unlike Deno it
lives in its own workspace with its own lockfile. Two things follow:

* The engine is redirected with a cargo ``[patch.crates-io]`` handed to
  ``--config``, so celld's sources are never edited. The checkout's manifest
  requirement is only *checked*, not rewritten: celld is a consumer of the
  bridge, and a requirement that no longer accepts the bridge's version is a
  decision for its own tree.
* celld's native dependencies (SQLite, OpenSSL's fork, the tunnel stack) do not
  build for Windows, so on Windows this script runs cargo inside WSL. The probe
  that validates the bridge is ``cargo check``; ``--build`` asks for the binary.

The default is ``cargo check -p celld`` because that is what answers the
question this tool exists for — "does the bridge still support a second host?".
A build compiles the whole workspace and, in the release profile, links with
fat LTO.

The checkout is cloned on first use and pinned to a commit, the same way
``build-deno.py`` treats its tree: a branch or short ref can be repointed after
review, so only a full commit id is accepted. celld's ``Cargo.lock`` is allowed
to be dirty when moving the pin, because reconciling it for the patched engine
is something this script does.

Requires: git, cargo (in the distro when running under WSL), and network access
for the initial clone.
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

CELLD_REPO = os.environ.get("CELLD_REPO", "https://github.com/denoland/celld.git")
DEFAULT_REPO = "https://github.com/denoland/celld.git"
# The v0.6.3 release commit (tag `v0.6.3`), the revision the embedding work is
# validated against.
DEFAULT_PIN = "8702f6d186c77698dbfb30cf29e3b1922a88f008"
CARGO = os.environ.get("CARGO", "cargo")


def log(message: str) -> None:
    print(f"build-celld: {message}", file=sys.stderr)


def die(message: str) -> None:
    log(message)
    sys.exit(1)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "Environment:\n"
            "  CELLD_DIR, CELLD_PIN, CELLD_REPO, CELLD_WSL_DISTRO, CARGO\n"
            "\n"
            "Anything after `--` is forwarded to cargo."
        ),
    )
    parser.add_argument(
        "--celld-dir",
        default=os.environ.get("CELLD_DIR", str(REPO_ROOT / "celld")),
        help="celld checkout (default: <repo>/celld)",
    )
    parser.add_argument(
        "--pin",
        default=os.environ.get("CELLD_PIN", DEFAULT_PIN),
        help=f"commit to build (default: {DEFAULT_PIN}, v0.6.3)",
    )
    parser.add_argument(
        "--fetch",
        action="store_true",
        help="fetch origin before checking out the pin",
    )
    parser.add_argument(
        "--build",
        action="store_true",
        help="build the binary instead of running `cargo check`",
    )
    parser.add_argument(
        "--release",
        action="store_true",
        help="build the release profile (with --build; fat LTO, slow)",
    )
    parser.add_argument(
        "--wsl-distro",
        default=os.environ.get("CELLD_WSL_DISTRO"),
        help="WSL distro to run in (default: $CELLD_WSL_DISTRO, else the first one)",
    )
    parser.add_argument(
        "--native",
        action="store_true",
        help="run cargo here instead of in WSL (Linux/macOS, or a Windows whose "
        "native dependencies build)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the cargo invocation instead of running it",
    )
    return parser.parse_args(argv)


def capture(command: list[str]) -> str:
    return subprocess.run(
        command, check=True, capture_output=True, text=True
    ).stdout.strip()


def ensure_checkout(celld_dir: Path, pin: str, fetch: bool) -> None:
    # A branch or short ref can be moved or repointed after review; only a
    # full commit id is a reproducible supply-chain input.
    if len(pin) != 40 or any(c not in "0123456789abcdef" for c in pin):
        die(f"pin must be a full 40-hex commit, got {pin!r}")
    if not (celld_dir / ".git").is_dir():
        log(f"cloning {CELLD_REPO} into {celld_dir}")
        celld_dir.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "clone", CELLD_REPO, str(celld_dir)], check=True)

    if fetch:
        log("fetching origin")
        subprocess.run(
            ["git", "-C", str(celld_dir), "fetch", "--tags", "origin"], check=True
        )

    if capture(["git", "-C", str(celld_dir), "rev-parse", "HEAD"]) == pin:
        return

    # Moving the checkout: require a pristine tree so no local work is at risk.
    # `Cargo.lock` is exempt — reconciling it for the patched engine is this
    # script's own doing, and it is regenerated.
    dirty = [
        line
        for line in capture(
            ["git", "--no-optional-locks", "-C", str(celld_dir), "status", "--porcelain"]
        ).splitlines()
        if not line.endswith("Cargo.lock")
    ]
    if dirty:
        die(
            f"{celld_dir} has uncommitted changes; commit, stash, or pass another --celld-dir"
        )

    log(f"checking out {pin}")
    subprocess.run(
        ["git", "-C", str(celld_dir), "checkout", "--detach", pin], check=True
    )


def bridge_version() -> str:
    """The version `crates/v8` declares, which is what a consumer must accept."""
    text = (V8_CRATE / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r'^version = "([^"]+)"', text, re.MULTILINE)
    if match is None:
        die(f"missing a version in {V8_CRATE / 'Cargo.toml'}")
    return match.group(1)


def accepts(celld_dir: Path, version: str) -> bool:
    """Whether celld's `v8` requirement admits the bridge's version.

    The requirement is a caret range with an optional patch component; cargo
    only applies a patch when the patched version satisfies it, so a mismatch
    would otherwise surface as "the patch was not used" with no explanation.
    """
    text = (celld_dir / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(
        r'^v8\s*=\s*"([^"]+)"', text, re.MULTILINE
    ) or re.search(r'^v8\s*=\s*\{\s*version\s*=\s*"([^"]+)"', text, re.MULTILINE)
    if match is None:
        return False
    required = tuple(int(part) for part in match.group(1).split(".") if part.isdigit())
    bridged = tuple(int(part) for part in version.split(".") if part.isdigit())
    # A caret requirement leading zeros (e.g. "0.2") admits only its own minor;
    # 152.1 and above admit every later patch of minor 1.
    if required[0] == 0:
        return bridged[:2] == required[:2]
    if len(required) >= 2 and bridged[0] == required[0]:
        return bridged[: len(required)] >= required
    return bridged[0] == required[0]


def write_patch_config(v8_path: str) -> Path:
    """A temp cargo config redirecting crates.io `v8` to Slag's crate.

    `v8_path` is spelled the way the *cargo that reads it* sees the filesystem:
    a WSL path when the build runs inside WSL, a Windows one otherwise.
    """
    handle, name = tempfile.mkstemp(prefix="slag-celld-", suffix=".toml")
    os.close(handle)
    config = Path(name)
    config.write_text(
        f'[patch.crates-io]\nv8 = {{ path = "{v8_path}" }}\n',
        encoding="utf-8",
    )
    return config


def wsl_path(path: Path) -> str:
    """A Windows path as WSL sees it (`C:\\a\\b` -> `/mnt/c/a/b`)."""
    posix = path.resolve().as_posix()
    if len(posix) < 2 or posix[1] != ":":
        die(f"{path} is not on a drive WSL mounts")
    return f"/mnt/{posix[0].lower()}{posix[2:]}"


def detect_distro() -> str | None:
    """The first installed WSL distro that is a developer distro.

    Container hosts (Docker Desktop's, Podman's machine VM) are skipped: they
    run their own VM and are not where a toolchain lives.
    """
    try:
        completed = subprocess.run(
            ["wsl", "-l", "-q"], capture_output=True, check=True
        )
    except (OSError, subprocess.CalledProcessError):
        return None
    text = completed.stdout.decode("utf-16-le", errors="replace")
    for line in text.splitlines():
        name = line.strip().strip("\x00").strip()
        if not name or name.lower().startswith(("docker", "podman")):
            continue
        return name
    return None


def cargo(args: list[str], celld_dir: Path, config: Path, distro: str | None) -> None:
    if distro is None:
        command = [CARGO, "--config", str(config), *args]
        try:
            subprocess.run(command, cwd=celld_dir, check=True)
        except subprocess.CalledProcessError as error:
            sys.exit(error.returncode)
        return

    inner = "cd {} && {}".format(
        shlex.quote(wsl_path(celld_dir)),
        shlex.join([CARGO, "--config", wsl_path(config), *args]),
    )
    try:
        subprocess.run(
            ["wsl", "-d", distro, "--", "bash", "-lc", inner], check=True
        )
    except subprocess.CalledProcessError as error:
        sys.exit(error.returncode)


def lock_needs_reconcile(lock_path: Path, version: str) -> bool:
    """Whether the lock still resolves `v8` from the registry, or at another
    version than the bridge's.

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
    if match is None or "source = " in match.group(1):
        return True
    locked = re.search(r'^version = "([^"]+)"', match.group(1), re.MULTILINE)
    return locked is None or locked.group(1) != version


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

    celld_dir = Path(args.celld_dir).resolve()
    if CELLD_REPO != DEFAULT_REPO:
        log(f"warning: CELLD_REPO overrides the default repository: {CELLD_REPO}")
    ensure_checkout(celld_dir, args.pin, args.fetch)

    version = bridge_version()
    if not accepts(celld_dir, version):
        die(
            f"celld's `v8` requirement does not admit {version}; "
            f"update `v8 = ...` in {celld_dir / 'Cargo.toml'}"
        )

    distro: str | None = None
    if os.name == "nt" and not args.native:
        distro = args.wsl_distro or detect_distro()
        if distro is None:
            die("no WSL distro found; pass --wsl-distro or --native")

    if distro is None and shutil.which(CARGO) is None:
        die("cargo not found (set CARGO, or use --wsl-distro)")

    config = write_patch_config(wsl_path(V8_CRATE) if distro else V8_CRATE.as_posix())
    profile = "release" if args.release else "debug"
    command = [("build" if args.build else "check"), "-p", "celld", "--locked"]
    if args.build and args.release:
        command.append("--release")
    command += extra

    try:
        log(f"engine: v8 {version} -> {V8_CRATE}")
        log(f"celld:  {args.pin} at {celld_dir}" + (f" (WSL: {distro})" if distro else ""))

        reconcile = lock_needs_reconcile(celld_dir / "Cargo.lock", version)
        if args.dry_run:
            if reconcile:
                log("(Cargo.lock would be reconciled first)")
            if distro is None:
                log(
                    " ".join(
                        shlex.quote(part)
                        for part in [CARGO, "--config", str(config), *command]
                    )
                )
            else:
                log(
                    "wsl -d {} -- bash -lc {}".format(
                        distro,
                        shlex.quote(
                            "cd {} && {}".format(
                                shlex.quote(wsl_path(celld_dir)),
                                shlex.join(
                                    [
                                        CARGO,
                                        "--config",
                                        wsl_path(config),
                                        *command,
                                    ]
                                ),
                            )
                        ),
                    )
                )
            return

        if reconcile:
            log("reconciling Cargo.lock for the patched engine")
            cargo(["update", "-p", "v8"], celld_dir, config, distro)

        log(f"{'building' if args.build else 'checking'} celld ({profile})")
        cargo(command, celld_dir, config, distro)

        if args.build:
            executable = "" if distro else (".exe" if os.name == "nt" else "")
            log(f"built {celld_dir / 'target' / profile / ('celld' + executable)}")
    finally:
        config.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
