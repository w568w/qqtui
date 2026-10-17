"""Fullscreen redraw must work when the terminal never answers cursor reports."""

import argparse
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    binary = parser.parse_args().binary.resolve()
    with tempfile.TemporaryDirectory(prefix="qqtui-no-cpr-") as temporary:
        root = Path(temporary)
        env = dict(os.environ, TERM="xterm-256color")
        for name, directory in [("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache"), ("XDG_RUNTIME_DIR", "runtime")]:
            path = root / directory
            path.mkdir(mode=0o700)
            env[name] = str(path)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 960, 800))
        process = subprocess.Popen([str(binary), "--account", "default"], stdin=slave, stdout=slave, stderr=slave, env=env, start_new_session=True)
        os.close(slave)
        transcript = bytearray()
        capabilities_sent = False

        def drain(seconds):
            nonlocal capabilities_sent
            end = time.monotonic() + seconds
            start = len(transcript)
            while time.monotonic() < end:
                if select.select([master], [], [], min(0.05, max(0, end - time.monotonic())))[0]:
                    try:
                        data = os.read(master, 65536)
                    except OSError:
                        break
                    if not data:
                        break
                    transcript.extend(data)
                    if not capabilities_sent and b"\x1b[5n" in transcript:
                        os.write(master, b"\x1b[?1;2c\x1b[6;20;8t\x1b[0n")
                        capabilities_sent = True
            return bytes(transcript[start:])

        try:
            deadline = time.monotonic() + 12
            while b"qqtui" not in transcript:
                drain(0.1)
                assert process.poll() is None, transcript.decode(errors="replace")[-500:]
                assert time.monotonic() < deadline, "TUI did not start without terminal replies"
            for width, height in [(100, 30), (140, 45), (80, 24)]:
                fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, width * 8, height * 20))
                os.kill(process.pid, signal.SIGWINCH)
                # Reply to startup capabilities, but never to cursor reports (ESC[6n).
                redraw = drain(2.5)
                assert process.poll() is None, transcript.decode(errors="replace")[-500:]
                assert b"\x1b[6n" not in redraw, "Fullscreen redraw requested cursor position"
                assert b"qqtui" in redraw, "Resize did not redraw the fullscreen header"
            os.write(master, b"q")
            drain(0.3)
            assert process.wait(timeout=5) == 0, transcript.decode(errors="replace")[-500:]
            assert b"cursor position could not be read" not in transcript
            print("No-CPR terminal: three resizes/redraws and clean exit passed")
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            os.close(master)


if __name__ == "__main__":
    main()
