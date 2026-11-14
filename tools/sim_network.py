#!/usr/bin/env python3
"""Simuliert (Linux, root) die AVB-Gegenseite an einem veth-Interface:
gPTP-Grandmaster, Pdelay, AVDECC-Controller (ADP/AECP/ACMP) und einen
MSRP-Listener. Prüft anschließend die empfangenen AAF-Streams.

  ip link add avbA type veth peer name avbB && ip link set avbA up && ip link set avbB up
  dante-milan-bridge run test.json      # avb_interface = avbA, dante_device = test-tone
  python3 tools/sim_network.py avbB
"""
import math, socket, struct, sys, time, threading, collections

IF = sys.argv[1] if len(sys.argv) > 1 else "avbB"
DUR = float(sys.argv[2]) if len(sys.argv) > 2 else 8.0
TAI = 37_000_000_000
s = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(3))
s.bind((IF, 0))
s.setsockopt(socket.SOL_SOCKET, 35, 1)  # SO_TIMESTAMPNS
MAC = s.getsockname()[4]
CLOCK = MAC[:3] + b"\xff\xfe" + MAC[3:]
GPTP_MC = bytes.fromhex("0180C200000E")
AVDECC_MC = bytes.fromhex("91E0F0010000")
CTRL_ID = 0x1122334455667788
lock = threading.Lock()

def send(f):
    with lock:
        s.send(f)

def gptp_now():
    return time.time_ns() + TAI

def ts10(ns):
    return struct.pack(">Q", ns // 1_000_000_000)[2:] + struct.pack(">I", ns % 1_000_000_000)

def rts10(b):
    return int.from_bytes(b[:6], "big") * 1_000_000_000 + int.from_bytes(b[6:10], "big")

def ptp(msg, length, seq, flags=0, control=5, logi=0, body=b""):
    h = bytes([0x10 | msg, 0x02]) + struct.pack(">H", length) + b"\x00\x00" + struct.pack(">H", flags)
    h += b"\x00" * 8 + b"\x00" * 4 + CLOCK + struct.pack(">H", 1) + struct.pack(">H", seq) + bytes([control, logi & 0xFF])
    return GPTP_MC + MAC + b"\x88\xf7" + h + body

stats = collections.Counter()
results = {}
streams = {}          # stream_id -> list of (seq, ts, rx_gptp, first sample)
talker_adv = {}
entity = {}
pd = {}

def gm_thread():
    seq = 0
    while not stop.is_set():
        seq = (seq + 1) & 0xFFFF
        send(ptp(0x0, 44, seq, 0x0200, 0, -3, b"\x00" * 10))
        t = gptp_now()
        tlv = struct.pack(">HH", 3, 28) + b"\x00\x80\xc2\x00\x00\x01" + b"\x00" * 26
        send(ptp(0x8, 76, seq, 0, 2, -3, ts10(t) + tlv))
        if seq % 8 == 0:
            body = b"\x00" * 10 + struct.pack(">h", 37) + b"\x00" + bytes([246]) + bytes([248, 0xFE, 0x43, 0x6A]) + bytes([248]) + CLOCK + b"\x00\x00\xa0"
            body += struct.pack(">HH", 8, 8) + CLOCK
            send(ptp(0xB, 34 + len(body), seq, 0x0008, 5, 0, body))
            # Pdelay an die Bridge
            pd["t1"] = time.time_ns(); pd["seq"] = seq
            send(ptp(0x2, 54, seq, 0, 5, 0, b"\x00" * 20))
        time.sleep(0.125)

def aecp(target, seq, cmd, payload):
    p = bytes([0xFB, 0x00]) + struct.pack(">H", 12 + len(payload)) + struct.pack(">QQHH", target, CTRL_ID, seq, cmd) + payload
    return entity["mac"] + MAC + b"\x22\xf0" + p

def acmp(msg, talker, uid):
    p = bytes([0xFC, msg]) + struct.pack(">H", 44) + b"\x00" * 8 + struct.pack(">QQQHH", CTRL_ID, talker, 0xAAAA, uid, 0)
    p += b"\x00" * 6 + struct.pack(">HHHHH", 0, 1, 0, 0, 0)
    return AVDECC_MC + MAC + b"\x22\xf0" + p

def msrp_listener_ready(sid):
    vec = struct.pack(">H", 1) + struct.pack(">Q", sid) + bytes([1 * 36]) + bytes([2 << 6])
    msg = bytes([3, 8]) + struct.pack(">H", len(vec) + 2) + vec + b"\x00\x00"
    return GPTP_MC + MAC + b"\x22\xea" + b"\x00" + msg + b"\x00\x00"

def rx_thread():
    s.settimeout(0.2)
    while not stop.is_set():
        try:
            f, anc, _, _ = s.recvmsg(2048, 64)
        except socket.timeout:
            continue
        rx = gptp_now()
        for lvl, typ, data in anc:   # Kernel-Empfangszeitstempel
            if lvl == socket.SOL_SOCKET and typ == 35:
                sec, nsec = struct.unpack("qq", data[:16]); rx = sec * 1_000_000_000 + nsec + TAI
        if f[6:12] == MAC:
            continue
        et = struct.unpack(">H", f[12:14])[0]; p = f[14:]
        if et == 0x8100:
            et = struct.unpack(">H", f[16:18])[0]; p = f[18:]
        if et == 0x88F7:
            m = p[0] & 0xF; seq = struct.unpack(">H", p[30:32])[0]
            if m == 0x2:     # Pdelay_Req der Bridge
                t2 = time.time_ns()
                send(ptp(0x3, 54, seq, 0x0200, 5, 0x7F, ts10(t2) + p[20:30]))
                send(ptp(0xA, 54, seq, 0, 5, 0x7F, ts10(time.time_ns()) + p[20:30]))
                stats["pdelay_req_from_bridge"] += 1
            elif m == 0x3:
                pd["t2"] = rts10(p[34:44]); pd["t4"] = time.time_ns()
            elif m == 0xA and "t2" in pd:
                t3 = rts10(p[34:44])
                d = ((pd["t4"] - pd["t1"]) - (t3 - pd["t2"])) / 2
                stats["pdelay_ok"] += 1; results["link_delay_ns_measured_by_switch"] = round(d)
        elif et == 0x22F0:
            st = p[0]
            if st == 0xFA and (p[1] & 0xF) == 0:
                entity["id"] = struct.unpack(">Q", p[4:12])[0]; entity["mac"] = f[6:12]
                entity["talkers"] = struct.unpack(">H", p[24:26])[0]
                stats["adp"] += 1
            elif st == 0xFB and (p[1] & 0xF) in (1, 7):
                seq, cmd = struct.unpack(">HH", p[20:24]); status = p[2] >> 3
                results.setdefault("aecp", {})[seq] = (cmd & 0x7FFF, status, p[24:])
            elif st == 0xFC:
                results.setdefault("acmp", []).append((p[1] & 0xF, p[2] >> 3, struct.unpack(">Q", p[4:12])[0], p[40:46].hex(":")))
            elif st == 0x02:
                sid = struct.unpack(">Q", p[4:12])[0]
                seq = p[2]; ts = struct.unpack(">I", p[12:16])[0]
                n = struct.unpack(">H", p[20:22])[0]
                ch = p[18] | ((p[17] & 3) << 8)
                smp = struct.unpack(">%di" % (n // 4), p[24:24 + n])
                streams.setdefault(sid, []).append((seq, ts, rx, smp[0], ch, smp[8 * 0 + 1]))
        elif et == 0x22EA:
            # Talker-Advertise der Bridge parsen
            o = 1
            while o + 4 <= len(p) and p[o:o + 2] != b"\x00\x00":
                at, al, ll = p[o], p[o + 1], struct.unpack(">H", p[o + 2:o + 4])[0]
                if at == 1:
                    v = o + 4
                    while v + 2 <= o + 4 + ll and p[v:v + 2] != b"\x00\x00":
                        n = struct.unpack(">H", p[v:v + 2])[0] & 0x1FFF
                        fv = p[v + 2:v + 2 + al]
                        talker_adv[struct.unpack(">Q", fv[:8])[0]] = (fv[8:14].hex(":"), struct.unpack(">H", fv[14:16])[0], struct.unpack(">H", fv[16:18])[0])
                        v += 2 + al + (n + 2) // 3
                o += 4 + ll
            stats["msrp"] += 1

stop = threading.Event()
threading.Thread(target=rx_thread, daemon=True).start()
threading.Thread(target=gm_thread, daemon=True).start()

# 1) ADP Discover
send(AVDECC_MC + MAC + b"\x22\xf0" + bytes([0xFA, 0x02]) + struct.pack(">H", 56) + b"\x00" * 64)
t0 = time.time()
while "id" not in entity and time.time() - t0 < 5:
    time.sleep(0.05)
assert "id" in entity, "keine ADP-Ankündigung empfangen"
print(f"ADP: Entity 0x{entity['id']:016X}, {entity['talkers']} Talker-Streams")

# 2) Komplettes Entity-Modell lesen (wie Hive / Network Manager)
seq = 0
def cmd(c, payload, wait=1.0):
    global seq
    seq += 1
    send(aecp(entity["id"], seq, c, payload))
    t = time.time()
    while seq not in results.get("aecp", {}) and time.time() - t < wait:
        time.sleep(0.005)
    return results.get("aecp", {}).get(seq)

def rd(dt, di):
    r = cmd(0x0004, struct.pack(">HHHH", 0, 0, dt, di))
    return r
r = rd(0, 0)
assert r and r[1] == 0, r
name = r[2][4 + 4 + 8 + 8 + 4 + 2 * 4 + 4 + 4 + 8:][:64].split(b"\x00")[0].decode()
print(f"READ_DESCRIPTOR ENTITY ok – Name '{name}'")
r = rd(1, 0)
d = r[2][4:]
cnt = struct.unpack(">H", d[70:72])[0]
counts = [struct.unpack(">HH", d[74 + 4 * i:78 + 4 * i]) for i in range(cnt)]
total = 0
for dt, c in counts:
    for i in range(c):
        rr = rd(dt, i); assert rr and rr[1] == 0, (dt, i, rr); total += 1
for dt, c in [(0x0F, 1), (0x0D, 1), (0x14, entity["talkers"] * 8), (0x17, entity["talkers"])]:
    for i in range(c):
        rr = rd(dt, i); assert rr and rr[1] == 0, (dt, i, rr); total += 1
print(f"Entity-Modell: {total} Descriptoren gelesen, alle SUCCESS")
assert cmd(0x0001, struct.pack(">IQHH", 0, 0, 0, 0))[1] == 0
mi = None
seq += 1
send(entity["mac"] + MAC + b"\x22\xf0" + bytes([0xFB, 0x06]) + struct.pack(">H", 20) + struct.pack(">QQH", entity["id"], CTRL_ID, seq) + bytes.fromhex("001BC50AC100") + b"\x00\x00\x00\x00")
time.sleep(0.2)
print("Milan GET_MILAN_INFO:", "ok" if seq in results.get("aecp", {}) else "keine Antwort")

# 3) Warten bis MAAP fertig + gPTP eingerastet, dann wie ein Milan-Listener binden
time.sleep(2.5)
for uid in range(entity["talkers"]):
    send(acmp(0, entity["id"], uid))
time.sleep(0.3)
probe = [a for a in results.get("acmp", []) if a[0] == 1]
for a in probe:
    print(f"ACMP PROBE_TX_RESPONSE: status {a[1]} stream 0x{a[2]:016X} dest {a[3]}")
print("MSRP Talker-Advertise:", {hex(k): v for k, v in talker_adv.items()})
r = cmd(0x000F, struct.pack(">HH", 6, 0))
print("GET_STREAM_INFO Stream 0: status", r[1])
r = cmd(0x0027, struct.pack(">HH", 9, 0))
print("GET_AVB_INFO: status", r[1], "gm 0x%016X" % struct.unpack(">Q", r[2][4:12])[0], "flags", r[2][17])

for sid in talker_adv:
    send(msrp_listener_ready(sid))
t_ready = time.time()
while time.time() - t_ready < DUR:
    for sid in talker_adv:
        send(msrp_listener_ready(sid))
    time.sleep(0.5)
stop.set(); time.sleep(0.3)

# 4) Streams auswerten
print(f"\nStats: {dict(stats)}  Link-Delay (vom Switch gemessen): {results.get('link_delay_ns_measured_by_switch')} ns")
ok = True
for sid, pk in streams.items():
    pk = pk[len(pk) // 4:]  # Einschwingphase ignorieren
    dur = (pk[-1][2] - pk[0][2]) / 1e9
    seq_err = sum(1 for a, b in zip(pk, pk[1:]) if (a[0] + 1) & 0xFF != b[0])
    dts = [((b[1] - a[1]) & 0xFFFFFFFF) for a, b in zip(pk, pk[1:])]
    dts_ok = sorted(dts)[len(dts) // 2]
    jitter = max(abs(x - 125000) for x in dts)
    margin = [(((p[1] - (p[2] & 0xFFFFFFFF)) + 2**31) % 2**32 - 2**31) / 1e6 for p in pk]
    print(f"Stream 0x{sid:016X}: {len(pk)/dur:.0f} Pakete/s, {pk[0][4]} Kanäle, Seq-Fehler {seq_err}, "
          f"TS-Schritt Median {dts_ok} ns (max. Abweichung {jitter} ns), "
          f"Presentation-Time minus Ankunft: min {min(margin):.2f} / max {max(margin):.2f} ms")
    ms = sorted(margin); late = sum(1 for m in margin if m < 0)
    big = sorted(dts, key=lambda x: abs(x - 125000))[-5:]
    print(f"   Reserve-Perzentile 0.1%/1%/50%: {ms[len(ms)//1000]:.2f} / {ms[len(ms)//100]:.2f} / {ms[len(ms)//2]:.2f} ms, "
          f"verspätet: {late} ({100*late/len(margin):.3f} %), Schritte >1 µs daneben: {sum(1 for x in dts if abs(x-125000) > 1000)}")
    ok &= abs(len(pk) / dur - 8000) < 400 and seq_err == 0 and abs(dts_ok - 125000) < 50
print("ERGEBNIS:", "OK" if ok and streams else "FEHLER")
sys.exit(0 if ok and streams else 1)
