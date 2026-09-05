# /// script
# requires-python = ">=3.11"
# dependencies = ["cryptography>=42"]
# ///
"""Probe: does a High Performance (RFB 003.889) session hand us the Mac's audio?

Speaks the same wire remotex does (Apple DH auth, SetEncryption, rekey, record
layer, SetDisplayConfiguration, the measured encoding list), then sends the
RFBMediaStreamServerConfiguration (0x1c) with an AVConference audio offer and
listens on the UDP port the server names for SRTP audio.

The AVConference offer plists it sends (--audio-offer / --video-offer) are not in
the repo: generate them on the Mac with tmp/probe2.m (see docs/apple-vnc-889.md),
because they are AVConference-produced protobufs this probe does not synthesize.

Usage:
  uv run tests/hp_audio_probe.py HOST USER PASS
         [--audio-offer PLIST] [--video-offer PLIST]
         [--seconds 20] [--encodings-order first|last] [--no-rtcp]
"""
import argparse
import hashlib
import os
import plistlib
import socket
import struct
import sys
import threading
import time
import zlib

from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

ENC_RAW, ENC_ZLIB = 0, 6
ENC_DESKTOP_SIZE, ENC_LAST_RECT = -223, -224
ENC_CURSOR_POS, ENC_DISPLAY_INFO, ENC_USER_INFO, ENC_REKEY = 0x44C, 0x44D, 0x44E, 0x44F
ENC_CURSOR_IMAGE, ENC_DISPLAY_LAYOUT = 0x450, 0x451
ENC_VENDOR_KEYSYMS, ENC_KEYBOARD_SOURCE, ENC_DEVICE_INFO = 0x453, 0x455, 0x456
ENC_MEDIA1, ENC_MEDIA2 = 1010, 1011
ENCODINGS = [ENC_RAW, ENC_CURSOR_POS, ENC_DISPLAY_INFO, ENC_REKEY, ENC_CURSOR_IMAGE,
             ENC_DISPLAY_LAYOUT, ENC_VENDOR_KEYSYMS, ENC_KEYBOARD_SOURCE,
             ENC_DESKTOP_SIZE, ENC_LAST_RECT]


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


class Sock:
    def __init__(self, s):
        self.s = s
        self.buf = b""

    def read(self, n):
        while len(self.buf) < n:
            chunk = self.s.recv(65536)
            if not chunk:
                raise EOFError("server closed")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def write(self, b):
        self.s.sendall(b)


def aes128_ecb(key):
    return Cipher(algorithms.AES(key), modes.ECB())


def ard_auth(sock, user, password):
    gen, klen = struct.unpack(">HH", sock.read(4))
    prime = sock.read(klen)
    peer = sock.read(klen)
    p = int.from_bytes(prime, "big")
    priv = int.from_bytes(os.urandom(klen), "big")
    pub = pow(gen, priv, p).to_bytes(klen, "big")
    secret = pow(int.from_bytes(peer, "big"), priv, p).to_bytes(klen, "big")
    key = hashlib.md5(secret).digest()
    cred = bytearray(os.urandom(128))
    u = user.encode(); pw = password.encode()
    cred[0:len(u)] = u; cred[len(u)] = 0
    cred[64:64 + len(pw)] = pw; cred[64 + len(pw)] = 0
    enc = aes128_ecb(key).encryptor()
    sock.write(enc.update(bytes(cred)) + enc.finalize() + pub)
    return key


class Cbc:
    def __init__(self, key, iv):
        self.cipher = aes128_ecb(key)
        self.chain = iv
        self.seq = 0

    def encrypt(self, data):
        out = bytearray()
        enc = self.cipher.encryptor()
        for i in range(0, len(data), 16):
            blk = bytes(a ^ b for a, b in zip(data[i:i + 16], self.chain))
            c = enc.update(blk)
            out += c
            self.chain = c
        return bytes(out)

    def decrypt(self, data):
        out = bytearray()
        dec = self.cipher.decryptor()
        for i in range(0, len(data), 16):
            c = data[i:i + 16]
            pt = dec.update(c)
            out += bytes(a ^ b for a, b in zip(pt, self.chain))
            self.chain = c
        return bytes(out)

    def trailer(self, covered):
        h = hashlib.sha1(struct.pack(">I", self.seq) + covered).digest()
        self.seq += 1
        return h


class Records:
    """Record layer over a Sock: framed writes, byte-stream reads."""

    def __init__(self, sock, key, iv):
        self.sock = sock
        self.tx = Cbc(key, iv)
        self.rx = Cbc(key, iv)
        self.rbuf = b""
        self.lock = threading.Lock()

    def send(self, msg):
        body = struct.pack(">H", len(msg)) + msg
        filler = (16 - (len(body) + 20) % 16) % 16
        body += b"\0" * filler
        plain = body + self.tx.trailer(body)
        with self.lock:
            self.sock.write(struct.pack(">H", len(plain)) + self.tx.encrypt(plain))

    def _fill(self):
        (clen,) = struct.unpack(">H", self.sock.read(2))
        ct = self.sock.read(clen)
        pt = self.rx.decrypt(ct)
        covered, tag = pt[:-20], pt[-20:]
        expect = self.rx.trailer(covered)
        if tag != expect:
            raise RuntimeError("record integrity failure")
        (blen,) = struct.unpack(">H", pt[:2])
        self.rbuf += pt[2:2 + blen]

    def read(self, n):
        while len(self.rbuf) < n:
            self._fill()
        out, self.rbuf = self.rbuf[:n], self.rbuf[n:]
        return out

    def u8(self): return self.read(1)[0]
    def u16(self): return struct.unpack(">H", self.read(2))[0]
    def i32(self): return struct.unpack(">i", self.read(4))[0]
    def u32(self): return struct.unpack(">I", self.read(4))[0]


def apple_msg(kind, body):
    return bytes([kind, 0]) + struct.pack(">H", len(body)) + body


def set_display_configuration(px, scaled):
    body = struct.pack(">HHI", 1, 1, 0)
    d = bytearray()
    d += struct.pack(">H", 0x9C + 0x1C)
    d += b"\0" * (0x7A - 2)
    d += struct.pack(">II", 1, 4)
    d += struct.pack(">ff", scaled[0] / 132.0 * 25.4, scaled[1] / 132.0 * 25.4)
    d += struct.pack(">IIHHIH", 3840, 2160, 0, 0, 7, 1)
    assert len(d) == 0x9C
    d += struct.pack(">IIII", px[0], px[1], scaled[0], scaled[1])
    d += struct.pack(">dI", 60.0, 0)
    return apple_msg(0x1D, body + bytes(d))


def set_pixel_format():
    m = bytearray(20)
    m[4], m[5], m[6], m[7] = 32, 24, 0, 1
    m[8:10] = m[10:12] = m[12:14] = struct.pack(">H", 255)
    m[14], m[15], m[16] = 16, 8, 0
    return bytes(m)


def set_encodings(encs):
    return bytes([2, 0]) + struct.pack(">H", len(encs)) + b"".join(struct.pack(">i", e) for e in encs)


def update_request(incremental, w, h):
    return bytes([3, int(incremental)]) + struct.pack(">HHHH", 0, 0, w, h)


def auto_framebuffer_update(w, h):
    return bytes([9, 0]) + struct.pack(">HI", 1, 0) + struct.pack(">HHHH", 0, 0, w, h)


def media_stream_config(session_uuid, audio_offer, akeys, video_offer, vkeys, flags=0):
    """RFBMediaStreamServerConfiguration, version 3.

    Header (0x80): 0x1c, ver=3, flags, then u16 audio/v1/v2 offer lengths at
    +0x0a, session UUID at +0x14, audio key viewer->server(46) at +0x24 and
    server->viewer(46) at +0x52. Then audio offer, then the video1 key pair
    (46+46) and its offer. The Mac refuses audio-only, so a real video1 offer
    has to ride along even though we never decode the HEVC screen.
    """
    body = bytearray(0x80)
    body[0] = 0x1C
    struct.pack_into(">H", body, 4, 3)
    struct.pack_into(">I", body, 6, flags)
    struct.pack_into(">HHH", body, 0x0A, len(audio_offer), len(video_offer), 0)
    body[0x14:0x24] = session_uuid
    body[0x24:0x52] = akeys[0]
    body[0x52:0x80] = akeys[1]
    body += audio_offer
    body += vkeys[0] + vkeys[1] + video_offer
    struct.pack_into(">H", body, 2, len(body) - 4)
    return bytes(body)


def pbdump(b, indent=0):
    """Tiny protobuf walker for the negotiator media blob."""
    i, out = 0, []

    def varint():
        nonlocal i
        v = s = 0
        while True:
            c = b[i]; i += 1
            v |= (c & 0x7F) << s; s += 7
            if not c & 0x80:
                return v
    while i < len(b):
        tag = varint(); fn, wt = tag >> 3, tag & 7
        if wt == 0:
            out.append(" " * indent + f"f{fn} = {varint()}")
        elif wt == 2:
            ln = varint(); s = b[i:i + ln]; i += ln
            try:
                txt = s.decode(); ok = all(32 <= ord(c) < 127 for c in txt)
            except UnicodeDecodeError:
                ok = False
            if ok and ln:
                out.append(" " * indent + f"f{fn} = {txt!r}")
            else:
                out.append(" " * indent + f"f{fn} bytes[{ln}]")
                try:
                    out += pbdump(s, indent + 2)
                except Exception:
                    pass
        elif wt == 1:
            out.append(" " * indent + f"f{fn} fixed64"); i += 8
        elif wt == 5:
            out.append(" " * indent + f"f{fn} fixed32"); i += 4
        else:
            raise ValueError
    return out


# ---- SRTP (AES-256-CM, no auth): RFC 3711 key derivation, key_derivation_rate 0
def aes_ctr_keystream(key, iv16, n):
    enc = Cipher(algorithms.AES(key), modes.CTR(iv16)).encryptor()
    return enc.update(b"\0" * n)


def srtp_derive(master_key, master_salt, label, n):
    x = int.from_bytes(master_salt, "big") ^ (label << 48)
    iv = (x << 16).to_bytes(16, "big")
    return aes_ctr_keystream(master_key, iv, n)


class Srtp:
    def __init__(self, master46):
        self.key = srtp_derive(master46[:32], master46[32:46], 0, 32)
        self.salt = srtp_derive(master46[:32], master46[32:46], 2, 14)
        self.roc = 0
        self.last_seq = None

    def decrypt(self, ssrc, seq, payload):
        if self.last_seq is not None and seq < 0x1000 and self.last_seq > 0xF000:
            self.roc += 1
        self.last_seq = seq
        index = (self.roc << 16) | seq
        iv = (int.from_bytes(self.salt, "big") << 16) ^ (ssrc << 64) ^ (index << 16)
        ks = aes_ctr_keystream(self.key, iv.to_bytes(16, "big"), len(payload))
        return bytes(a ^ b for a, b in zip(payload, ks))


def udp_listener(mac_ip, port, srtp_s2v, srtp_v2s, stop, out_path, send_rtcp, viewer_ssrc):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
    except OSError:
        pass
    s.bind(("0.0.0.0", port))
    s.settimeout(0.5)
    log(f"udp: bound 0.0.0.0:{port}, expecting packets from {mac_ip}:{port}")
    n = 0; rtp = 0; rtcp = 0; first = None; last_rtcp = 0
    sizes = {}
    f = open(out_path, "wb")
    while not stop.is_set():
        now = time.time()
        if send_rtcp and now - last_rtcp >= 1.0:
            # Minimal RTCP receiver report (no report blocks) as SRTCP, E=1, index.
            rr = struct.pack(">BBHI", 0x80, 201, 1, viewer_ssrc)
            s.sendto(rr, (mac_ip, port))
            last_rtcp = now
        try:
            data, addr = s.recvfrom(65536)
        except socket.timeout:
            continue
        n += 1
        if first is None:
            first = time.time()
            log(f"udp: first packet {len(data)} bytes from {addr}: {data[:16].hex()}")
        if len(data) >= 12 and data[0] >> 6 == 2:
            v_p_x_cc, m_pt, seq, ts, ssrc = struct.unpack(">BBHII", data[:12])
            pt = m_pt & 0x7F
            if 200 <= pt <= 207:
                rtcp += 1
                if rtcp <= 3:
                    log(f"udp: rtcp pt={pt} len={len(data)} from {addr}")
                continue
            rtp += 1
            cc = v_p_x_cc & 0x0F; x = (v_p_x_cc >> 4) & 1
            hdr = 12 + 4 * cc
            if x:
                (_, xl) = struct.unpack(">HH", data[hdr:hdr + 4]); hdr += 4 + 4 * xl
            payload = data[hdr:]
            sizes[len(payload)] = sizes.get(len(payload), 0) + 1
            # Suite 5 = no auth (whole payload is ciphertext); suite 7 =
            # AES256-CM + HMAC-SHA1-80 (last 10 bytes are the auth tag). Decrypt
            # both readings for the first few so the wire can settle which it is.
            plain_noauth = srtp_s2v.decrypt(ssrc, seq, payload)
            plain_auth = srtp_s2v.decrypt(ssrc, seq, payload[:-10]) if len(payload) > 10 else b""
            f.write(struct.pack(">IHIH", ssrc, seq, ts, len(plain_auth)) + plain_auth)
            if rtp <= 6 or rtp % 200 == 0:
                log(f"udp: rtp #{rtp} pt={pt} m={m_pt >> 7} seq={seq} ts={ts} ssrc={ssrc:#x} "
                    f"x={x} payload={len(payload)}\n"
                    f"     suite5 plain[:16]={plain_noauth[:16].hex()}\n"
                    f"     suite7 plain[:16]={plain_auth[:16].hex()}")
        else:
            log(f"udp: non-RTP {len(data)} bytes: {data[:16].hex()}")
    f.close()
    log(f"udp: done, {n} packets, {rtp} rtp, {rtcp} rtcp, payload sizes {sorted(sizes.items())[:8]}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("host"); ap.add_argument("user"); ap.add_argument("password")
    ap.add_argument("--port", type=int, default=5900)
    ap.add_argument("--audio-offer", default="tmp/avc-audio-offer.plist")
    ap.add_argument("--video-offer", default="tmp/avc-video-offer.plist")
    ap.add_argument("--seconds", type=float, default=20)
    ap.add_argument("--encodings-order", choices=["first", "last", "none"], default="last")
    ap.add_argument("--no-rtcp", action="store_true")
    ap.add_argument("--out", default="tmp/hp_audio_rtp.bin")
    args = ap.parse_args()
    args.rtcp = not args.no_rtcp

    audio_offer = open(args.audio_offer, "rb").read()
    video_offer = open(args.video_offer, "rb").read()
    blob = zlib.decompress(plistlib.loads(audio_offer)["avcMediaStreamNegotiatorMediaBlob"])
    log(f"audio offer {len(audio_offer)}B, video offer {len(video_offer)}B")
    log("audio offer media blob:\n" + "\n".join(pbdump(blob)))

    tcp = socket.create_connection((args.host, args.port), timeout=10)
    tcp.settimeout(30)
    sock = Sock(tcp)
    ver = sock.read(12); log("server version", ver)
    sock.write(b"RFB 003.889\n")
    n = sock.read(1)[0]
    if n == 0:
        (l,) = struct.unpack(">I", sock.read(4)); raise SystemExit("refused: " + sock.read(l).decode())
    types = list(sock.read(n)); log("security types", types)
    assert 30 in types
    sock.write(bytes([30]))
    wrap = ard_auth(sock, args.user, args.password)
    (res,) = struct.unpack(">I", sock.read(4))
    if res != 0:
        (l,) = struct.unpack(">I", sock.read(4)); raise SystemExit("auth failed: " + sock.read(l).decode())
    log("authenticated")
    sock.write(bytes([0xC1]))
    w, h = struct.unpack(">HH", sock.read(4)); sock.read(16)
    (nl,) = struct.unpack(">I", sock.read(4)); name = sock.read(nl)
    log(f"ServerInit {w}x{h}, name field {nl} bytes, flags {name[2:6].hex()}, name {name[22:]!r}")

    sock.write(bytes([0x12, 0, 0, 1, 0, 1, 0, 1, 0, 0, 0, 1]))
    sock.write(bytes([0x12, 0, 0, 2, 0, 1, 0, 0]))
    # await rekey
    while True:
        t = sock.read(1)[0]
        if t == 2:
            continue
        assert t == 0, f"unexpected cleartext message {t}"
        sock.read(1); (rects,) = struct.unpack(">H", sock.read(2))
        if rects == 0:
            continue
        sock.read(8); (enc,) = struct.unpack(">i", sock.read(4))
        assert enc == ENC_REKEY, enc
        body = sock.read(36)
        assert rects == 1
        break
    dec = aes128_ecb(wrap).decryptor()
    key = dec.update(body[4:20]); iv = dec.update(body[20:36]); dec.finalize()
    gen = struct.unpack(">I", body[:4])[0]
    log(f"rekey generation {gen}; record layer up")
    rec = Records(sock, key, iv)

    px = (1600, 1000)
    rec.send(set_display_configuration(px, px))
    rec.send(set_pixel_format())
    rec.send(set_encodings(ENCODINGS))
    rec.send(auto_framebuffer_update(w, h))

    size = [w, h]
    layouts = 0
    media_sent = False
    base_port = None
    listener = None
    stop = threading.Event()
    deadline = None
    akeys = (os.urandom(46), os.urandom(46))   # audio viewer->server, server->viewer
    vkeys = (os.urandom(46), os.urandom(46))   # video1 key pair (unused for decode)
    session_uuid = os.urandom(16)
    viewer_ssrc = 0
    # The viewer SSRC is f3.f1 of the audio offer blob; used in our RTCP report.
    import re
    m = re.search(r"f3 bytes\[\d+\]\n\s+f1 = (\d+)", "\n".join(pbdump(blob)))
    viewer_ssrc = int(m.group(1)) if m else 0
    log(f"viewer SSRC {viewer_ssrc}")

    def skip(n):
        rec.read(n)

    tcp.settimeout(5)
    t0 = time.time()
    while True:
        if deadline and time.time() > deadline:
            break
        try:
            t = rec.u8()
        except socket.timeout:
            if media_sent:
                continue
            log("idle; nudging with an update request")
            rec.send(update_request(True, *size))
            continue
        if t == 0:
            rec.u8(); rects = rec.u16()
            i = 0
            while i < rects:
                i += 1
                x, y, rw, rh = struct.unpack(">HHHH", rec.read(8)); enc = rec.i32()
                if enc == ENC_RAW:
                    skip(rw * rh * 4)
                elif enc == ENC_ZLIB:
                    skip(rec.u32())
                elif enc == ENC_LAST_RECT:
                    break
                elif enc in (ENC_DESKTOP_SIZE, ENC_CURSOR_POS):
                    pass
                elif enc in (ENC_VENDOR_KEYSYMS, ENC_KEYBOARD_SOURCE, ENC_DEVICE_INFO):
                    skip(rec.u16())
                elif enc == ENC_DISPLAY_INFO:
                    head = rec.read(8); skip(struct.unpack(">H", head[4:6])[0] * 0x1C)
                elif enc == ENC_USER_INFO:
                    skip(rec.u16()); n = rec.u32(); rec.u32(); skip(n)
                elif enc == ENC_CURSOR_IMAGE:
                    rec.u32(); skip(rec.u32())
                elif enc == ENC_DISPLAY_LAYOUT:
                    declared = rec.u16(); payload = rec.read(declared - 4)
                    bw, bh = struct.unpack(">HH", payload[6:10])
                    size[:] = [bw, bh]; layouts += 1
                    log(f"layout #{layouts}: backing {bw}x{bh}, declared {declared}")
                    rec.send(auto_framebuffer_update(bw, bh))
                    if layouts == 1:
                        encs = list(ENCODINGS) + [ENC_ZLIB]
                        if args.encodings_order == "first":
                            encs = [ENC_MEDIA1] + encs
                        elif args.encodings_order == "last":
                            encs = encs + [ENC_MEDIA1]
                        rec.send(set_encodings(encs))
                        rec.send(update_request(False, bw, bh))
                        if not media_sent:
                            msg = media_stream_config(session_uuid, audio_offer, akeys, video_offer, vkeys)
                            log(f"sending RFBMediaStreamServerConfiguration ({len(msg)} bytes): {msg[:0x24].hex()}")
                            rec.send(msg)
                            media_sent = True
                            deadline = time.time() + args.seconds
                elif enc == ENC_REKEY:
                    raise SystemExit("second rekey")
                elif enc == ENC_MEDIA1:
                    sz = rec.u16(); body = rec.read(sz)
                    mtype, ver, mflags = struct.unpack(">HHI", body[:8])
                    log(f"MEDIA (enc 1010) size {sz} type {mtype} version {ver} flags {mflags:#x}: {body.hex()}")
                    if mtype == 3:
                        etype, esub = struct.unpack(">II", body[8:16])
                        log(f"  MEDIA STREAM ERROR from server: errorType {etype} subCode {esub}")
                        continue
                    if mtype != 1:
                        log("  unexpected type; skipping")
                        continue
                    base_port = struct.unpack(">H", body[8:10])[0]
                    log(f"  message 1: audio UDP port {base_port}, audio flags {body[10:14].hex()}, video1 port {struct.unpack('>H', body[14:16])[0]} flags {body[16:20].hex()}")
                    if listener is None:
                        listener = threading.Thread(target=udp_listener, args=(
                            args.host, base_port, Srtp(akeys[1]), Srtp(akeys[0]), stop, args.out, args.rtcp, viewer_ssrc), daemon=True)
                        listener.start()
                elif enc == ENC_MEDIA2:
                    sz = rec.u16(); body = rec.read(sz)
                    log(f"MEDIA MESSAGE 2 (enc 1011) size {sz}: head {body[:0x14].hex()}")
                    ver, mtype = struct.unpack(">HH", body[:4])
                    al, v1l, v2l = struct.unpack(">HHH", body[8:14])
                    log(f"  version {ver} type {mtype} answer lens audio={al} v1={v1l} v2={v2l}")
                    if mtype == 3:
                        log("  ERROR message body:", body.hex())
                    else:
                        # find plist start
                        idx = body.find(b"bplist00")
                        if idx >= 0:
                            ans = plistlib.loads(body[idx:idx + al])
                            log("  answer plist:", {k: (v if not isinstance(v, bytes) else f"<{len(v)}B>") for k, v in ans.items()})
                            if "avcMediaStreamNegotiatorMediaBlob" in ans:
                                log("  answer blob:\n" + "\n".join(pbdump(zlib.decompress(ans["avcMediaStreamNegotiatorMediaBlob"]))))
                        else:
                            log("  no plist found; raw:", body.hex())
                else:
                    raise SystemExit(f"unknown encoding {enc} ({enc:#x}) rect {rw}x{rh}+{x}+{y}")
            # keep polling
            rec.send(update_request(True, *size))
        elif t in (0x04, 0x07):
            pass
        elif t in (0x51, 0x53, 0x55, 0x56):
            skip(rec.u16())
        elif t == 2:
            pass
        elif t == 3:
            rec.read(3); skip(rec.u32())
        else:
            raise SystemExit(f"unknown server message type {t:#x}")
    stop.set()
    if listener:
        listener.join(2)
    log("closing")
    tcp.close()


if __name__ == "__main__":
    main()
