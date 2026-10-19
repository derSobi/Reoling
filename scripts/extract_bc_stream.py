#!/usr/bin/env python3
"""Not part of the app; a helper for `examples/decrypt_bc_stream.rs`.

Pulls the two raw Baichuan (BC) byte streams — one per direction — out of a
.pcap capture, using `tshark` (does the actual packet reassembly; nothing
here parses TCP/UDP itself). Works on:

  - a direct or relay-direct TCP:9000 session (most captures of the
    official app on this network), or
  - a UDP/P2P session (BcUdp `Data` packets, reordered by their packet id,
    exactly as Reoling's own transport reassembles them; a retransmitted id
    keeps its first copy). With several sessions to/from the IP, the one
    with the most Data packets is taken.

`decrypt_bc_stream` only finds the nonce in a legacy login exchange. A UDP
session usually has none: the nonce comes from the P2P handshake
(`D2C_CFM`'s `nc`) instead. For those, and for a per-message
timeline of every TCP connection and UDP session in a capture, use
`examples/bc_timeline.rs`, which reads the handshake too.

Usage:
    python3 scripts/extract_bc_stream.py <capture.pcap> <device-ip> <out-dir>

Writes <out-dir>/to_device.bin and <out-dir>/from_device.bin. Nothing in
this script touches the login password — it only rearranges bytes that
were already in the capture.
"""
import re
import shutil
import struct
import subprocess
import sys
from pathlib import Path

# A BcUdp `Data` packet (magic 0x2a87cf10, little-endian on the wire), as
# `src/protocol/bcudp` defines it: magic, connection id, 0, packet id,
# payload length, payload.
MAGIC_UDP_DATA = bytes.fromhex("10cf872a")


def run(args):
    return subprocess.run(args, capture_output=True, text=True, check=False).stdout


def try_tcp(pcap: str, device_ip: str):
    filt = f"tcp.port==9000 && ip.addr=={device_ip} && tcp.flags.syn==1 && tcp.flags.ack==0"
    stream_ids = run(["tshark", "-r", pcap, "-Y", filt, "-T", "fields", "-e", "tcp.stream"]).split()
    if not stream_ids:
        return None
    stream_id = stream_ids[0]
    follow = run(["tshark", "-r", pcap, "-q", "-z", f"follow,tcp,raw,{stream_id}"])
    to_device, from_device = bytearray(), bytearray()
    for line in follow.splitlines():
        stripped = line.strip()
        if not re.fullmatch(r"[0-9a-f]+", stripped) or len(stripped) < 2:
            continue
        chunk = bytes.fromhex(stripped)
        # A line indented with a tab is the responder (Node 1) -> from the
        # device; Node 0 (the initiator, since :9000 is always dialled
        # outbound) -> to the device.
        (from_device if line.startswith("\t") else to_device).extend(chunk)
    if not to_device and not from_device:
        return None
    return bytes(to_device), bytes(from_device)


def try_udp(pcap: str, device_ip: str):
    out = run(
        [
            "tshark",
            "-r",
            pcap,
            "-Y",
            f"udp && ip.addr=={device_ip} && udp.payload[0:4] == 10:cf:87:2a",
            "-T",
            "fields",
            "-e",
            "ip.src",
            "-e",
            "udp.srcport",
            "-e",
            "ip.dst",
            "-e",
            "udp.dstport",
            "-e",
            "udp.payload",
        ]
    )
    # (device endpoint, other endpoint) -> direction -> connection id ->
    # packet id -> payload.
    sessions = {}
    for line in out.splitlines():
        parts = line.split("\t")
        if len(parts) < 5 or not parts[4]:
            continue
        src, sport, dst, dport, hex_payload = parts[:5]
        payload = bytes.fromhex(hex_payload.replace(":", ""))
        if len(payload) < 20 or payload[:4] != MAGIC_UDP_DATA:
            continue
        connection_id, _, packet_id, length = struct.unpack("<iIII", payload[4:20])
        body = payload[20 : 20 + length]
        from_device = src == device_ip
        key = ((src, sport), (dst, dport)) if from_device else ((dst, dport), (src, sport))
        direction = "from_device" if from_device else "to_device"
        by_id = sessions.setdefault(key, {}).setdefault(direction, {}).setdefault(connection_id, {})
        by_id.setdefault(packet_id, body)  # a retransmission keeps the first copy
    if not sessions:
        return None

    def busiest(directions, direction):
        streams = directions.get(direction, {})
        return max(streams.values(), key=len) if streams else {}

    def size(item):
        return sum(len(busiest(item[1], d)) for d in ("to_device", "from_device"))

    (device_ep, other_ep), directions = max(sessions.items(), key=size)
    if len(sessions) > 1:
        print(f"{len(sessions)} UDP sessions with {device_ip}; taking {device_ep[0]}:{device_ep[1]} <-> {other_ep[0]}:{other_ep[1]}")
    result = []
    for direction in ("to_device", "from_device"):
        packets = busiest(directions, direction)
        ids = sorted(packets)
        if ids and (ids[0] != 0 or ids[-1] - ids[0] + 1 != len(ids)):
            print(f"warning: {direction}: packet ids {ids[0]}..{ids[-1]} are not all in the capture; the stream has gaps")
        result.append(b"".join(packets[i] for i in ids))
    return result[0], result[1]


def main():
    if len(sys.argv) != 4:
        print(__doc__)
        sys.exit(1)
    pcap, device_ip, out_dir = sys.argv[1], sys.argv[2], Path(sys.argv[3])
    out_dir.mkdir(parents=True, exist_ok=True)

    # `tshark` refuses to open a capture file it doesn't own — even a
    # world-readable one (e.g. one `tcpdump` wrote as a different system
    # user, as `sudo tcpdump -w` typically does) — with a permission error
    # that has nothing to do with the actual Unix file permissions. A plain
    # copy sidesteps it: this process only needs read access to make one,
    # which the file's own permissions already grant.
    own_copy = out_dir / "_capture.pcap"
    shutil.copyfile(pcap, own_copy)
    pcap = str(own_copy)

    result = try_tcp(pcap, device_ip)
    kind = "TCP:9000"
    if result is None:
        result = try_udp(pcap, device_ip)
        kind = "UDP/P2P"
    if result is None:
        print(
            f"No TCP:9000 or BcUdp session with {device_ip} found in {pcap}.\n"
            "Check the IP (the one the official app actually reached — a relay IP if it\n"
            "didn't go direct), or this may be a UDP-relay session (see this script's\n"
            "own docstring for why that doesn't work here)."
        )
        sys.exit(1)

    to_device, from_device = result
    (out_dir / "to_device.bin").write_bytes(to_device)
    (out_dir / "from_device.bin").write_bytes(from_device)
    print(f"{kind} session found: {len(to_device)} bytes to the device, {len(from_device)} bytes from it.")
    print(f"Wrote {out_dir / 'to_device.bin'} and {out_dir / 'from_device.bin'}.")


if __name__ == "__main__":
    main()
