import json, os, pathlib, signal, socket, subprocess, time, urllib.request, urllib.parse, re

entry = "/usr/bin/qqtui"
home = pathlib.Path(os.environ["HOME"])
home.mkdir(parents=True, exist_ok=True)
# The fixture chooses its mounted QQ installation explicitly; the product does not assume /opt.
config = home / ".config/qqtui"
config.mkdir(parents=True)
(config / "config.toml").write_text('qq_path = "/opt/QQ/qq"\n')
log = open("/tmp/supervisor.log", "w+")
os.environ["DBUS_SESSION_BUS_ADDRESS"] = "unix:path=/tmp/desktop-bus-unreachable"
os.environ["WAYLAND_DISPLAY"] = "desktop-wayland"
host = subprocess.Popen([entry, "host"], stdout=log, stderr=log)
viewer = None
second = None
try:
    status = None
    for _ in range(35):
        if host.poll() is not None:
            break
        p = subprocess.run([entry, "--account", "default", "status"], capture_output=True, text=True)
        if p.returncode == 0:
            status = json.loads(p.stdout)
            if status["phase"] in ["login", "ready", "incompatible"]:
                break
        time.sleep(0.5)
    if not status:
        raise RuntimeError("No clean-user bridge")
    print("clean-user phase:", status["phase"], "errors:", status["errors"], flush=True)
    assert status["phase"] == "login", status
    qq_envs = []
    for proc in pathlib.Path("/proc").iterdir():
        if not proc.name.isdigit():
            continue
        try:
            if (proc / "exe").resolve() != pathlib.Path("/opt/QQ/qq"):
                continue
            env = dict(
                part.split(b"=", 1)
                for part in (proc / "environ").read_bytes().split(b"\0")
                if b"=" in part
            )
            if b"QQTUI_SOCKET" in env:
                qq_envs.append(env)
        except OSError:
            pass
    assert qq_envs, "No managed QQ process"
    for env in qq_envs:
        assert env[b"DBUS_SESSION_BUS_ADDRESS"].startswith(b"unix:")
        assert (
            env[b"DBUS_SESSION_BUS_ADDRESS"]
            != os.environ["DBUS_SESSION_BUS_ADDRESS"].encode()
        )
        assert b"WAYLAND_DISPLAY" not in env
        assert env[b"XDG_RUNTIME_DIR"].endswith(b"/qqtui/qq-session")
    print("QQ private D-Bus and display environment: passed", flush=True)
    other = config / "accounts/123456789"
    other.mkdir(parents=True)
    (other / "config.toml").write_text('qq_path = "/opt/QQ/qq"\n')
    second = subprocess.Popen(
        [entry, "--account", "123456789", "host"], stdout=log, stderr=log
    )
    second_status = None
    for _ in range(40):
        if second.poll() is not None:
            break
        probe = subprocess.run(
            [entry, "--account", "123456789", "status"], capture_output=True, text=True
        )
        if probe.returncode == 0:
            second_status = json.loads(probe.stdout)
            if second_status["phase"] in ["login", "ready", "incompatible"]:
                break
        time.sleep(0.25)
    assert second_status and second_status["phase"] == "login", second_status
    assert host.poll() is None, "Starting a second account stopped the original host"
    first_runtime = pathlib.Path("/run/user/1000/qqtui")
    second_runtime = first_runtime / "accounts/123456789"
    assert (
        first_runtime.joinpath("display").read_text()
        != second_runtime.joinpath("display").read_text()
    )
    assert second_runtime.joinpath("bridge.sock").is_socket()
    profile = home / ".local/state/qqtui/accounts/123456789/qq-profile"
    assert profile.is_dir()
    assert not any(profile.glob("nt_qq_*/nt_db/*")), "Copied native account database"
    print(
        "Two concurrent QQ login hosts: independent sockets, displays, browser/login metadata; original data preserved",
        flush=True,
    )
    second.terminate()
    second.wait(timeout=18)
    assert host.poll() is None, "Stopping the second account stopped the original"

    assert (pathlib.Path("/run/user/1000/qqtui/Xauthority")).is_file()
    viewer = subprocess.Popen(
        [entry, "--account", "default", "backend", "view", "--no-open"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    line = viewer.stdout.readline().strip()
    url = re.search(r"http://\S+", line).group()
    with urllib.request.urlopen(url, timeout=5) as response:
        assert b"noVNC" in response.read()
    print("clean-user noVNC HTTP: 200", flush=True)
    parts = urllib.parse.urlsplit(url)
    query = urllib.parse.parse_qs(parts.query)
    time.sleep(0.3)

    def handshake():
        with socket.create_connection(
            (parts.hostname, parts.port), timeout=5
        ) as remote:
            request = (
                f"GET /{query['path'][0]} HTTP/1.1\r\nHost: {parts.netloc}\r\n"
                "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
                "Sec-WebSocket-Protocol: binary\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            )
            remote.sendall(request.encode())
            data = b""
            while b"RFB 003." not in data:
                chunk = remote.recv(4096)
                assert chunk, "VNC closed before handshake"
                data += chunk
                assert len(data) < 16384, "VNC handshake failed"
            assert b"101 Switching Protocols" in data

    handshake()
    time.sleep(0.3)
    assert viewer.poll() is None, "Viewer exited on browser refresh"
    handshake()
    assert viewer.poll() is None
    print("packaged WebSocket/VNC handshake and browser reconnect: passed", flush=True)
    viewer.communicate(timeout=13)
    assert not pathlib.Path("/run/user/1000/qqtui/viewer.pass").exists()
    print("viewer temporary credential cleanup: passed", flush=True)
    journal = home / ".local/state/qqtui/send-journal.sqlite3"
    import sqlite3

    with sqlite3.connect(journal.as_uri() + "?mode=ro", uri=True) as db:
        assert db.execute("SELECT value FROM meta WHERE key='legacy'").fetchone() == (
            "1",
        )
    print(
        "QQ embedded SQLite worker capability and journal initialization: passed",
        flush=True,
    )
    # A frontend can disappear while its response is being written.
    for _ in range(8):
        with socket.socket(socket.AF_UNIX) as client:
            client.connect(str(first_runtime / "bridge.sock"))
            client.sendall(
                b'{"jsonrpc":"2.0","id":1,"method":"host.status","params":{}}\n' * 32
            )
        time.sleep(0.1)
    assert host.poll() is None, "Cancelled read-only clients killed QQ"
    subprocess.run([entry, "--account", "default", "status"], check=True, stdout=subprocess.DEVNULL)
    print("Cancelled RPC clients preserve host health: passed", flush=True)
    for component in ["Xvfb", "dbus-daemon", "qq"]:
        children = (
            pathlib.Path(f"/proc/{host.pid}/task/{host.pid}/children")
            .read_text()
            .split()
        )
        victim = next(
            int(pid)
            for pid in children
            if any(
                pathlib.Path(arg.decode(errors="replace")).name == component
                for arg in pathlib.Path(f"/proc/{pid}/cmdline")
                .read_bytes()
                .split(b"\0")
                if arg
            )
        )
        os.kill(victim, signal.SIGKILL)
        host.wait(timeout=18)
        assert host.returncode != 0, (component, host.returncode)
        assert not list(pathlib.Path("/tmp/.X11-unix").glob("X*"))
        print(
            "Supervised component failure and cleanup:", component, "passed", flush=True
        )
        if component != "qq":
            host = subprocess.Popen([entry, "host"], stdout=log, stderr=log)
            for _ in range(40):
                if host.poll() is not None:
                    raise RuntimeError("Host failed to restart after " + component)
                result = subprocess.run(
                    [entry, "--account", "default", "status"], capture_output=True, text=True
                )
                if (
                    result.returncode == 0
                    and json.loads(result.stdout)["phase"] == "login"
                ):
                    break
                time.sleep(0.25)
            else:
                raise RuntimeError("Restarted host did not initialize")

finally:
    if second and second.poll() is None:
        second.terminate()
        second.wait(timeout=18)
    if viewer and viewer.poll() is None:
        viewer.kill()
        viewer.wait()
    host.terminate()
    try:
        host.wait(timeout=18)
    except subprocess.TimeoutExpired:
        host.kill()
        host.wait()
    log.seek(0)
    text = log.read()
    print(
        "host shutdown:",
        host.returncode,
        "X sockets:",
        list(pathlib.Path("/tmp/.X11-unix").glob("X*")),
        flush=True,
    )
    if not locals().get("status"):
        print(text[-2000:])
        qlog = home / ".local/state/qqtui/host.log"
        print(qlog.read_text()[-3000:] if qlog.exists() else "No QQ log")
    assert not list(pathlib.Path("/tmp/.X11-unix").glob("X*"))
