"""Validate an Arch package and its system dependencies in an isolated user home."""
import argparse
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("package", type=Path)
    parser.add_argument("--qq-path", type=Path, default=Path("/opt/QQ"))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="qqtui-native-package-") as temp:
        stage = Path(temp)
        subprocess.run(["bsdtar", "-xf", str(args.package.resolve()), "-C", temp], check=True)
        metadata = stage.joinpath(".PKGINFO").read_text()
        for name in ["dbus", "systemd", "xorg-server-xvfb", "xorg-xauth", "xorg-xkbcomp", "tigervnc", "novnc"]:
            assert f"depend = {name}\n" in metadata, name
        binary = stage / "usr/bin/qqtui"
        assert binary.read_bytes().startswith(b"\x7fELF")
        assert not stage.joinpath("usr/lib/qqtui").exists()
        assert not any(p.name.startswith("libc.so") for p in stage.rglob("*"))
        for unit in ["qqtui-host.service", "qqtui-host@.service"]:
            assert "/usr/bin/qqtui" in stage.joinpath("usr/lib/systemd/user", unit).read_text()
        print("Native ELF, pacman dependencies and systemd units: passed", flush=True)
        command = [
            "bwrap", "--die-with-parent", "--unshare-user", "--unshare-pid", "--unshare-net", "--unshare-ipc",
            "--ro-bind", "/usr", "/usr", "--ro-bind", "/etc", "/etc",
            "--ro-bind", str(args.qq_path.resolve()), "/opt/QQ",
            "--symlink", "usr/lib", "/lib", "--symlink", "usr/lib", "/lib64", "--symlink", "usr/bin", "/bin",
            "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--tmpfs", "/home", "--dir", "/run/user/1000",
            "--ro-bind", str(binary), "/usr/bin/qqtui", "--clearenv",
            "--setenv", "HOME", "/home/全新 用户", "--setenv", "XDG_RUNTIME_DIR", "/run/user/1000",
            "--setenv", "PATH", "/usr/bin", "--setenv", "LANG", "C.UTF-8",
            "/usr/bin/python3", "-c", ROOT.joinpath("tests/package_sandbox.py").read_text(),
        ]
        subprocess.run(command, check=True, timeout=150)


if __name__ == "__main__":
    main()
