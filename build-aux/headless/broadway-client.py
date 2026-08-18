"""A protocol-correct-enough Broadway client, headless.

Lets the GUI run — and actually RENDER — inside the bare devcontainer,
where there is no Wayland/X display. GTK's Broadway backend
(gtk4-broadwayd, already in the image) gates the frame clock on a
connected client that echoes roundtrips; this does the minimum a browser
would: websocket handshake, initial screen-size event, serial tracking,
and ROUNDTRIP -> NOTIFY echo. All display data is discarded.

Usage (three shells, or backgrounded):
    XDG_RUNTIME_DIR=/tmp/xdg gtk4-broadwayd :5 &
    python3 build-aux/headless/broadway-client.py 8085 &
    GDK_BACKEND=broadway BROADWAY_DISPLAY=:5 TASTE_PROBE_CHECK=1 \
        cargo run -p taste-app -- "$PWD"

TASTE_PROBE_CHECK (see window.rs) then exercises the agents' UI probe —
ide_screenshot / ide_widget_geometry — through the real channel and
writes the rendered PNGs under /tmp for inspection.

Wire format (recovered from the broadway.js embedded in gtk4-broadwayd):
  incoming: per command: u8 op, u32le serial, op-specific payload (LE)
  outgoing: i32 BIG-endian words: [cmd, lastSerial, lastTimeStamp, *args]
"""
import base64
import os
import socket
import struct
import sys

EVENT_SCREEN_SIZE_CHANGED = 12
EVENT_ROUNDTRIP_NOTIFY = 14

OP_GRAB_POINTER = 0
OP_UNGRAB_POINTER = 1
OP_NEW_SURFACE = 2
OP_SHOW_SURFACE = 3
OP_HIDE_SURFACE = 4
OP_RAISE_SURFACE = 5
OP_LOWER_SURFACE = 6
OP_DESTROY_SURFACE = 7
OP_MOVE_RESIZE = 8
OP_SET_TRANSIENT_FOR = 9
OP_DISCONNECTED = 10
OP_SET_SHOW_KEYBOARD = 12
OP_UPLOAD_TEXTURE = 13
OP_RELEASE_TEXTURE = 14
OP_SET_NODES = 15
OP_ROUNDTRIP = 16

last_serial = 0


def ws_send(sock, payload):
    # Client frames must be masked (RFC 6455).
    mask = os.urandom(4)
    header = b"\x82"  # FIN + binary
    n = len(payload)
    if n < 126:
        header += bytes([0x80 | n])
    else:
        header += bytes([0x80 | 126]) + struct.pack(">H", n)
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    sock.sendall(header + mask + masked)


def send_input(sock, cmd, args):
    words = [cmd, last_serial, 0] + args
    ws_send(sock, struct.pack(f">{len(words)}i", *words))


def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return buf


def ws_messages(sock):
    """Yield complete (defragmented) websocket messages."""
    fragments = b""
    while True:
        b0, b1 = recv_exact(sock, 2)
        fin, opcode = b0 & 0x80, b0 & 0x0F
        n = b1 & 0x7F
        if n == 126:
            (n,) = struct.unpack(">H", recv_exact(sock, 2))
        elif n == 127:
            (n,) = struct.unpack(">Q", recv_exact(sock, 8))
        payload = recv_exact(sock, n) if n else b""
        if opcode == 8:  # close
            return
        if opcode in (1, 2, 0):
            fragments += payload
            if fin:
                yield fragments
                fragments = b""


def handle_commands(sock, message):
    """Walk the command stream; echo roundtrips; skip everything else."""
    global last_serial
    pos = 0

    def u8():
        nonlocal pos
        pos += 1
        return message[pos - 1]

    def u16():
        nonlocal pos
        pos += 2
        return struct.unpack_from("<H", message, pos - 2)[0]

    def u32():
        nonlocal pos
        pos += 4
        return struct.unpack_from("<I", message, pos - 2 - 2)[0]

    while pos < len(message):
        op = u8()
        last_serial = u32()
        if op in (OP_SHOW_SURFACE, OP_HIDE_SURFACE, OP_RAISE_SURFACE,
                  OP_LOWER_SURFACE, OP_DESTROY_SURFACE, OP_SET_SHOW_KEYBOARD):
            u16()
        elif op == OP_NEW_SURFACE:
            pos += 10
        elif op == OP_SET_TRANSIENT_FOR:
            pos += 4
        elif op == OP_GRAB_POINTER:
            pos += 3
        elif op in (OP_UNGRAB_POINTER, OP_DISCONNECTED):
            pass
        elif op == OP_MOVE_RESIZE:
            u16()
            flags = u8()
            if flags & 1:
                pos += 4
            if flags & 2:
                pos += 4
        elif op == OP_UPLOAD_TEXTURE:
            u32()
            size = u32()  # NOT `pos += u32()`: += loads pos before the call
            pos += size
        elif op == OP_RELEASE_TEXTURE:
            u32()
        elif op == OP_SET_NODES:
            u16()
            words = u32()
            pos += 4 * words
        elif op == OP_ROUNDTRIP:
            surface_id = u16()
            tag = u32()
            send_input(sock, EVENT_ROUNDTRIP_NOTIFY, [surface_id, tag])
            print(f"roundtrip {surface_id}/{tag} echoed", flush=True)
        else:
            print(f"unknown op {op}; stopping parse of this message", flush=True)
            return


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8085
    sock = socket.create_connection(("127.0.0.1", port))
    key = base64.b64encode(os.urandom(16)).decode()
    request = (
        f"GET /socket HTTP/1.1\r\n"
        f"Host: 127.0.0.1:{port}\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        "Sec-WebSocket-Version: 13\r\n"
        "Sec-WebSocket-Protocol: broadway\r\n\r\n"
    )
    sock.sendall(request.encode())
    # Read headers until the blank line; anything after is frame data.
    buf = b""
    while b"\r\n\r\n" not in buf:
        buf += sock.recv(1)
    print(buf.split(b"\r\n", 1)[0].decode(), flush=True)

    send_input(sock, EVENT_SCREEN_SIZE_CHANGED, [1920, 1080, 1])
    first = True
    for message in ws_messages(sock):
        if first:
            print(f"first message: {len(message)} bytes: {message[:40].hex()}", flush=True)
            first = False
        handle_commands(sock, message)


if __name__ == "__main__":
    main()
