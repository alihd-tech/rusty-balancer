#!/usr/bin/env python3
"""Loopback TCP, PROXY v1 and UDP data-path integration checks."""
import pathlib
import socket
import subprocess
import tempfile
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
def bound(kind):
    s = socket.socket(socket.AF_INET, kind)
    s.bind(("127.0.0.1", 0))
    s.settimeout(5)
    return s

with bound(socket.SOCK_STREAM) as backend, bound(socket.SOCK_STREAM) as unavailable, bound(socket.SOCK_DGRAM) as udp, tempfile.TemporaryDirectory() as directory:
    backend.listen(2)
    # The first target is bound but not listening, exercising pre-forward failover.
    def tcp_echo():
        with backend.accept()[0] as conn:
            conn.settimeout(5)
            stream = conn.makefile("rb")
            line = stream.readline(256)
            assert line.startswith(b"PROXY TCP4 127.0.0.1 127.0.0.1 ")
            conn.sendall(line + stream.read(5))
    def udp_echo():
        frame, addr = udp.recvfrom(20000)
        udp.sendto(frame, addr)
    with bound(socket.SOCK_STREAM) as http, bound(socket.SOCK_STREAM) as tcp, bound(socket.SOCK_DGRAM) as front:
        http_port, tcp_port, udp_port = [s.getsockname()[1] for s in (http,tcp,front)]
    config = pathlib.Path(directory)/"transport.toml"
    config.write_text(f"""listen = "127.0.0.1:{http_port}"
[[routes]]
static_dir = "examples/public"
[[transport.tcp]]
name = "tcp"
listen = "127.0.0.1:{tcp_port}"
proxy_protocol = true
targets = [{{address = "127.0.0.1:{unavailable.getsockname()[1]}"}}, {{address = "127.0.0.1:{backend.getsockname()[1]}"}}]
[[transport.udp]]
name = "udp"
listen = "127.0.0.1:{udp_port}"
allow = ["127.0.0.0/8"]
targets = [{{address = "127.0.0.1:{udp.getsockname()[1]}"}}]
""")
    subprocess.run(["cargo","build","-p","rusty-balancer"], cwd=ROOT, check=True)
    process = subprocess.Popen([str(ROOT/"target/debug/rusty-balancer"),"--config",str(config),"serve"],cwd=ROOT)
    try:
        for _ in range(100):
            if process.poll() is not None:
                raise RuntimeError("server exited")
            try:
                with socket.create_connection(("127.0.0.1",http_port),timeout=0.2):
                    break
            except OSError:
                time.sleep(0.1)
        else:
            raise RuntimeError("startup timeout")
        threading.Thread(target=tcp_echo,daemon=True).start()
        threading.Thread(target=udp_echo,daemon=True).start()
        with socket.create_connection(("127.0.0.1",tcp_port),timeout=5) as client:
            client.sendall(b"hello")
            client.shutdown(socket.SHUT_WR)
            reader = client.makefile("rb")
            line = reader.readline(256)
            assert line.startswith(b"PROXY TCP4 127.0.0.1 127.0.0.1 "), line
            assert reader.read(5) == b"hello"
        with bound(socket.SOCK_DGRAM) as client:
            client.sendto(b"udp echo",("127.0.0.1",udp_port))
            data,addr=client.recvfrom(1024)
            assert data==b"udp echo" and addr[1]==udp_port
        print("TCP/PROXY and UDP smoke checks passed")
    finally:
        process.terminate()
        try:
            process.wait(timeout=35)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
