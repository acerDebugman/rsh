#!/usr/bin/env python3
"""Minimal IPv4-only HTTP CONNECT proxy.

Some machines resolve public hosts to IPv6 first but have no working IPv6
route, which makes cargo hang while fetching the registry. This proxy accepts
CONNECT requests and dials upstream with AF_INET, giving cargo a working path.

Usage: python3 scripts/ipv4_connect_proxy.py [port]
"""

import select
import socket
import sys
import threading


def pipe(a: socket.socket, b: socket.socket) -> None:
    try:
        while True:
            readable, _, _ = select.select([a, b], [], [], 300)
            if not readable:
                return
            for sock in readable:
                data = sock.recv(65536)
                if not data:
                    return
                (b if sock is a else a).sendall(data)
    except OSError:
        pass
    finally:
        for sock in (a, b):
            try:
                sock.close()
            except OSError:
                pass


def handle(conn: socket.socket) -> None:
    try:
        buf = b""
        while b"\r\n\r\n" not in buf:
            chunk = conn.recv(4096)
            if not chunk:
                return
            buf += chunk
        request_line = buf.split(b"\r\n", 1)[0].decode("latin1")
        method, target, _ = request_line.split(" ")
        if method.upper() != "CONNECT":
            conn.sendall(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n")
            return
        host, port = target.rsplit(":", 1)
        infos = socket.getaddrinfo(host, int(port), socket.AF_INET, socket.SOCK_STREAM)
        upstream = socket.create_connection(infos[0][4], timeout=15)
        conn.sendall(b"HTTP/1.1 200 Connection established\r\n\r\n")
        pipe(conn, upstream)
    finally:
        try:
            conn.close()
        except OSError:
            pass


def main() -> None:
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18080
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", port))
    server.listen(64)
    print(f"ipv4 connect proxy listening on 127.0.0.1:{port}", flush=True)
    while True:
        conn, _ = server.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    main()
