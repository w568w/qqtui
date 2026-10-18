"""Exercise history navigation in real Kitty against an isolated local fixture.

Requires Kitty, Xvfb, xauth and Python Pillow. All account data and sockets are temporary.
Usage: python3 tests/tui-history.test.py [--binary /path/to/qqtui]
"""

import argparse
import base64
import json
import shlex
import struct
import zlib
import os
from pathlib import Path
import socketserver
import sqlite3
import subprocess
import tempfile
import threading
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
PEER = {"chat_type": 1, "uid": "fixture"}
SECOND_PEER = {"chat_type": 1, "uid": "second"}
GROUP_PEER = {"chat_type": 2, "uid": "group"}


def message(number):
    return dict(
        account="fixture",
        peer=PEER,
        id=str(number),
        seq=str(number),
        time=str(1700000000 + number),
        sender="Fixture",
        sender_uid="fixture",
        text=f"fixture-message-{number}",
        attachments=[],
        reply_to=None,
        is_self=False,
        send_status=2,
    )


class Fixture(socketserver.ThreadingUnixStreamServer):
    daemon_threads = True

    def __init__(self, path):
        self.overrides = {}
        self.sequence = 0
        self.latest = 1200
        self.calls = []
        self.contact_queries = 0
        self.reaction_clicked = False
        self.sticker_path = ""
        self.face_path = ""
        self.forward_image_path = ""
        self.presentation_enabled = False
        self.muted = False
        self.unread_enabled = False
        self.unread_count = 0
        self.avatar_path = ""
        self.second_messages = [dict(message(1),peer=SECOND_PEER,text="other-chat-content")]
        self.clients = []
        self.lock = threading.Lock()
        super().__init__(str(path), Handler)

    def status(self):
        return dict(
            protocol_version=2,
            phase="ready",
            account="fixture",
            epoch="fixture-host",
            seq=self.sequence,
            errors=[],
            capabilities=["history_navigation", "request_context","reactions","stickers"]+(["unread_anchor"] if self.unread_enabled else [])+(["avatars","speech_permissions"] if self.presentation_enabled else []),
        )

    def call(self, method, params):
        self.calls.append((method, params))
        if method in ["host.status", "events.subscribe"]:
            return self.status()
        if method=="accounts.list":return dict(items=[])
        if method=="avatars.resolve":return dict(path=self.avatar_path,version="fixture-avatar")
        if method=="conversations.permissions":return dict(state="blocked" if self.muted else "allowed",scope="all" if self.muted else "none",reason="群已开启全员禁言" if self.muted else "",until=None)
        if method == "conversations.snapshot":
            return dict(
                account="fixture",
                epoch="fixture-host",
                seq=self.sequence,
                conversations=[
                    dict(
                        peer=PEER,
                        id="fixture",
                        name="History fixture",
                        unread="0",
                        pinned=False,
                        last_id=str(self.latest),
                    ),
                    dict(peer=SECOND_PEER,id="second",name="Other fixture",unread="0",pinned=False,last_id=self.second_messages[-1]["id"]),
                    dict(peer=GROUP_PEER,id="group",name="Group fixture",unread=str(self.unread_count),pinned=False,last_id="5002"),
                ],
            )
        if method == "contacts.list":
            self.contact_queries += 1
            if self.contact_queries == 1:
                time.sleep(0.25)
            name = "Stale contact" if self.contact_queries == 1 else "Fresh contact"
            return dict(
                items=[dict(peer=dict(chat_type=1,uid="contact"),id="1:contact",name=name,unread="0",pinned=False)],
            )
        if method == "stickers.list":
            assert params["account"] == "fixture"
            return dict(items=[dict(id="favorite:fixture",kind="sticker",name="Animated favorite",path=self.sticker_path,emoji_id="",emoji_type=""),dict(id="face:14",kind="face",name="微笑",path=self.face_path,emoji_id="14",emoji_type="1")])
        if method == "stickers.resolve":
            assert params["account"] == "fixture" and params["id"]=="favorite:fixture"
            return dict(path=self.sticker_path)
        assert params["account"] == "fixture"
        if params["peer"] == SECOND_PEER:
            if method == "messages.read": return {}
            if method == "messages.send":
                assert params["attachments"][0]["kind"]=="sticker"
                assert params["attachments"][0]["width"]>0
                sent=dict(message(8002),peer=SECOND_PEER,text=params["text"],is_self=True,attachments=[dict(params["attachments"][0],kind="image",element_id="sent-image")])
                self.second_messages.append(sent)
                self.sequence+=1
                event=dict(epoch="fixture-host",account="fixture",seq=self.sequence,kind="messages",messages=[sent])
                for client in self.clients:client.write(dict(jsonrpc="2.0",method="event",params=event))
                return dict(account="fixture",peer=SECOND_PEER,request_id=params["request_id"],native_id="8002",message_id="8002",state="confirmed")
            assert method == "messages.history", method
            return dict(messages=self.second_messages,has_before=False,has_after=False,anchor_found=True)
        if params["peer"] == GROUP_PEER:
            reaction=dict(emoji_id="14",emoji_type="1",count=str(2 if self.reaction_clicked else 1),clicked=self.reaction_clicked,label="微笑")
            original=dict(message(5001),peer=GROUP_PEER,text="group-fixture-original",is_self=True,reactions=[reaction])
            notice=dict(message(5002),peer=GROUP_PEER,text="[表情回应]",reaction_notice='<gtip><qq jp="123" nm="Alice"/><url msgseq="5001"/><face id="14"/></gtip>')
            if method == "conversations.unread":return dict(id="4900")
            if method == "messages.read":
                self.unread_count=0
                return {}
            if method == "reactions.set":
                assert params["epoch"]=="fixture-host" and params["id"]=="5001" and params["emoji_type"]=="1"
                self.reaction_clicked=params["set"]
                return {}
            if method == "reactions.members":return dict(members=[dict(uid="member",name="Alice")],cookie=None)
            if method == "messages.get" or method == "reactions.refresh":return dict(messages=[original])
            if method=="messages.sequence":
                assert params["seq"]=="5001"
                return dict(messages=[original])
            assert method == "messages.history",method
            if params.get("around")=="4900":
                time.sleep(0.5)
                return dict(messages=[dict(message(4900),peer=GROUP_PEER,text="unread-origin-fixture https://example.com/unread-origin"),dict(message(4901),peer=GROUP_PEER,text="newer-than-origin https://example.com/not-origin")],has_before=True,has_after=True,anchor_found=True)
            return dict(messages=[original,notice],has_before=False,has_after=False,anchor_found=True)
        assert params["peer"] == PEER
        if method == "messages.read":
            return {}
        if method == "messages.forward":
            if params["path"] == ["1412","9002"]:
                return dict(messages=[dict(message(9004),text="nested-body-fixture")])
            assert params["path"] == ["1412"]
            return dict(messages=[dict(message(9001),text="forward-body-fixture",forward_path=params["path"],attachments=[dict(element_id="forward-image",kind="image",name="forward.png",path=self.forward_image_path,size="100")]),
                dict(message(9002),text="[合并转发聊天记录]",forward="<msg><title>nested-card-fixture</title></msg>"),
                dict(message(9003),text="long-record\n"+"line\n"*60+"long-record-end")])
        if method == "messages.quote":
            source = self.overrides.get(int(params["id"]), message(int(params["id"])))
            target = source.get("reply_to") or source.get("reply_seq")
            return dict(messages=[message(int(target))] if target else [])
        assert method == "messages.history", method
        limit = params["limit"]
        if params.get("around"):
            start = max(1, int(params["around"]) - (limit - 1) // 2)
            end = min(self.latest, start + limit - 1)
        elif params.get("before"):
            end = int(params["before"]) - 1
            start = max(1, end - limit + 1)
        elif params.get("after"):
            start = int(params["after"]) + 1
            end = min(self.latest, start + limit - 1)
        else:
            end = self.latest
            start = max(1, end - limit + 1)
        return dict(
            messages=[self.overrides.get(i, message(i)) for i in range(start, end + 1)],
            has_before=start > 1,
            has_after=end < self.latest,
            anchor_found=True,
        )

    def notify_state(self,kind):
        with self.lock:
            self.sequence+=1
            event=dict(epoch="fixture-host",account="fixture",seq=self.sequence,kind=kind)
            for client in self.clients:client.write(dict(jsonrpc="2.0",method="event",params=event))

    def notify_new_message(self, number=1201):
        with self.lock:
            self.latest = max(self.latest, number)
            self.sequence += 1
            event = dict(
                epoch="fixture-host",
                account="fixture",
                seq=self.sequence,
                kind="messages",
                messages=[self.overrides.get(number, message(number))],
            )
            for client in self.clients:
                client.write(dict(jsonrpc="2.0", method="event", params=event))

    def notify_group_notice(self):
        notice=dict(message(5002),peer=GROUP_PEER,text="[表情回应]",reaction_notice='<gtip><qq jp="123" nm="Alice"/><url msgseq="5001"/><face id="14"/></gtip>')
        with self.lock:
            self.sequence+=1
            event=dict(epoch="fixture-host",account="fixture",seq=self.sequence,kind="messages",messages=[notice])
            for client in self.clients:client.write(dict(jsonrpc="2.0",method="event",params=event))

    def notify_gap(self):
        with self.lock:
            self.latest += 1
            self.sequence += 2
            event = dict(
                epoch="fixture-host",
                account="fixture",
                seq=self.sequence,
                kind="recent.invalidated",
            )
            for client in self.clients:
                client.write(dict(jsonrpc="2.0", method="event", params=event))


class Handler(socketserver.StreamRequestHandler):
    def write(self, frame):
        self.wfile.write((json.dumps(frame) + "\n").encode())
        self.wfile.flush()

    def handle(self):
        self.server.clients.append(self)
        try:
            for line in self.rfile:
                request = json.loads(line)
                with self.server.lock:
                    try:
                        result = self.server.call(
                            request["method"], request.get("params", {})
                        )
                        self.write(dict(jsonrpc="2.0", id=request["id"], result=result))
                    except Exception as error:
                        self.write(
                            dict(
                                jsonrpc="2.0",
                                id=request["id"],
                                error=dict(code=-32000, message=str(error)),
                            )
                        )
        finally:
            self.server.clients.remove(self)


def seed(base):
    state = base / "state/qqtui"
    state.mkdir(parents=True)
    db = sqlite3.connect(state / "cache.sqlite3")
    db.executescript("""
        CREATE TABLE messages(account TEXT,peer TEXT,id TEXT,time INTEGER,seq INTEGER,
                              text TEXT,data TEXT,PRIMARY KEY(account,peer,id));
        CREATE TABLE snapshots(account TEXT PRIMARY KEY,data TEXT);
        CREATE TABLE settings(key TEXT PRIMARY KEY,data TEXT);
        CREATE TABLE drafts(account TEXT,peer TEXT,data TEXT,PRIMARY KEY(account,peer));
        CREATE TABLE outbox(account TEXT,id TEXT,data TEXT,draft TEXT,PRIMARY KEY(account,id));
    """)
    for i in range(1, 1201):
        msg = message(i)
        db.execute(
            "INSERT INTO messages VALUES(?,?,?,?,?,?,?)",
            (
                "fixture",
                "1:fixture",
                str(i),
                int(msg["time"]),
                i,
                msg["text"],
                json.dumps(msg),
            ),
        )
    snapshot = dict(
        account="fixture",
        epoch="fixture-host",
        seq=0,
        conversations=[
            dict(
                peer=PEER,
                id="fixture",
                name="History fixture",
                unread="0",
                pinned=False,
                last_id="1200",
            )
        ],
    )
    db.execute("INSERT INTO snapshots VALUES(?,?)", ("fixture", json.dumps(snapshot)))
    saved = dict(
        account="fixture",
        selected_peer=PEER,
        views=[
            [PEER, dict(anchor="300", line=1, follow=False), "300"],
            [
                {"chat_type": 1, "uid": "unopened"},
                dict(anchor=None, line=0, follow=True),
                None,
            ],
        ],
    )
    db.execute("INSERT INTO settings VALUES(?,?)", ("ui-state", json.dumps(saved)))
    unopened = dict(
        text="untouched draft 中文",
        cursor=0,
        reply_to=None,
        attachments=[],
        mentions=[],
    )
    db.execute(
        "INSERT INTO drafts VALUES(?,?,?)",
        ("fixture", "1:unopened", json.dumps(unopened)),
    )
    recovery = dict(
        text="fixture recovery draft",
        cursor=0,
        reply_to=None,
        attachments=[],
        mentions=[],
    )
    receipt = dict(
        account="fixture",
        peer=PEER,
        request_id="fixture-failed",
        state="failed",
        error="fixture failure",
    )
    db.execute(
        "INSERT INTO outbox VALUES(?,?,?,?)",
        ("fixture", "fixture-failed", json.dumps(receipt), json.dumps(recovery)),
    )
    db.commit()
    db.close()


def wait_for(predicate):
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError("TUI fixture timed out")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/qqtui")
    binary = parser.parse_args().binary.resolve()
    with tempfile.TemporaryDirectory(
        prefix="qqtui-history-", dir=os.environ["XDG_RUNTIME_DIR"]
    ) as temporary:
        base = Path(temporary)
        seed(base)
        runtime = base / "runtime/qqtui"
        runtime.mkdir(parents=True)
        server = Fixture(runtime / "bridge.sock")
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        display = next(
            n
            for n in range(200, 300)
            if not Path(f"/tmp/.X{n}-lock").exists()
            and not Path(f"/tmp/.X11-unix/X{n}").exists()
        )
        authority = base / "Xauthority"
        authority.touch(mode=0o600)
        subprocess.run(
            [
                "xauth",
                "-f",
                str(authority),
                "add",
                f":{display}",
                ".",
                uuid.uuid4().hex,
            ],
            check=True,
        )
        processes = []
        log = (base / "process.log").open("w+")
        try:
            processes.append(
                subprocess.Popen(
                    [
                        "Xvfb",
                        f":{display}",
                        "-screen",
                        "0",
                        "1280x900x24",
                        "-nolisten",
                        "tcp",
                        "-auth",
                        str(authority),
                    ],
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
            config = base / "config" / "qqtui"
            config.mkdir(parents=True, exist_ok=True)
            (config / "config.toml").write_text("auto_switch_ime = false\n")
            # Kitty placeholders encode image IDs in truecolor foreground channels.
            environment.pop("NO_COLOR", None)
            address = "unix:" + str(base / "kitty.sock")
            processes.append(
                subprocess.Popen(
                    [
                        "kitty",
                        "--config",
                        "NONE",
                        "-o",
                        "linux_display_server=x11",
                        "-o",
                        "allow_remote_control=yes",
                        "-o",
                        "initial_window_width=120c",
                        "-o",
                        "initial_window_height=40c",
                        "--listen-on",
                        address,
                        "--title",
                        "qqtui-history-fixture",
                        str(binary),
                    ],
                    env=environment,
                    stdout=log,
                    stderr=log,
                )
            )
            remote = ["kitty", "@", "--to", address]

            def screen():
                return subprocess.run(
                    [*remote, "get-text"], capture_output=True, text=True
                ).stdout

            def keys(*values):
                subprocess.run([*remote, "send-key", *values], check=True)

            def mouse(button, x, y, release=False):
                suffix = "m" if release else "M"
                subprocess.run([*remote,"send-text","--stdin","--bracketed-paste=disable"],
                    input=f"\x1b[<{button};{x+1};{y+1}{suffix}",text=True,check=True)

            wait_for(lambda:"选择 QQ 账号" in screen())
            keys("enter")
            # Later reconnect/cache checks deliberately reopen the chosen slot directly.
            processes[-1].args.extend(["--account","default"])
            wait_for(
                lambda: "历史位置" in screen() and "fixture-message-300" in screen()
            )
            server.notify_new_message()
            server.notify_new_message()
            wait_for(lambda: "1 条新消息" in screen())
            assert "fixture-message-1201" not in screen()
            for number in range(1202, 1402):
                server.notify_new_message(number)
            wait_for(lambda: "201 条新消息" in screen())
            before = sum(
                m == "messages.history" and p.get("around") == "300"
                for m, p in server.calls
            )
            server.notify_gap()
            wait_for(
                lambda: sum(
                    m == "messages.history" and p.get("around") == "300"
                    for m, p in server.calls
                )
                > before
            )
            keys("c")
            wait_for(lambda: server.contact_queries == 1)
            keys("escape")
            time.sleep(0.35)
            assert "Stale contact" not in screen()
            keys("c")
            wait_for(lambda: "Fresh contact" in screen())
            keys("escape")
            keys("l")
            for _ in range(3):
                keys("ctrl+d")
                time.sleep(0.12)
            wait_for(
                lambda: any(
                    p.get("after") == "320"
                    for m, p in server.calls
                    if m == "messages.history"
                )
            )
            keys("g", "g")
            wait_for(
                lambda: any(
                    p.get("before") == "281"
                    for m, p in server.calls
                    if m == "messages.history"
                )
            )
            assert not any(m == "messages.read" for m, _ in server.calls)
            keys("shift+g")
            wait_for(lambda: f"fixture-message-{server.latest}" in screen())
            wait_for(lambda: any(m == "messages.read" for m, _ in server.calls))
            keys("slash")
            subprocess.run(
                [*remote, "send-text", "--stdin", "--bracketed-paste=enable"],
                input="fixture-message-100",
                text=True,
                check=True,
            )
            wait_for(lambda: "Fixture：fixture-message-100" in screen())
            keys("enter")
            wait_for(
                lambda: any(
                    p.get("around") == "100"
                    for m, p in server.calls
                    if m == "messages.history"
                )
            )
            wait_for(
                lambda: "历史位置" in screen() and "fixture-message-100" in screen()
            )
            keys("ctrl+o")
            wait_for(lambda: "发送记录" in screen())
            keys("enter")
            wait_for(
                lambda: "已恢复草稿" in screen()
                and "fixture recovery draft" in screen()
            )
            assert sum(m == "messages.read" for m, _ in server.calls) == 1
            keys("escape", "h", "j")
            time.sleep(0.2)
            assert "fixture-message-100" in screen() and "other-chat-content" not in screen()
            assert not any(m=="messages.history" and p.get("peer")==SECOND_PEER for m,p in server.calls)
            keys("enter")
            wait_for(lambda: "other-chat-content" in screen())
            # Clicking the first actual sidebar row activates the original conversation.
            mouse(0, 5, 2)
            mouse(0, 5, 2, release=True)
            wait_for(lambda: "fixture-message-100" in screen() and "other-chat-content" not in screen())
            print("Kitty: sidebar cursor waits for Enter; mouse click activates the chosen conversation",flush=True)
            image = base / "fixture.png"
            def chunk(kind, data):
                return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
            image.write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 320, 160, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress((b"\0" + b"\x10\x80\xe0" * 320) * 160)) + chunk(b"IEND", b""))
            launcher_dir = base / "bin"
            launcher_dir.mkdir()
            opened = base / "opened.txt"
            opener = launcher_dir / "xdg-open"
            opener.write_text('#!/bin/sh\nprintf "%s\\n" "$1" >> ' + shlex.quote(str(opened)) + '\n')
            opener.chmod(0o755)
            # The TUI inherits PATH at startup, so provide the recorder before restarting it.
            keys("escape", "q")
            processes[-1].wait(timeout=8)
            environment["PATH"] = str(launcher_dir) + os.pathsep + environment["PATH"]
            server.overrides[1403] = dict(message(1403), attachments=[dict(element_id="1",kind="image",name="fixture.png",path=str(image),size=str(image.stat().st_size)),dict(element_id="extra",kind="image",name="fixture-second.png",path=str(image),size=str(image.stat().st_size))])
            server.latest = 1403
            processes.append(subprocess.Popen(processes[-1].args, env=environment, stdout=log, stderr=log))
            wait_for(lambda: "历史位置" in screen() and "fixture-message-100" in screen())
            keys("escape", "l", "shift+g")
            wait_for(lambda: chr(0x10EEEE) in screen())
            assert "[image]" not in screen() and "fixture.png" not in screen()
            assert "╭" in screen() and "╯" in screen()
            inline_cells = screen().count(chr(0x10EEEE))
            keys("v")
            wait_for(lambda: "Esc 关闭" in screen() and screen().count(chr(0x10EEEE)) > inline_cells)
            normal_cells=screen().count(chr(0x10EEEE))
            from PIL import ImageGrab
            os.environ["XAUTHORITY"] = str(authority)
            def preview_colors():
                shot=ImageGrab.grab(xdisplay=f":{display}").crop((360,230,720,570)).convert("RGB")
                counts=shot.getcolors(shot.width*shot.height)
                colors=set()
                for count,(r,g,b) in counts:
                    if count>100 and r>200 and g<30 and b<30:colors.add("red")
                    if count>100 and g>200 and r<30 and b<30:colors.add("green")
                    if count>100 and b>180 and 70<g<180 and r<50:colors.add("blue")
                return colors
            wait_for(lambda:"blue" in preview_colors())
            for step in range(18):
                if step%3==0:server.notify_new_message(1403)
                time.sleep(0.1)
                assert "blue" in preview_colors(),"Static preview went black during a native update"

            keys("plus")
            wait_for(lambda: "大窗" in screen() and screen().count(chr(0x10EEEE))>normal_cells)
            keys("z")
            wait_for(lambda: "全屏" in screen())
            keys("0")
            wait_for(lambda: "默认" in screen())
            for width,height in [(110,36),(120,40)]:
                subprocess.run([*remote,"resize-os-window","--width",str(width),"--height",str(height),"--unit","cells"],check=True)
                wait_for(lambda: len(screen().splitlines())==height and "Esc 关闭" in screen() and chr(0x10EEEE) in screen())
            keys("right")
            wait_for(lambda: "图片 2/2" in screen())
            keys("left")
            wait_for(lambda: "图片 1/2" in screen())
            wait_for(lambda:"blue" in preview_colors())
            keys("escape","v")
            wait_for(lambda:"Esc 关闭" in screen() and "blue" in preview_colors())
            for _ in range(10):
                time.sleep(.1)
                assert "blue" in preview_colors(),"Reopened static viewer lost its image data"

            keys("o")
            wait_for(lambda: opened.exists() and str(image) in opened.read_text())
            keys("escape")
            wait_for(lambda: "Esc 关闭" not in screen())
            link = "https://example.com/qqtui-test"
            server.overrides[1404] = dict(message(1404), text=link)
            server.notify_new_message(1404)
            keys("shift+g")
            wait_for(lambda: link in screen())
            keys("o")
            wait_for(lambda: link in opened.read_text())
            attachment_file = base / "fixture.txt"
            attachment_file.write_text("fixture file")
            server.overrides[1405] = dict(message(1405), attachments=[dict(element_id="2",kind="file",name="fixture.txt",path=str(attachment_file),size="12")])
            server.notify_new_message(1405)
            keys("shift+g")
            wait_for(lambda: "fixture.txt" in screen())
            keys("o")
            wait_for(lambda: str(attachment_file) in opened.read_text())
            multi_links=["https://example.org/first","https://example.org/second"]
            server.overrides[1405]["text"]=" ".join(multi_links)
            server.notify_new_message(1405)
            wait_for(lambda:multi_links[1] in screen())
            keys("o")
            wait_for(lambda:"打开对象" in screen() and multi_links[1] in screen() and "fixture.txt" in screen())
            assert multi_links[1] not in opened.read_text()
            keys("2")
            wait_for(lambda:multi_links[1] in opened.read_text())
            print("Kitty: o lists mixed links and files, numeric selection opens only the chosen object",flush=True)
            server.overrides[1406] = dict(message(1406), text="[微笑]", parts=[dict(type="attachment",element_id="3")], attachments=[dict(element_id="3",kind="emoji",name="微笑",path=str(image),size="0")])
            server.notify_new_message(1406)
            keys("shift+g")
            wait_for(lambda: "[微笑]" not in screen() and chr(0x10EEEE) in screen().rsplit("╭",1)[-1].split("╯",1)[0])
            assert "[emoji]" not in screen() and "[image]" not in screen()
            server.overrides[1407] = dict(message(1407),text="reply-to-older",reply_to="50",reply_seq="50")
            server.notify_new_message(1407)
            keys("shift+g")
            wait_for(lambda: "reply-to-older" in screen() and "Fixture：fixture-message-50" in screen())
            assert any(m=="messages.quote" and p["id"]=="1407" for m,p in server.calls)
            keys("g","r")
            wait_for(lambda: "fixture-message-50" in screen() and "reply-to-older" not in screen())
            assert any(m=="messages.history" and p.get("around")=="50" for m,p in server.calls)
            keys("ctrl+t")
            wait_for(lambda: "reply-to-older" in screen())
            # A click selects the message under the pointer, even when another panel has focus.
            keys("h")
            row = next(i for i,line in enumerate(screen().splitlines()) if "reply-to-older" in line)
            mouse(0, 45, row)
            mouse(0, 45, row, release=True)
            keys("g","r")
            wait_for(lambda: "fixture-message-50" in screen() and "reply-to-older" not in screen())
            keys("ctrl+t")
            wait_for(lambda: "reply-to-older" in screen())
            before_pages = {p.get("before") for m,p in server.calls if m=="messages.history" and p.get("before")}
            for _ in range(150):
                mouse(64, 50, 10)
                time.sleep(0.025)
            after_pages = {p.get("before") for m,p in server.calls if m=="messages.history" and p.get("before")}
            assert len(after_pages-before_pages) >= 3, (before_pages,after_pages)
            keys("h")
            server.notify_new_message(1408)
            wait_for(lambda: "条新消息" in screen())
            assert "G 回到底部" not in screen()
            keys("l", "shift+g")
            wait_for(lambda: "fixture-message-1408" in screen())
            # Leave follow mode, then reach the bottom with the wheel, without G.
            for _ in range(4): mouse(64, 50, 10)
            time.sleep(0.1)
            for _ in range(6): mouse(65, 50, 10)
            time.sleep(0.2)
            reads = sum(m=="messages.read" for m,_ in server.calls)
            server.notify_new_message(1409)
            wait_for(lambda: "fixture-message-1409" in screen() and "条新消息" not in screen())
            wait_for(lambda: sum(m=="messages.read" for m,_ in server.calls)>reads)
            print("Kitty: message clicks, three consecutive wheel history pages, focus-specific hints, automatic follow and read passed",flush=True)
            animated = base / "animated-sticker.jpg"
            animated.write_bytes(base64.b64decode("R0lGODlhQABAAIEAAP8AAAAAAAAAAAAAACH/C05FVFNDQVBFMi4wAwEAAAAh+QQAHgAAACwAAAAAQABAAAAIaQABCBxIsKDBgwgTKlzIsKHDhxAjSpxIsaLFixgzatzIsaPHjyBDihxJsqTJkyhTqlzJsqXLlzBjypxJs6bNmzhz6tzJs6fPn0CDCh1KtKjRo0iTKl3KtKnTp1CjSp1KtarVq1izagUQEAAh+QQBHgABACwAAAAAQABAAIEA/wAAAAAAAAAAAAAIaQABCBxIsKDBgwgTKlzIsKHDhxAjSpxIsaLFixgzatzIsaPHjyBDihxJsqTJkyhTqlzJsqXLlzBjypxJs6bNmzhz6tzJs6fPn0CDCh1KtKjRo0iTKl3KtKnTp1CjSp1KtarVq1izagUQEAA7"))
            server.overrides[1410] = dict(message(1410),text="animated-sticker",attachments=[dict(element_id="animation",kind="image",name="sticker.jpg",path=str(animated),size=str(animated.stat().st_size))])
            server.notify_new_message(1410)
            wait_for(lambda: "animated-sticker" in screen() and chr(0x10EEEE) in screen())
            from PIL import ImageGrab
            os.environ["XAUTHORITY"] = str(authority)
            def animation_plays():
                colors=set()
                for _ in range(80):
                    shot=ImageGrab.grab(xdisplay=f":{display}")
                    for count,(r,g,b) in shot.convert("RGB").getcolors(shot.width*shot.height):
                        if count>100 and r>200 and g<30 and b<30: colors.add("red")
                        if count>100 and g>200 and r<30 and b<30: colors.add("green")
                    if len(colors)==2: return True
                    time.sleep(0.1)
                shot.save(Path(os.environ["XDG_RUNTIME_DIR"])/"qqtui-animation-check.png")
                print("Animation screenshot colors:",colors, sorted(shot.convert("RGB").getcolors(shot.width*shot.height),reverse=True)[:10],flush=True)
                return False
            assert animation_plays(), "Inline GIF did not display both frames in Kitty"
            keys("v")
            wait_for(lambda: "Esc 关闭" in screen())
            assert animation_plays(), "Preview GIF did not display both frames in Kitty"
            late_colors=set();black=0
            for step in range(50):
                if step%10==0:server.notify_new_message(1410)
                time.sleep(0.13)
                colors=preview_colors() & {"red","green"}
                if step>=25:late_colors.update(colors)
                black=0 if colors else black+1
                assert black<4,"Animation stayed black after replacing a displayed frame"
            assert late_colors=={"red","green"},"Animation stopped after its initial frames"
            keys("escape","v")
            wait_for(lambda:"Esc 关闭" in screen())
            assert animation_plays(),"Reopening a cached animation lost its terminal image"

            keys("escape")
            print("Kitty: screenshots confirm both GIF frames play inline and in the enlarged viewer",flush=True)
            server.overrides[1411] = dict(message(1411),text="text-before[/大冤种]text-after",parts=[dict(type="text",text="text-before"),dict(type="attachment",element_id="inline-face"),dict(type="text",text="text-after")],attachments=[dict(element_id="inline-face",kind="emoji",name="大冤种",path=str(image),size="0")])
            server.notify_new_message(1411)
            wait_for(lambda: any("text-before" in line and "text-after" in line and chr(0x10EEEE) in line for line in screen().splitlines()))
            assert "[/大冤种]" not in screen() and "[大冤种]" not in screen()
            server.forward_image_path=str(image)
            server.overrides[1412] = dict(message(1412),text="[合并转发聊天记录]",forward="<msg><item><title>forward-card-fixture</title><title>A: &amp; preview</title></item><summary>3 records</summary></msg>")
            server.notify_new_message(1412)
            wait_for(lambda: "forward-card-fixture" in screen() and "A: & preview" in screen())
            assert "[消息类型 16]" not in screen() and "<title>" not in screen()
            keys("enter")
            wait_for(lambda: "forward-body-fixture" in screen() and chr(0x10EEEE) in screen())
            keys("v")
            wait_for(lambda:"Esc 关闭" in screen() and "blue" in preview_colors())
            keys("escape")
            wait_for(lambda:"forward-body-fixture" in screen())
            keys("j","enter")
            wait_for(lambda: "nested-body-fixture" in screen())
            keys("escape")
            wait_for(lambda: "forward-body-fixture" in screen())
            keys("shift+g",*(["ctrl+d"]*10))
            wait_for(lambda: "long-record-end" in screen())
            keys("escape")
            wait_for(lambda: "forward-card-fixture" in screen() and "long-record-end" not in screen())
            print("Kitty: one inline emoji without fallback labels/newlines; forward XML card, native expansion, nested records, long-record scrolling and return passed",flush=True)
            print("Kitty: centered zoom viewer, viewer resize/redraw, attachment switching, older quote fetching, gr jump and Ctrl+T return passed",flush=True)
            print("Kitty: inline image, v preview, Esc cleanup, o image/file/link, QQ faces without filename labels and selected-message border passed", flush=True)
            server.sticker_path=str(animated)
            server.face_path=str(image)
            def choose_chat(name):
                keys("escape","ctrl+p")
                subprocess.run([*remote,"send-text","--stdin","--bracketed-paste=enable"],input=name,text=True,check=True)
                keys("enter")
            choose_chat("Group fixture")
            wait_for(lambda:"group-fixture-original" in screen())
            assert "[系统消息]" not in screen()
            wait_for(lambda:"Alice" in screen() and "表情回应" in screen())
            server.notify_group_notice()
            wait_for(lambda:any(m=="reactions.refresh" for m,_ in server.calls))
            keys("shift+g","g","r")
            wait_for(lambda:any(m=="messages.sequence" and p.get("seq")=="5001" for m,p in server.calls))
            wait_for(lambda:any(m=="messages.history" and p.get("around")=="5001" for m,p in server.calls))
            keys("ctrl+t")
            wait_for(lambda:"Alice" in screen())
            keys("g","g","shift+r")
            wait_for(lambda:"Enter 成员" in screen())
            keys("enter")
            wait_for(lambda:"Alice (member)" in screen())
            keys("escape")
            wait_for(lambda:"Enter 成员" in screen())
            keys("space")
            wait_for(lambda:server.reaction_clicked and "我已回应" in screen())
            keys("space")
            wait_for(lambda:not server.reaction_clicked and "Enter 成员" in screen())
            keys("a")
            wait_for(lambda:"选择回应" in screen() and "微笑" in screen())
            keys("enter")
            wait_for(lambda:server.reaction_clicked and "我已回应" in screen())
            choose_chat("Other fixture")
            wait_for(lambda:"other-chat-content" in screen())
            keys("ctrl+e")
            wait_for(lambda:"Animated favorite" in screen() and "表情包" in screen())
            keys("enter")
            wait_for(lambda:"Animated favorite" in screen() and "INSERT" in screen())
            assert not any(m=="messages.send" for m,_ in server.calls),"Picking a sticker sent it before the send key"
            keys("alt+enter", "ctrl+enter")
            time.sleep(.2)
            assert not any(m=="messages.send" for m,_ in server.calls),"Newline or removed Ctrl+Enter binding sent the draft"
            keys("enter")
            wait_for(lambda:any(m=="messages.send" for m,_ in server.calls))
            assert next(p for m,p in server.calls if m=="messages.send")["text"]=="\n", "Alt+Enter did not insert a newline"
            def sticker_confirmed():
                with sqlite3.connect(base/"state/qqtui/cache.sqlite3") as saved:
                    return any((value:=json.loads(row[0]))["state"]=="confirmed" and value["peer"]==SECOND_PEER for row in saved.execute("SELECT data FROM outbox WHERE account='fixture'"))
            wait_for(sticker_confirmed)
            wait_for(lambda: "提交中" not in screen())
            choose_chat("History fixture")
            wait_for(lambda:"fixture recovery draft" in screen())
            print("Kitty: native reaction counts, friendly notifications, member list, toggle/add, sticker gallery preview and explicit sticker send passed",flush=True)
            server.presentation_enabled=True;server.avatar_path=str(image);server.notify_state("status")
            wait_for(lambda:any(m=="avatars.resolve" for m,_ in server.calls))
            wait_for(lambda:any(chr(0x10EEEE) in line[27:35] for line in screen().splitlines()))
            server.muted=True
            choose_chat("Group fixture")
            wait_for(lambda:"群已开启全员禁言" in screen() and "READONLY" in screen())
            before=len([m for m,_ in server.calls if m=="messages.send"])
            keys("i");subprocess.run([*remote,"send-text","--stdin","--bracketed-paste=enable"],input="blocked-composer-text",text=True,check=True);keys("enter")
            time.sleep(.2);assert "blocked-composer-text" not in screen();assert len([m for m,_ in server.calls if m=="messages.send"])==before
            server.muted=False;server.notify_state("permissions.invalidated")
            wait_for(lambda:"READONLY" not in screen())
            choose_chat("History fixture");wait_for(lambda:"fixture recovery draft" in screen())
            print("Kitty: native avatar gutter, group mute composer protection, unchanged drafts and live unmute passed",flush=True)
            server.unread_enabled=True;server.unread_count=1000;server.notify_state("status")
            choose_chat("Group fixture")
            wait_for(lambda:any(m=="conversations.unread" for m,_ in server.calls))
            keys("shift+g")
            wait_for(lambda:server.unread_count==0)
            server.notify_state("recent.invalidated")
            keys("g","u")
            wait_for(lambda:"unread-origin-fixture" in screen())
            keys("o")
            wait_for(lambda:"https://example.com/unread-origin" in opened.read_text())
            keys("ctrl+t")
            wait_for(lambda:"group-fixture-original" in screen())
            choose_chat("History fixture");keys("escape","shift+g")
            card_id=server.latest+1
            body="card-title-fixture\ncard-description-fixture\nhttps://example.com/card"
            server.overrides[card_id]=dict(message(card_id),text=body,parts=[dict(type="text",text=body)],card_parts=[0])
            server.notify_new_message(card_id)
            wait_for(lambda:"card-title-fixture" in screen() and "┌ 卡片" in screen() and "https://example.com/card" in screen())
            print("Kitty: card frames and gu jump after unread count clears, with Ctrl+T return, passed",flush=True)
            keys("escape", "q")
            processes[-1].wait(timeout=8)
            server.shutdown()
            server.server_close()
            thread.join()
            # Restart without a backend; cached outbox entries and unopened drafts remain available.
            processes.append(
                subprocess.Popen(
                    processes[-1].args, env=environment, stdout=log, stderr=log
                )
            )
            wait_for(
                lambda: "离线缓存" in screen() and "fixture recovery draft" in screen()
            )
            keys("ctrl+o")
            wait_for(
                lambda: "发送记录" in screen() and "fixture recovery draft" in screen()
            )
            keys("escape", "q")
            processes[-1].wait(timeout=8)
            db = sqlite3.connect(base / "state/qqtui/cache.sqlite3")
            unopened = json.loads(
                db.execute(
                    "SELECT data FROM drafts WHERE account='fixture' AND peer='1:unopened'"
                ).fetchone()[0]
            )
            assert unopened["text"] == "untouched draft 中文"
            restored = json.loads(
                db.execute(
                    "SELECT data FROM drafts WHERE account='fixture' AND peer='1:fixture'"
                ).fetchone()[0]
            )
            assert restored["text"] == "fixture recovery draft"
            db.close()
            sends=[p for m,p in server.calls if m=="messages.send"]
            assert len(sends)==1 and sends[0]["peer"]==SECOND_PEER and sends[0]["attachments"][0]["kind"]=="sticker"
            print(
                "Kitty: anchor recovery, both history directions, deferred live messages, G, search, read gating, gaps, event bursts, cancelled dialogs, offline outbox and unopened drafts passed"
            )
        except BaseException:
            print("fixture RPC calls:", server.calls[-12:])
            print("fixture screen:", screen())
            log.seek(0)
            print(log.read()[-2000:])
            raise
        finally:
            for process in reversed(processes):
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
            server.shutdown()
            server.server_close()
            thread.join()
            log.close()


if __name__ == "__main__":
    main()
