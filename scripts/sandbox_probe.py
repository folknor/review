#!/usr/bin/env python3
"""Witness program run *inside* a codex sandbox by `readonly_network_check.py`.

Prints one `NAME: ok` / `NAME: EPERM ...` line per syscall class, so the driver
can tell a seccomp denial (EPERM on the socket call) apart from a filesystem
denial (EROFS/EACCES on a write) apart from a genuine absence of network.

Takes the path of an AF_UNIX socket the driver is already listening on: under
`read-only` nothing is writable, so `bind()` cannot succeed on filesystem
grounds no matter what the seccomp filter allows. `connect()` to an existing
socket is the witness that isolates the filter.
"""

import os
import socket
import sys


def report(name, fn):
    try:
        fn()
        print(f"{name}: ok")
    except OSError as exc:
        print(f"{name}: {errno_name(exc)} {exc.strerror}")
    except Exception as exc:  # noqa: BLE001 - probe reports whatever it hits
        print(f"{name}: {type(exc).__name__} {exc}")


def errno_name(exc):
    import errno as e
    return {v: k for k, v in vars(e).items() if isinstance(v, int)}.get(
        exc.errno, str(exc.errno))


def unix_connect(path):
    def go():
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            s.connect(path)
            s.sendall(b"ping")
        finally:
            s.close()
    return go


def inet_socket():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.close()


def tcp_connect():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.settimeout(10)
    try:
        s.connect(("1.1.1.1", 443))
    finally:
        s.close()


def sockopt():
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.getsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR)
    finally:
        s.close()


def loopback_bind():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        s.bind(("127.0.0.1", 0))
        s.listen(1)
        s.getsockname()
    finally:
        s.close()


def write_cwd():
    p = os.path.join(os.getcwd(), "probe-write-witness")
    with open(p, "w") as fh:
        fh.write("x")
    os.unlink(p)


def read_outside():
    with open("/etc/hostname") as fh:
        fh.read()


def main():
    unix_path = sys.argv[1] if len(sys.argv) > 1 else ""
    print("PROBE-BEGIN")
    report("unix_socket_create", lambda: socket.socket(
        socket.AF_UNIX, socket.SOCK_STREAM).close())
    if unix_path:
        report("unix_connect", unix_connect(unix_path))
    report("unix_sockopt", sockopt)
    report("inet_socket_create", inet_socket)
    report("inet_bind_listen", loopback_bind)
    report("tcp_connect_1.1.1.1:443", tcp_connect)
    report("write_cwd", write_cwd)
    report("read_etc_hostname", read_outside)
    print("PROBE-END")


if __name__ == "__main__":
    main()
