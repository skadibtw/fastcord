#!/usr/bin/env python3
"""Rust admission/lease core + real NFQUEUE, confined to fresh user/net namespaces.

Build: cargo build -p fastcord-platform --features test-support --example linux_packet_session
Run: python3 scripts/test_linux_packet_session.py /absolute/path/to/example
No host firewall mutation, raw injection, external endpoint or live credential.
"""
import ctypes
import errno
import json
import os
import select
import socket
import struct
import subprocess
import sys
import time
from pathlib import Path
sys.dont_write_bytecode = True
from test_nfqueue_namespace import Queue, run

REQUEST = b"\0\1\0\x46\x12\x34\x56\x78" + bytes(66)
TABLE = "fastcord_session_fixture"


def drop_capabilities():
    libc = ctypes.CDLL(None, use_errno=True)
    class Header(ctypes.Structure):
        _fields_ = [("version", ctypes.c_uint32), ("pid", ctypes.c_int)]
    class Data(ctypes.Structure):
        _fields_ = [("effective", ctypes.c_uint32), ("permitted", ctypes.c_uint32), ("inheritable", ctypes.c_uint32)]
    assert libc.prctl(38, 1, 0, 0, 0) == 0  # PR_SET_NO_NEW_PRIVS
    assert libc.capset(ctypes.byref(Header(0x20080522, 0)), ctypes.byref((Data * 2)())) == 0
    status = Path("/proc/self/status").read_text()
    assert all(int(line.split()[1], 16) == 0 for line in status.splitlines() if line.startswith(("CapEff:", "CapPrm:", "CapAmb:")))


def child(mode, fd, name, port):
    import array
    flow = socket.socket(fileno=int(fd))
    drop_capabilities()
    control = None
    if mode == "registered":
        control = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
        control.connect(bytes.fromhex(name))
        assert control.recv(4) == b"FCL1"
        frame = b"FCL1\1" + (1).to_bytes(8, "big") + b"\4\x7f\0\0\1" + bytes(12) + int(port).to_bytes(2, "big") + REQUEST
        control.sendmsg([frame], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", [flow.fileno()]))])
    print("ready", flush=True)
    for command in sys.stdin:
        command = command.strip()
        if command == "send":
            flow.sendto(REQUEST, ("127.0.0.1", int(port)))
            print("sent", flush=True)
        elif command.startswith("bind "):
            attack = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            attack.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            attack.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
            try:
                attack.bind(("127.0.0.1", int(command.split()[1])))
                raise AssertionError("same-UID source-port hijack succeeded")
            except OSError as error:
                assert error.errno == errno.EADDRINUSE
            attack.close()
            print("bind-denied", flush=True)
        elif command == "cancel":
            control.send(b"FCL1\2" + (1).to_bytes(8, "big"))
            flow.close()
            print("canceled", flush=True)
        elif command == "quit":
            break
        else:
            raise AssertionError(command)
    flow.close()
    if control:
        control.close()


def line(process):
    assert select.select([process.stdout], [], [], 5)[0], "fixture handshake timed out"
    result = process.stdout.readline().strip()
    assert result, "fixture closed before handshake"
    return result


def instruct(process, command, answer):
    process.stdin.write(command + "\n")
    process.stdin.flush()
    assert line(process) == answer


class UnmarkedQueue(Queue):
    def packet(self):
        data = self.sock.recv(8192)
        length, kind, _, _, _ = struct.unpack_from("IHHII", data)
        assert kind == 0x300 and length <= len(data)
        offset, values = 20, {}
        while offset < length:
            size, kind = struct.unpack_from("HH", data, offset)
            assert size >= 4 and offset + size <= length
            values[kind & 0x3fff] = data[offset + 4:offset + size]
            offset += (size + 3) & ~3
        return struct.unpack_from("!I", values[1])[0], values[10]


def reject_foreign_namespace(binary, host_net, foreign_fd):
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    name = b"\0fastcord-foreign-fixture-" + os.urandom(12).hex().encode()
    listener.bind(name); listener.listen(1)
    client = subprocess.Popen([sys.executable, __file__, "--child", "registered", str(foreign_fd), name.hex(), "50000"],
        pass_fds=(foreign_fd,), stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
    helper = subprocess.Popen([binary, host_net, str(listener.fileno()), str(client.pid), "0", "0"],
        pass_fds=(listener.fileno(),), stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
    try:
        assert line(helper) == "ended PermissionDenied", "foreign network-namespace FD must not admit a flow"
        assert line(client) == "ready"
        helper.stdin.write("removed\n"); helper.stdin.flush()
        client.stdin.write("quit\n"); client.stdin.flush()
        assert helper.wait(timeout=3) == client.wait(timeout=3) == 0
    finally:
        for process in (client, helper):
            if process.poll() is None:
                process.kill(); process.wait(timeout=3)
        listener.close()


def inside(binary, host_net, foreign_fd):
    assert os.readlink("/proc/self/ns/net") != host_net, "refusing host namespace"
    uid_map = Path("/proc/self/uid_map").read_text().split()
    assert os.getuid() == 0 and len(uid_map) == 3 and uid_map[0] == "0" and uid_map[2] == "1"
    assert not run("nft", "list", "ruleset", capture_output=True).stdout.strip()
    run("ip", "link", "set", "lo", "up")
    reject_foreign_namespace(binary, host_net, int(foreign_fd))
    receiver = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    receiver.bind(("127.0.0.1", 0)); receiver.settimeout(3)
    port = receiver.getsockname()[1]
    flow = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); flow.bind(("127.0.0.1", 0))
    original_port = flow.getsockname()[1]
    other = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); other.bind(("127.0.0.1", 0))
    other_port = other.getsockname()[1]
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    name = b"\0fastcord-session-fixture-" + os.urandom(12).hex().encode()
    listener.bind(name); listener.listen(2)
    children, helper, queue, table = [], None, None, False
    try:
        def launch(mode, sock):
            process = subprocess.Popen([sys.executable, __file__, "--child", mode, str(sock.fileno()), name.hex(), str(port)],
                pass_fds=(sock.fileno(),), stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
            children.append(process)
            return process
        registered = launch("registered", flow)
        unregistered = launch("other", other)
        helper = subprocess.Popen([binary, host_net, str(listener.fileno()), str(registered.pid), "0", "0"],
            pass_fds=(listener.fileno(),), stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
        event = line(helper).split()
        assert event[:2] == ["registered", "1"] and int(event[2]) == original_port and int(event[3]) == port and int(event[4]) != 0
        assert line(registered) == line(unregistered) == "ready"
        instruct(unregistered, f"bind {original_port}", "bind-denied")
        queue = UnmarkedQueue()
        rules = f"""table ip {TABLE} {{
 chain output {{ type filter hook output priority 0; policy accept;
  ip saddr 127.0.0.1 ip daddr 127.0.0.1 udp sport {original_port} udp dport {port} udp length 82 @th,64,32 0x00010046 @th,96,32 0x12345678 queue num 42 bypass
 }}
}}
"""
        run("nft", "-f", "-", input=rules.encode()); table = True
        instruct(registered, "send", "sent")
        packet_id, raw = queue.packet()
        ihl = (raw[0] & 15) * 4
        assert struct.unpack_from("!HH", raw, ihl) == (original_port, port)
        assert raw[ihl + 8:] == REQUEST
        instruct(unregistered, "send", "sent")
        payload, source = receiver.recvfrom(4096)
        assert payload == REQUEST and source[1] == other_port, "same-UID identical discovery must bypass"
        queue.accept(packet_id)
        payload, source = receiver.recvfrom(4096)
        assert payload == REQUEST and source[1] == original_port
        queue.sock.settimeout(0.05)
        try:
            queue.sock.recv(8192)
            raise AssertionError("unregistered flow was queued")
        except socket.timeout:
            pass
        # Future packets bypass a stopped listener; held-packet draining is NOT claimed.
        queue.sock.close()
        instruct(registered, "send", "sent")
        assert receiver.recvfrom(4096)[0] == REQUEST
        instruct(registered, "cancel", "canceled")
        assert line(helper) == "retired 1"
        flow.close()  # The Rust consumer now holds the sole original socket FD.
        replacement = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            replacement.bind(("127.0.0.1", original_port))
            raise AssertionError("canceled source port released before rule cleanup")
        except OSError as error:
            assert error.errno == errno.EADDRINUSE
        run("nft", "delete", "table", "ip", TABLE); table = False
        helper.stdin.write("removed\n"); helper.stdin.flush()
        assert helper.wait(timeout=3) == 0
        replacement.bind(("127.0.0.1", original_port))
        replacement.sendto(REQUEST, ("127.0.0.1", port))
        assert receiver.recvfrom(4096)[0] == REQUEST
        replacement.close()
        assert not run("nft", "list", "ruleset", capture_output=True).stdout.strip()
        print(json.dumps({"result":"PASS", "rust_core_authenticated_registration":True, "foreign_network_namespace_fd_refused":True,
            "same_uid_unprivileged_identical_payload_excluded":True, "same_uid_source_port_hijack_refused":True,
            "registered_original_preserved":True, "stopped_listener_future_packets_bypass":True,
            "canceled_source_port_pinned_until_rule_cleanup":True, "source_port_reusable_after_cleanup":True,
            "owned_table_removed":True, "scope":"fresh user/net namespace, loopback, no marks/fakes/raw injection"}))
    finally:
        if table:
            run("nft", "delete", "table", "ip", TABLE)
        if queue:
            queue.sock.close()
        for process in [*children, helper]:
            if process and process.poll() is None:
                process.kill(); process.wait(timeout=3)
        for sock in (receiver, flow, other, listener):
            sock.close()
        assert not run("nft", "list", "ruleset", capture_output=True).stdout.strip()


if __name__ == "__main__":
    if len(sys.argv) == 6 and sys.argv[1] == "--child":
        child(*sys.argv[2:])
    elif len(sys.argv) == 5 and sys.argv[1] == "--inside":
        inside(sys.argv[2], sys.argv[3], sys.argv[4])
    elif len(sys.argv) == 2:
        binary = str(Path(sys.argv[1]).resolve())
        host = os.readlink("/proc/self/ns/net")
        rejected = subprocess.run([binary, host, "-1", str(os.getpid()), str(os.getuid()), str(os.getgid())], capture_output=True, timeout=5)
        assert rejected.returncode != 0, "Rust fixture must refuse host namespace before any socket work"
        # A host UDP socket is passed only as an adversarial FD; never sent on.
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as foreign:
            foreign.bind(("127.0.0.1", 0))
            subprocess.run(["unshare", "-Urn", sys.executable, str(Path(__file__).resolve()), "--inside", binary, host, str(foreign.fileno())],
                pass_fds=(foreign.fileno(),), check=True, timeout=20)
    else:
        raise SystemExit("usage: python3 scripts/test_linux_packet_session.py <Rust example executable>")
