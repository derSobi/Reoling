#!/usr/bin/env python3
"""Not part of the app; a helper for `examples/decrypt_bc_stream.rs`.

Pulls the two raw Baichuan (BC) byte streams — one per direction — out of a
.pcap capture, using `tshark` (does the actual packet reassembly; nothing
here parses TCP/UDP itself). Works on:

  - a direct or relay-direct TCP:9000 session (most captures of the
    official app on this network), or
  - a direct UDP/P2P session (BcUdp `Data` packets, reordered by their
    packet id, exactly as Reoling's own transport reassembles them).

It does NOT work on a pure UDP-relay session: there, the login nonce never
appears as a Baichuan message (it travels inside the P2P discovery
handshake instead), so `decrypt_bc_stream` has nothing to find. If this
script reports it found no session, try recapturing while on the VPN
(direct path), or with the relay path unavailable.

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

MAGIC_DATA = bytes.fromhex("f0debc0a")


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
            f"udp && ip.addr=={device_ip}",
            "-T",
            "fields",
            "-e",
            "ip.src",
            "-e",
            "udp.payload",
        ]
    )
    packets = {"to_device": {}, "from_device": {}}
    for line in out.splitlines():
        parts = line.split("\t")
        if len(parts) < 2 or not parts[1]:
            continue
        src, hex_payload = parts[0], parts[1].replace(":", "")
        payload = bytes.fromhex(hex_payload)
        if len(payload) < 20 or payload[:4] != MAGIC_DATA:
            continue
        packet_id = struct.unpack("<I", payload[12:16])[0]
        length = struct.unpack("<I", payload[16:20])[0]
        body = payload[20 : 20 + length]
        bucket = "from_device" if src == device_ip else "to_device"
        packets[bucket][packet_id] = body
    if not packets["to_device"] and not packets["from_device"]:
        return None
    to_device = b"".join(packets["to_device"][i] for i in sorted(packets["to_device"]))
    from_device = b"".join(packets["from_device"][i] for i in sorted(packets["from_device"]))
    return to_device, from_device


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
