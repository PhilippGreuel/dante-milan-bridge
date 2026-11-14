# Dante → AVB/Milan Bridge

Software-Bridge nach dem Vorbild des PreSonus AVB-D16: Dante-Audio kommt über die
**Dante Virtual Soundcard (DVS)** in den Rechner, wird über eine **frei belegbare
Routing-Matrix** verteilt und als **Milan-Talker** (AAF, 48 kHz, 32 Bit, 8 Kanäle
pro Stream) über eine **zweite Netzwerkkarte** ins AVB-Netz gesendet, z. B. an
L-Acoustics-Verstärker (LA7.16, LA12X, LA4X …).

```
Mischpult ──Dante──▶ NIC 1 ─▶ Dante Virtual Soundcard ─▶ ┌──────────────────┐
                                                          │ Routing-Matrix   │
                                                          │ Media-Clock-DLL  │
L-Acoustics ◀──Milan── AVB-Switch ◀── NIC 2 (pcap) ◀───── │ AVB/Milan-Stack  │
                                                          └──────────────────┘
```

## Funktionsumfang (Version 0.1)

| Bereich | Umsetzung |
|---|---|
| Audio-Eingang | DVS über CoreAudio (macOS), WASAPI oder ASIO (Windows) |
| Routing | Matrix Dante-Eingang → AVB-Ausgang, Web-UI, Kanalnamen, speicherbar |
| AVB-Streams | 1–8 Talker-Streams à 8 Kanäle (Standard 2 Streams = 16 Kanäle wie beim AVB-D16) |
| gPTP (802.1AS) | Slave, Pdelay-Responder und -Initiator, Servo (lineare Regression) |
| MSRP / MVRP | Talker Advertise, Domain, VLAN-Join, Auswertung von Listener Ready |
| MAAP | automatische Vergabe der Multicast-Zieladressen |
| AVDECC / Milan | ADP, AEM-Entity-Modell (27 Descriptoren), LOCK, GET_STREAM_INFO, GET_AVB_INFO, SET/GET_NAME, GET_AUDIO_MAP, Milan GET_MILAN_INFO, ACMP PROBE_TX |
| Media-Clock | AVB folgt exakt dem Dante-Takt (per DLL aus der DVS-Clock abgeleitet). Keine Abtastratenwandlung. |
| Testton | `"dante_device": "test-tone"`: Testsignal ohne Pult/DVS |

Die Bridge erscheint im **L-Acoustics Network Manager** bzw. in **Hive** als
Milan-Entity. Die Verbindung zum Verstärker wird dort wie gewohnt hergestellt.

## Installation

**macOS:** Rust installieren (`curl https://sh.rustup.rs -sSf | sh`), dann `cargo build --release`.
Für den pcap-Zugriff ohne root: Wireshark installieren (ChmodBPF) oder mit `sudo` starten.

**Windows:** [Npcap](https://npcap.com) installieren (Option *WinPcap API-compatible
mode*). Zum Bauen das Npcap-SDK herunterladen und `LIB` auf `npcap-sdk\Lib\x64` setzen.
Danach `cargo build --release`. Mit ASIO (geringere Latenz mit DVS):
`cargo build --release --features asio` (benötigt das Steinberg ASIO-SDK, siehe cpal-Doku).
Fertige Binaries baut `.github/workflows/build.yml`.

## Netzwerk-Switch: Anforderungen

AVB/Milan braucht **AVB-fähige Switches**. Sie müssen gPTP (802.1AS) als zeitbewusste
Bridge weitergeben, Streams per MSRP reservieren und Traffic per Credit-Based-Shaper
(802.1Qav) formen. gPTP- und MSRP-Frames gehen an Link-lokale Adressen
(`01:80:C2:00:00:0E`), die ein normaler Managed Switch **nicht weiterleitet**.

* **Ungeeignet:** gewöhnliche Managed Switches ohne AVB, z. B. MikroTik CRS/CSS
  (nach bisherigem Kenntnisstand ohne AVB-Unterstützung). Dahinter sehen sich Bridge und Verstärker
  zwar per AVDECC, aber es gibt weder gemeinsame gPTP-Zeit noch Stream-Reservierung.
  Milan-Listener spielen dann nicht ab.
* **Geeignet:** Milan-/AVB-Switches, z. B. L-Acoustics LS10, Luminex GigaCore,
  Netgear M4250 AV Line oder Extreme Networks mit AVB-Lizenz.
* **Direktverbindung:** Bridge-NIC direkt am AVB-Port eines Verstärkers ist protokollseitig
  möglich (gPTP/MSRP sind Punkt-zu-Punkt). Das Problem der Software-Zeitstempel bleibt
  aber bestehen: Der Verstärker prüft die Link-Verzögerung ebenfalls.
* Das Dante-Netz kann auf den vorhandenen Switches bleiben. Dante braucht kein AVB.

## Inbetriebnahme

```bash
dante-milan-bridge init bridge.json
```
```bash
dante-milan-bridge list-audio
```
```bash
dante-milan-bridge list-interfaces
```
In `bridge.json` mindestens `dante_device` (z. B. `"Dante Virtual Soundcard"`) und
`avb_interface` (Name/Teilname der AVB-Netzwerkkarte) eintragen. Dann starten:
```bash
dante-milan-bridge run bridge.json
```
Die Web-Oberfläche liegt unter <http://127.0.0.1:8090/>.

Wichtige Parameter:

| Schlüssel | Standard | Bedeutung |
|---|---|---|
| `avb_streams` | 2 | Anzahl 8-Kanal-Streams |
| `buffer_frames` | 64 | Puffergröße DVS (klein = geringe Latenz) |
| `tx_delay_us` | 4000 | Abstand Capture → Versand. Muss größer sein als der DVS-Puffer plus Scheduler-Jitter |
| `presentation_offset_us` | 2000 | Milan Max-Transit-Time. Erhöhen, falls die Verstärker „late packets“ melden |
| `always_stream` | false | auch ohne Listener senden (nur zum Testen) |
| `backend` | `milan` | `device` = Ausgabe auf ein OS-Audio-Device (siehe unten) |

Gesamtlatenz Dante → Verstärkerausgang ≈ DVS-Latenz + `tx_delay_us` + `presentation_offset_us`
(Standard etwa 7–8 ms).

## ⚠️ Bekannte Grenzen / Risiken – bitte zuerst lesen

1. **gPTP mit Software-Zeitstempeln.** Weder Npcap noch macOS-BPF liefern Hardware-Zeitstempel
   für Layer-2-PTP. Der Switch misst deshalb eine zu große Link-Verzögerung zur Bridge
   (im Test ≈ 30–50 µs statt < 1 µs). Viele AVB-Switches setzen den Port dann auf
   *asCapable = false* (Schwelle `neighborPropDelayThresh`, Standard 800 ns) und
   **verweigern die Stream-Reservierung**. Abhilfe:
   * Switch mit einstellbarer Schwelle verwenden (z. B. Netgear M4250: `dot1as pdelay-threshold`,
     Extreme, Luminex – bitte im Handbuch prüfen) und diese erhöhen, oder
   * auf macOS das **Backend `device`** mit dem eingebauten AVB-Stack von macOS verwenden
     (siehe unten). Dort macht das Betriebssystem gPTP mit Hardware-Zeitstempeln.
2. **Echtzeitverhalten.** Der Talker sendet alle 125 µs ein Paket pro Stream und hält
   dafür einen CPU-Kern im Spin-Wait. Energiesparmodi deaktivieren und unter Windows
   den Höchstleistungs-Energieplan nutzen. Der Zähler „verspätet/verworfen“ in der UI zeigt Probleme.
3. **Nicht Milan-zertifiziert.** Das Entity-Modell folgt IEEE 1722.1-2021/Milan 1.2, wurde
   aber nur gegen einen Protokollsimulator getestet, nicht gegen Hive, den Network Manager
   oder echte Verstärker. Noch nicht umgesetzt: Unsolicited Notifications, GET_COUNTERS-Werte,
   CRF-Media-Clock-Eingang, redundante Streams (Primary/Secondary).
4. **Nur Richtung Dante → AVB.** Die Gegenrichtung (AVB-Listener → DVS) braucht eine
   Abtastratenwandlung zwischen AVB- und Dante-Clock und ist noch nicht implementiert.

### Backend `device` (macOS)

macOS bringt einen eigenen AVB-Stack mit (Audio-MIDI-Setup → Fenster →
*Netzwerkgeräte-Browser*). Mit `"backend": "device"` und `"output_device": "<Name des AVB-Geräts>"`
routet die Bridge DVS → macOS-AVB-Device. Ein PI-geregelter Resampler gleicht dabei die
Clock-Drift aus. Ob die macOS-AVB-Entity mit dem L-Acoustics-Netz als Milan
zusammenarbeitet, muss vor Ort getestet werden.

## Testen ohne Hardware (Linux)

`tools/sim_network.py` simuliert Grandmaster, Switch, AVDECC-Controller und
Milan-Listener auf einem veth-Paar und wertet die empfangenen Streams aus:
```bash
ip link add avbA type veth peer name avbB && ip link set avbA up && ip link set avbB up
```
```bash
dante-milan-bridge run test.json
```
```bash
sudo python3 tools/sim_network.py avbB 10
```
(`test.json`: `{"dante_device":"test-tone","avb_interface":"avbA"}`)

## Empfohlene nächste Schritte

1. Test mit **Hive** (kostenlos, von L-Acoustics, Mac/Win) als Controller: Entity sichtbar?
   Kompatibilitätsanzeige? Stream an einen Milan-Listener binden.
2. Test mit dem AVB-Switch: Wird der Bridge-Port *asCapable*? (Switch-CLI / Hive „AS Path“)
3. Test mit einem L-Acoustics-Verstärker über den Network Manager.
4. Danach: Unsolicited Notifications, Rückrichtung AVB → Dante, Installer/Tray-App.

## Code-Übersicht

```
src/main.rs          CLI, Start
src/audio.rs         DVS-Eingang, Testton, Device-Backend + Drift-Resampler
src/routing.rs       lock-freie Routing-Matrix
src/timing.rs        Zeitbasis, Sample-Clock-DLL
src/net.rs           pcap/Npcap, Ethernet-Helfer
src/avb/gptp.rs      802.1AS
src/avb/mrp.rs       MSRP/MVRP
src/avb/maap.rs      MAAP
src/avb/avdecc.rs    ADP/AECP/ACMP + Entity-Modell
src/avb/avtp.rs      AAF-Pakete
src/avb/talker.rs    Echtzeit-Sende-Thread
src/avb/stack.rs     Zusammenspiel der Protokolle
src/web.rs, ui/      Web-Oberfläche
```
