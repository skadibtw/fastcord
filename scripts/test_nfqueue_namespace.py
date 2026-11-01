#!/usr/bin/env python3
"""Synthetic NFQUEUE feasibility proof. Never touches the host network namespace.

Run directly with Python3; the launcher always creates fresh user+network
namespaces. Requires Linux unshare, ip and nft. No dependencies/account/network.
This is a prototype test, NOT an authorized production packet helper.
"""
import json
import os
import socket
import struct
import subprocess
import sys

QUEUE = 42
MARK = 0xFC001
REGISTERED = b"registered-synthetic-udp:" + bytes(range(128))
OTHER = b"same-uid-unregistered-synthetic-udp"


def run(*args, **kwargs):
    return subprocess.run(args, check=True, timeout=5, **kwargs)


def attr(kind, payload):
    data = struct.pack("HH", len(payload) + 4, kind) + payload
    return data + b"\0" * (-len(data) % 4)


class Queue:
    def __init__(self):
        self.sock = socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, 12)
        self.sock.bind((0, 0))
        self.sock.settimeout(3)
        self.seq = 0
        self.config(attr(1, struct.pack("!BBH", 1, 0, socket.AF_INET)))
        self.config(attr(2, struct.pack("!IB", 4096, 2)) + attr(3, struct.pack("!I", 32)))

    def send(self, operation, payload, ack=False):
        self.seq += 1
        body = struct.pack("!BBH", socket.AF_UNSPEC, 0, QUEUE) + payload
        self.sock.sendto(struct.pack("IHHII", len(body) + 16, 0x300 | operation,
                                     1 | (4 if ack else 0), self.seq, 0) + body, (0, 0))

    def config(self, payload):
        self.send(2, payload, ack=True)
        reply = self.sock.recv(8192)
        length, kind, _, seq, _ = struct.unpack_from("IHHII", reply)
        assert length <= len(reply) and kind == 2 and seq == self.seq
        error = struct.unpack_from("i", reply, 16)[0]
        if error:
            raise OSError(-error, os.strerror(-error))

    def packet(self):
        data = self.sock.recv(8192)
        length, kind, _, _, _ = struct.unpack_from("IHHII", data)
        assert kind == 0x300 and length <= len(data)
        offset = 20
        values = {}
        while offset < length:
            size, kind = struct.unpack_from("HH", data, offset)
            assert size >= 4 and offset + size <= length
            values[kind & 0x3fff] = data[offset + 4:offset + size]
            offset += (size + 3) & ~3
        packet_id = struct.unpack_from("!I", values[1])[0]
        assert struct.unpack("!I", values[3])[0] == MARK
        return packet_id, values[10]

    def accept(self, packet_id):
        # NF_ACCEPT without payload attribute preserves the original skb bytes.
        self.send(1, attr(2, struct.pack("!II", 1, packet_id)))


def child(fd, port, payload):
    code = "import socket,sys; s=socket.socket(fileno=int(sys.argv[1])); s.sendto(bytes.fromhex(sys.argv[3]),('127.0.0.1',int(sys.argv[2])))"
    return subprocess.Popen([sys.executable, "-c", code, str(fd), str(port), payload.hex()],
                            pass_fds=(fd,))


def inside(host_net):
    assert os.readlink("/proc/self/ns/net") != host_net, "refusing host namespace"
    uid_map = open("/proc/self/uid_map").read().split()
    assert os.getuid() == 0 and len(uid_map) == 3 and uid_map[0] == "0" and uid_map[2] == "1", "refusing a namespace without a single-user mapping"
    # Require a fresh empty ruleset before creating our isolated table.
    assert not run("nft", "list", "ruleset", capture_output=True).stdout.strip()
    run("ip", "link", "set", "lo", "up")
    receiver = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    receiver.bind(("127.0.0.1", 0))
    receiver.settimeout(3)
    port = receiver.getsockname()[1]
    registered = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    registered.bind(("127.0.0.1", 0))
    registered.setsockopt(socket.SOL_SOCKET, socket.SO_MARK, MARK)
    other = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    other.bind(("127.0.0.1", 0))
    queue = Queue()
    children = []
    table_created = False
    try:
        rules = f"""table ip fastcord_fixture {{
 chain output {{ type filter hook output priority 0; policy accept;
  ip daddr 127.0.0.1 udp sport {registered.getsockname()[1]} udp dport {port} meta mark {MARK} queue num {QUEUE} bypass
 }}
}}
"""
        run("nft", "-f", "-", input=rules.encode())
        table_created = True
        children = [child(registered.fileno(), port, REGISTERED), child(other.fileno(), port, OTHER)]
        packet_id, raw = queue.packet()
        assert raw[0] >> 4 == 4 and raw[9] == socket.IPPROTO_UDP
        ip_len = (raw[0] & 15) * 4
        source, destination, udp_len, _ = struct.unpack_from("!HHHH", raw, ip_len)
        assert source == registered.getsockname()[1] and destination == port
        assert raw[ip_len + 8:ip_len + udp_len] == REGISTERED
        assert struct.unpack_from("!H", raw, 2)[0] == len(raw)
        # Registered datagram is held; unrelated same-UID child still delivers.
        assert receiver.recvfrom(4096)[0] == OTHER
        queue.accept(packet_id)
        assert receiver.recvfrom(4096)[0] == REGISTERED
        queue.sock.settimeout(0.15)
        try:
            queue.sock.recv(8192)
            raise AssertionError("unregistered flow was queued")
        except socket.timeout:
            pass
        for process in children:
            assert process.wait(timeout=3) == 0
        # A stopped listener must not blackhole this queue-bypass fixture.
        queue.sock.close()
        registered.sendto(b"listener-stopped", ("127.0.0.1", port))
        assert receiver.recvfrom(4096)[0] == b"listener-stopped"
        print(json.dumps({"result": "PASS", "queued_registered": 1,
                          "same_uid_unregistered_bypassed": True,
                          "original_payload_preserved": True,
                          "stopped_listener_bypassed": True,
                          "scope": "fresh user+network namespace; loopback only"}))
    finally:
        for process in children:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=3)
        queue.sock.close()
        for sock in (receiver, registered, other):
            sock.close()
        if table_created:
            run("nft", "delete", "table", "ip", "fastcord_fixture")
        assert not run("nft", "list", "ruleset", capture_output=True).stdout.strip()


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--inside":
        inside(sys.argv[2])
    elif len(sys.argv) == 1:
        subprocess.run(["unshare", "-Urn", sys.executable, os.path.abspath(__file__),
                        "--inside", os.readlink("/proc/self/ns/net")], check=True, timeout=20)
    else:
        raise SystemExit("usage: python3 scripts/test_nfqueue_namespace.py")
