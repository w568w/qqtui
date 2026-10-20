"""Render the TUI against a fixture backend in Xvfb and capture a README screenshot.

Requires Kitty, Xvfb, xauth, ffmpeg and Python Pillow.
Usage: python3 scripts/screenshot.py [--binary target/debug/qqtui] [--output docs/screenshot.png]
"""

import argparse
import json
import os
from pathlib import Path
import socketserver
import subprocess
import tempfile
import threading
import time
import uuid

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[1]
GROUP = {"chat_type": 2, "uid": "group"}
FRIEND = {"chat_type": 1, "uid": "yunyan"}
ACCOUNT = "10001"
EPOCH = "fixture-host"


def msg(number, peer, sender, sender_uid, text, **extra):
    base = dict(
        account=ACCOUNT,
        peer=peer,
        id=str(number),
        seq=str(number),
        time=str(1760900000 + number),
        sender=sender,
        sender_uid=sender_uid,
        text=text,
        parts=[],
        attachments=[],
        reply_to=None,
        is_self=False,
        send_status=2,
    )
    base.update(extra)
    return base


def paint_photo(path):
    """A small landscape photo used as the image message."""
    width, height = 400, 225
    image = Image.new("RGB", (width, height))
    draw = ImageDraw.Draw(image)
    for y in range(height):
        t = y / height
        r = int(30 + 90 * t)
        g = int(60 + 110 * t)
        b = int(120 + 90 * t)
        draw.line([(0, y), (width, y)], fill=(r, g, b))
    draw.ellipse([340, 30, 410, 100], fill=(255, 236, 170))
    draw.polygon([(0, 200), (130, 110), (260, 210)], fill=(36, 74, 60))
    draw.polygon([(180, 210), (330, 90), (480, 215)], fill=(28, 60, 52))
    draw.rectangle([0, 210, width, height], fill=(52, 96, 70))
    image.save(path)


def paint_avatar(path, color, letter):
    size = 96
    image = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    draw.ellipse([0, 0, size, size], fill=color)
    draw.ellipse([8, 8, size - 8, size - 8], fill=tuple(min(255, c + 40) for c in color[:3]))
    image.save(path)


class Fixture(socketserver.ThreadingUnixStreamServer):
    daemon_threads = True

    def __init__(self, path, media):
        self.sequence = 1
        self.clients = []
        self.lock = threading.Lock()
        photo, avatars = media
        photo = str(photo)
        reaction = dict(
            emoji_id="14", emoji_type="1", count="2", clicked=False, label="微笑", path=""
        )
        self.messages = [
            msg(101, GROUP, "火、死亡、离别", "fire", "看 modelcard 很穷啊。"),
            msg(
                102,
                GROUP,
                "美波",
                "meibo",
                "看了下确实，很多预览图明显不是上时代本地绘图模型能做的。",
                parts=[
                    {"type": "text", "text": "看了下确实，很多预览图明显不是上时代本地绘图模型能做的。"},
                    {"type": "attachment", "element_id": "photo"},
                ],
                attachments=[
                    dict(
                        element_id="photo",
                        kind="image",
                        name="photo.png",
                        path=photo,
                        size="240000",
                        width=400,
                        height=225,
                    )
                ],
            ),
            msg(
                103,
                GROUP,
                "云烟",
                "yunyan",
                "@美波 什么游戏",
                reply_to="102",
                reply_seq="102",
            ),
            msg(
                104,
                GROUP,
                "火、死亡、离别",
                "fire",
                "我不敢相信这是本地绘图的理解力能做出来的东西",
                reactions=[reaction],
            ),
            msg(
                105,
                GROUP,
                "",
                "",
                "美波 撤回了一条消息",
                system_notice=True,
                recalled=True,
            ),
            msg(106, GROUP, "云烟", "yunyan", "明白了，晚上试试"),
        ]
        self.avatars = {uid: str(p) for uid, p in avatars.items()}
        super().__init__(str(path), Handler)

    def status(self):
        return dict(
            protocol_version=2,
            phase="ready",
            account=ACCOUNT,
            epoch=EPOCH,
            seq=self.sequence,
            errors=[],
            capabilities=[
                "history_navigation",
                "request_context",
                "reactions",
                "stickers",
                "unread_anchor",
                "avatars",
                "speech_permissions",
            ],
        )

    def call(self, method, params):
        if method in ["host.status", "events.subscribe"]:
            return self.status()
        if method == "accounts.list":
            return dict(items=[])
        if method == "avatars.resolve":
            return dict(path=self.avatars.get(params["uid"], ""), version="v1")
        if method == "conversations.permissions":
            return dict(state="allowed", scope="none", reason="", until=None)
        if method == "conversations.unread":
            return dict(id="101")
        if method == "messages.read":
            return {}
        if method == "messages.history":
            return dict(messages=self.messages, has_before=True, has_after=False, anchor_found=True)
        if method == "messages.quote":
            return dict(messages=[m for m in self.messages if m["id"] == params.get("id")])
        if method == "conversations.snapshot":
            return dict(
                account=ACCOUNT,
                epoch=EPOCH,
                seq=self.sequence,
                unread_total="6",
                conversations=[
                    dict(peer=GROUP, id="group", name="这里是说怪话阵线", unread="3", pinned=True, preview="云烟：明白了，晚上试试", last_id="106"),
                    dict(peer=FRIEND, id="yunyan", name="云烟", unread="1", pinned=False, preview="晚上吃什么", last_id="21"),
                    dict(peer={"chat_type": 8, "uid": "device"}, id="device", name="我的手机", unread="0", pinned=False, preview="[图片]", last_id="9"),
                    dict(peer={"chat_type": 103, "uid": "mail"}, id="mail", name="QQ 邮箱提醒", unread="2", pinned=False, preview="每周新鲜事", last_id="31"),
                ],
            )
        if method == "messages.get" or method == "reactions.refresh":
            return dict(messages=[self.messages[3]])
        raise AssertionError(f"unexpected fixture call: {method} {params}")


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        self.server.clients.append(self)
        try:
            for line in self.rfile:
                request = json.loads(line)
                with self.server.lock:
                    try:
                        result = self.server.call(request["method"], request.get("params", {}))
                        self.write(dict(jsonrpc="2.0", id=request["id"], result=result))
                    except Exception as error:
                        self.write(dict(jsonrpc="2.0", id=request["id"], error=dict(code=-32000, message=str(error))))
        finally:
            self.server.clients.remove(self)

    def write(self, frame):
        self.wfile.write((json.dumps(frame) + "\n").encode())
        self.wfile.flush()


def wait_for(predicate, timeout=20):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return
        time.sleep(0.1)
    raise TimeoutError("fixture condition was not met")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/qqtui")
    parser.add_argument("--output", type=Path, default=ROOT / "docs/screenshot.png")
    args = parser.parse_args()
    binary = args.binary.resolve()
    output = args.output.resolve()

    with tempfile.TemporaryDirectory(prefix="qqtui-shot-", dir=os.environ["XDG_RUNTIME_DIR"]) as temporary:
        base = Path(temporary)
        runtime = base / "runtime/qqtui"
        runtime.mkdir(parents=True)
        photo = base / "photo.png"
        paint_photo(photo)
        avatars = {
            "fire": base / "fire.png",
            "meibo": base / "meibo.png",
            "yunyan": base / "yunyan.png",
        }
        paint_avatar(avatars["fire"], (224, 122, 95, 255), "火")
        paint_avatar(avatars["meibo"], (97, 175, 190, 255), "美")
        paint_avatar(avatars["yunyan"], (155, 133, 199, 255), "云")
        server = Fixture(runtime / "bridge.sock", (photo, avatars))
        threading.Thread(target=server.serve_forever, daemon=True).start()

        display = next(
            n
            for n in range(300, 400)
            if not Path(f"/tmp/.X{n}-lock").exists() and not Path(f"/tmp/.X11-unix/X{n}").exists()
        )
        authority = base / "Xauthority"
        authority.touch(mode=0o600)
        subprocess.run(["xauth", "-f", str(authority), "add", f":{display}", ".", uuid.uuid4().hex], check=True)
        processes = []
        log = (base / "process.log").open("w+")
        try:
            processes.append(
                subprocess.Popen(
                    ["Xvfb", f":{display}", "-screen", "0", "1440x1000x24", "-nolisten", "tcp", "-auth", str(authority)],
                    stdout=log,
                    stderr=log,
                )
            )
            wait_for(lambda: Path(f"/tmp/.X11-unix/X{display}").exists())
            environment = dict(
                os.environ,
                DISPLAY=f":{display}",
                XAUTHORITY=str(authority),
                XDG_STATE_HOME=str(base / "state"),
                XDG_RUNTIME_DIR=str(base / "runtime"),
                XDG_CONFIG_HOME=str(base / "config"),
                XDG_CACHE_HOME=str(base / "cache"),
            )
            environment.pop("NO_COLOR", None)
            config = base / "config" / "qqtui"
            config.mkdir(parents=True, exist_ok=True)
            (config / "config.toml").write_text("auto_switch_ime = false\n")
            address = "unix:" + str(base / "kitty.sock")
            processes.append(
                subprocess.Popen(
                    [
                        "kitty",
                        "--config", "NONE",
                        "-o", "linux_display_server=x11",
                        "-o", "allow_remote_control=yes",
                        "-o", "font_size=13",
                        "-o", "initial_window_width=116c",
                        "-o", "initial_window_height=34c",
                        "-o", "remember_window_size=no",
                        "--listen-on", address,
                        "--title", "qqtui-screenshot",
                        str(binary),
                    ],
                    env=environment,
                    stdout=log,
                    stderr=log,
                )
            )
            remote = ["kitty", "@", "--to", address]

            def screen():
                return subprocess.run([*remote, "get-text"], capture_output=True, text=True).stdout

            def keys(*values):
                subprocess.run([*remote, "send-key", *values], check=True)

            wait_for(lambda: "选择 QQ 账号" in screen())
            keys("enter")
            wait_for(lambda: "我不敢相信" in screen() and "正在加载图片" not in screen())
            keys("i")
            subprocess.run([*remote, "send-text", "确实离谱，晚上我也跑跑看"], check=True)
            keys("esc")
            keys("k", "k")
            time.sleep(1.0)

            raw = base / "root.png"
            subprocess.run(
                ["ffmpeg", "-loglevel", "error", "-f", "x11grab", "-video_size", "1440x1000", "-i", f":{display}", "-frames:v", "1", "-y", str(raw)],
                check=True,
                env=environment,
            )
            shot = Image.open(raw).convert("RGB")
            gray = shot.convert("L").point(lambda v: 255 if v > 10 else 0)
            box = gray.getbbox()
            if box:
                shot = shot.crop(box)
            shot.save(output)
            print(f"saved {output} {shot.size}")
        finally:
            for process in processes:
                process.terminate()
            for process in processes:
                process.wait(timeout=5)


if __name__ == "__main__":
    main()
