# riddle-rm2

A handwriting diary for the **reMarkable 2**. Write on the page with the stylus; a
vision LLM reads what you wrote and replies in synthesized handwriting. Summon and
dismiss it with a five-finger tap. The stock reMarkable UI keeps running underneath.

This is a port of [MaximeRivest/riddle](https://github.com/MaximeRivest/riddle),
which targets the reMarkable **Paper Pro**. Almost nothing carried over unchanged:
different CPU architecture, different screen, different digitizer, different display
stack, and an OS version with no public tooling.

It runs on **reMarkable 2, OS 3.9.3.1986**. See [Portability](#portability) before
trying it on anything else.

---

## What it does

- **Write** with the stylus anywhere on the page.
- **Rest the pen** — the page is captured and sent to a vision model.
- **The reply is drawn back as handwriting**, in ink, on the same page.
- **Five-finger tap** starts or stops the diary. No computer needed.
- The stock UI is untouched while the diary is closed; your notebooks are never modified.

---

## Architecture

Three processes, deliberately separated so a failure in any one cannot take the
tablet's UI down with it.

```
   ┌──────────────────────────── xochitl (reMarkable's UI, Qt 6.5.2) ───────────┐
   │                                                                            │
   │   riddlefb.so   ← xovi LD_PRELOAD extension                                │
   │     ├── FBController : QQuickPaintedItem   (overlay, full screen, z=10000) │
   │     ├── EPScreenMode  mode=Pen             (selects the FAST e-ink waveform)│
   │     └── qtfb server   /tmp/qtfb.sock + /dev/shm/qtfb_<key>                 │
   └────────────────────────────────────────────────────────────────────────────┘
                                    ▲  RGB565 shared memory, 1404×1872
                                    │
   ┌────────────────────────────────┴───────────┐   ┌──────────────────────────┐
   │ riddle          (Rust, armv7)              │   │ riddle-gesture (C)       │
   │  • reads the Wacom digitizer directly      │   │  • reads /dev/input/event2│
   │  • renders ink + replies into shared memory│   │  • 5-finger tap → toggles │
   │  • calls an OpenAI-compatible vision API   │   │    riddle on/off          │
   └────────────────────────────────────────────┘   └──────────────────────────┘
```

**Why an overlay rather than taking over the screen:** on the rM2 the e-ink waveform
engine (SWTCON) is compiled *inside* the xochitl binary. There is no driver, no
`libquill.so`, and no `libqsgepaper.so` to link against. Writing greyscale to
`/dev/fb0` displays nothing. The only way to put pixels on the panel — short of
reimplementing the waveform engine and stopping xochitl entirely — is to become part
of xochitl's own scene graph.

---

## Install

You need SSH access to the tablet and [xovi](https://github.com/asivery/xovi)
installed. Build everything first (see [Building](#building)).

```sh
# 1. the in-xochitl extension
scp fbcontroller/build/libriddlefb.so root@TABLET:/home/root/xovi/extensions.d/riddlefb.so

# 2. xochitl needs these in its environment
cat >> /etc/systemd/system/xochitl.service.d/xovi.conf <<'EOF'
Environment=QML_DISABLE_DISK_CACHE=1
Environment=RIDDLE_FB_SCREENMODE=0
Environment=EPFB_SPLIT_UPDATES=1
EOF
systemctl daemon-reload && systemctl restart xochitl

# 3. the app + the gesture daemon
scp riddle/target/armv7-unknown-linux-gnueabihf/release/riddle root@TABLET:/home/root/riddle/
scp gesture/riddle-gesture               root@TABLET:/home/root/riddle/
scp riddle/oracle.env.example            root@TABLET:/home/root/riddle/oracle.env   # then edit it
systemctl enable --now riddle-gesture
```

`QML_DISABLE_DISK_CACHE=1` is **required** — without it the extension aborts on every
start.

### Configuration

`oracle.env`, next to the binary, mode `600`:

```ini
RIDDLE_OPENAI_KEY=...
RIDDLE_OPENAI_BASE=https://your-gateway/v1     # /chat/completions is appended
RIDDLE_OPENAI_MODEL=minimax-m3
RIDDLE_FLUSH_MS=20
```

The model **must be vision-capable** and must stream real `content` deltas — see
[Choosing a model](#choosing-a-model), which is less obvious than it sounds.

---

## What we learned

Everything below was established the hard way, on the device. Most of it is not
documented anywhere else.

### The device

| | |
|---|---|
| SoC | i.MX7, **armv7l** 32-bit hard-float, NEON, Cortex-A7, 2 cores |
| Screen | **1404 × 1872** (Paper Pro is 1620 × 2160) |
| OS | reMarkable **3.9.3.1986**, `Codex Linux 4.0.258 (kirkstone)` |
| libc | glibc **2.35** |
| Qt | **6.5.2**, built by GCC 11.3.0 |
| Pen | `/dev/input/event1` "Wacom I2C Digitizer" |
| Touch | `/dev/input/event2` "pt_mt" |

`os-release` reports `VERSION=4.0.258` — that is the **Yocto distro** version, not the
OS version. The user-facing version is in `/usr/share/remarkable/update.conf`. Easy
to chase the wrong number for an hour.

### Getting a Qt toolchain

The device ships no headers, no CMake files, and Qt's `qconfig.h` / `qt*-config.h` are
**build-generated** — not in the source tarball, not on the device, not recoverable
from the binaries (`strings libQt6Core.so.6.5.2 | grep -c QT_FEATURE` → `0`).

The answer is the official reMarkable Yocto SDK — **matched on Qt version, not OS
version**. No SDK exists for 3.9.3, but the oldest published one ships Qt 6.5.2:

```
https://storage.googleapis.com/remarkable-codex-toolchain/3.15.4.2/remarkable-platform-image-4.1.112-rm2-public-x86_64-toolchain.sh
```

It is a shell header with an appended `.tar.xz`, extractable on any host:

```sh
off=$(( $(grep -na -m1 "^MARKER:$" installer.sh | cut -d: -f1) + 1 ))
tail -n +$off installer.sh | tar xJ -C <dest>
```

Do **not** run `relocate_sdk.py`; instead bind-mount the tree at the hardcoded
`/usr/local/oe-sdk-hardcoded-buildpath` inside a container and every ELF interpreter
path is already correct.

Verification that mattered: the SDK's Qt libraries have *identical* export counts and
identical `.dynsym` ordering to the device's, and byte-identical file sizes. Then the
real test — a cross-compiled `.so` run through the device's own loader resolved all
111 undefined symbols.

**Build flags that are not optional:**

```
-mfpu=neon -mfloat-abi=hard -mcpu=cortex-a7   # Debian armhf baseline lacks NEON
-std=c++17                                    # QT_FEATURE_cxx20 is -1
-Wl,-z,defs                                   # turn dlopen failures into link errors
```

Use the **SDK's own `moc`**. Distro mocs (Qt 6.2 / 6.4) emit the wrong metaobject
revision. GCC 12 is the wrong choice — it can emit `GLIBCXX_3.4.30` references the
device's libstdc++ lacks, which fails only at runtime.

### Three 32-vs-64-bit bugs

Every one of these produces mysterious behaviour rather than an error. Porting
aarch64 code to armv7 (ILP32), these are the class of bug to hunt first.

| where | bug | symptom |
|---|---|---|
| `riddle/src/pen.rs` | `struct input_event` is **16** bytes on armv7, not 24 — fields at 8/10/12 | every pen event decodes as garbage |
| `fbcontroller/src/qtfb_common.h` | `size_t shmSize` is **4** bytes, not 8 → 24-byte reply where the client reads 32 | handshake returns nonsense |
| `fbcontroller/src/FBController.cpp` | `QQuickPaintedItem::update(QRect())` is a **no-op** when the dirty rect is still empty | ~95% of the first frame stays black |

`input_event` is computed from `size_of::<c_long>()` and asserted by a test that runs
on the real target; the wire structs are pinned with `static_assert` on all 20 offsets,
so the armv7 compile itself is the proof.

### The digitizer is rotated

```
ABS_X 0..20966   ABS_Y 0..15725   ABS_PRESSURE 0..4095   ABS_TILT_X/Y ±9000
```

The pen's **larger axis is X**, but the screen is portrait. Pen coordinates are rotated
90° from the framebuffer:

```
screen_x = raw_y           * 1403 / 15725
screen_y = (20966 - raw_x) * 1871 / 20966       swap, invert Y, do not invert X
```

The swap is derived at runtime from `EVIOCGABS` rather than hardcoded, and all three
flags are overridable (`RIDDLE_PEN_SWAP`, `RIDDLE_PEN_INVERT_X`, `RIDDLE_PEN_INVERT_Y`)
so a wrong guess is an env var, not a rebuild. Touch (`pt_mt`) is already 1:1 with
pixels but still needs a Y flip.

### Ink latency: the real story

This took the longest and the answer was not where we looked.

The symptom: ink fell progressively behind while writing, then **"caught up in steps"**
after the pen lifted. That is a queue draining, one panel refresh at a time.

Things that were **not** the cause, each measured rather than assumed:

- **Not the update rate.** We made the flush interval tunable and tested 35 / 120 /
  250 ms. Longer intervals just make each bounding box bigger and each refresh slower.
  Total panel area per second is set by handwriting speed, not by a timer.
- **Not CPU.** Under continuous drawing: xochitl ~6.5% of one core, riddle ~2%, with an
  elevated load average — threads blocked in uninterruptible sleep, waiting on the panel.

The actual cause is in `EPFramebufferSwtcon::update`, which picks the panel waveform by
testing the damaged region against per-mode regions contributed by the scene graph:

| damage covered by | waveform |
|---|---|
| a fast region | **1 (DU)** — fast |
| full-refresh flag | 2 (GC16) |
| **nothing** → default | **3 (Gc16Fast)** — slow |

A foreign overlay contributes no region, so **every ink flush took the slow default**.

The fix is a plain QML type that xochitl already uses for its own toolbars:

```qml
EPScreenMode { anchors.fill: parent; mode: EPScreenMode.Pen }
```

`EPScreenMode` is in `com.remarkable 1.0` (C++ class `EPScreenModeItem`) with
`Mode { Pen=0, Mono=1, Gray=2, Color=3, Quality=4, TextAndImage=5, Animation=6 }`.
Parent one onto the overlay and that region gets the fast waveform. No reverse
engineering, no hardcoded addresses — the item is constructed via `QQmlComponent` and
its metaobject confirms itself at runtime (`RIDDLE_FB_PROBE=1`).

Also useful: xochitl reads an undocumented **`EPFB_SPLIT_UPDATES=1`**, which makes it
emit one panel update per mode-region instead of choosing a single conservative
waveform for the whole damaged area.

**The two settings interact, which is what made this confusing:**

| waveform | flush rate | result |
|---|---|---|
| slow | fast (35 ms) | requests pile up → lag grows, catches up in steps |
| slow | slow (250 ms) | fewer, bigger, slower refreshes → same problem |
| fast | slow (100 ms) | no backlog, but a fixed delay before ink appears |
| **fast (Pen)** | **fast (20 ms)** | **responsive, and keeps up** |

Dead end for the record: `EPFramebuffer` — the obvious-looking singleton, reachable
from QML — exposes only a read-only `backend` property, and its `clearGhosting()` and
`scheduleGhostRemoval()` are **verified no-ops on rM2** (they inherit a bare `bx lr`).

### Choosing a model

riddle sends the page as a PNG, so the model must do vision. Two non-obvious traps:

1. **Test with the parameters the client actually uses.** One model answered fine at
   `max_tokens=300` but at the real `max_tokens=2000` streamed *only* `reasoning_content`
   — which riddle's SSE parser deliberately skips — producing "empty reply" after 20 s.
   Testing non-streaming, or with a small budget, gives a false pass.
2. **Vision support is per-gateway, not per-model.** Models that are vision-capable
   upstream can still 400 on images if the gateway mishandles the request.

So the requirement is: vision **and** plain `content` deltas **at the real settings**.

### Dead ends, so you don't repeat them

| approach | why not |
|---|---|
| `rm2fb` / ddvk | Stops at OS 3.3.2.1666. Qt5→Qt6 rewrite; the string anchors and class names are gone. |
| riddle's `takeover` backend | Links `libquill.so`, which is **Paper Pro only**. Does not exist on rM2. |
| `timower/rM2-stuff` | Hardcoded per-build address tables keyed on xochitl's build ID; **no 3.9.x entry**. |
| Direct EPDC / DRM | rM2 does not use the i.MX7 EPDC — it is LCDIF plus userspace SWTCON. |
| Downgrading to 3.3.x | Moves *backwards* into Qt5, where there is no display backend at all. |
| Tailscale on the tablet | **No TUN driver** in the kernel and no `modprobe`. Userspace mode or a LAN bridge only. |

---

## Building

Everything cross-compiles in Docker; nothing but Docker is needed on the host.

```sh
# Rust app  → riddle/target/armv7-unknown-linux-gnueabihf/release/riddle
cd riddle && ./scripts/build-armv7.sh

# C++ extension (needs the reMarkable SDK, see above) → build/libriddlefb.so
RM2_SRC=$PWD/fbcontroller/src RM2_OUT=$PWD/fbcontroller/build ./sdk-run.sh sh /src/build.sh

# gesture daemon
arm-remarkable-linux-gnueabi-gcc --sysroot=$SYSROOT -mfpu=neon -mfloat-abi=hard \
    -mcpu=cortex-a7 -O2 -o riddle-gesture gesture/riddle-gesture.c
```

Build the Rust app against **glibc ≤ 2.35** — `rust:1-bullseye` (2.31) works;
bookworm (2.36) is too new.

The SDK is **not** redistributed here. Download it from the URL above.

---

## Tools

Small utilities written while porting; useful for anyone poking at this device.

| tool | what it does |
|---|---|
| `tools/absinfo.c` | dumps `EVIOCGABS` axis ranges (the device has no `evtest`, no Python) |
| `tools/grabtest.c` | reports whether an input device is exclusively `EVIOCGRAB`ed |
| `tools/carve_qml.py` | carves QML out of xochitl's embedded Qt resource section |
| `tools/qrc_names.py` | extracts the qrc name table (UTF-16BE, hashed entries) |
| `tools/hash_probe.py` | qmldiff's hash — **djb2 seeded 5481** (not 5381), each char truncated to its low byte |

### If you need qmldiff

The shipped design does **not** need it — the extension attaches to the window's
`contentItem` directly from C++, so it works with no QML patching at all. That is
deliberate: an unregistered QML type aborts the whole `MainWindow.qml` load, leaving
xochitl with no UI and a `WatchdogSec=60` restart loop, whereas a failed `.so` is just
an invisible node.

If you do want the QML route: the rM2 3.9.3 resource root is an `xofm` module tree,
e.g. `/src/xofm/modules/qtgui/qml/MainWindow.qml` — nothing like the Paper Pro's
`/qml/device/view/main/`, so Paper Pro diffs cannot be ported. Build your own hashtab
on-device with `qt-resource-rebuilder` under `QMLDIFF_HASHTAB_CREATE`. Use the
**v11-01072025** build; v19 needs GLIBC 2.38/2.39 and silently fails to load on 2.35.
Never set `QMLDIFF_HASHTAB_CREATE` at boot — hashing on a Cortex-A7 exceeds the
60-second watchdog and causes a permanent crash loop.

No hashtab is included in this repo; generate your own for your OS version.

---

## Portability

Pinned to **OS 3.9.3.1986** and verified nowhere else. What would need re-checking on
another version:

- `EPScreenMode` and its `Mode` enum values (`RIDDLE_FB_PROBE=1` prints them at runtime)
- The QML resource paths, if you use the qmldiff route
- Screen and digitizer geometry, on a different model

The `.so` deliberately contains **no hardcoded addresses** — everything is public Qt
API plus runtime metaobject lookup — so it has a fair chance of working across versions.

If your device has a pending OS update, note that `/etc` lives on the OS partition:
an A/B switch silently leaves the extension unloaded, because the systemd drop-in is
gone. `/home` survives. Recovery is `fw_setenv active_partition <n> && reboot`.

---

## Safety

Every device change here is reversible over SSH, and nothing touches the bootloader,
the kernel, or the OS partitions. The deploy scripts restart xochitl and **automatically
roll back** if it does not stay healthy for 100 seconds — because xochitl runs under
`Restart=on-failure` with `WatchdogSec=60`, so a bad extension means a restart loop.

Rollback is always: delete the `.so` from `extensions.d/` and restart xochitl.

---

## Licensing

This repository is deliberately mixed, because its parts have different origins.

| path | license | origin |
|---|---|---|
| `fbcontroller/` | **GPL-3.0** | derived from [asivery/rm-appload](https://github.com/asivery/rm-appload) (`src/qtfb`) |
| `riddle/` | **MIT** | derived from [MaximeRivest/riddle](https://github.com/MaximeRivest/riddle), © Maxime Rivest |
| `gesture/`, `tools/`, `docs/` | **MIT** | original to this project |
| `riddle/fonts/` | OFL | see `riddle/fonts/OFL.txt` |

`riddle` and `riddlefb.so` are separate programs communicating over a socket, not a
single combined work.

Not included and not redistributable: the reMarkable SDK, the `xochitl` binary, QML
extracted from it, the device's Qt libraries, and panel waveform files.

## Credits

- [MaximeRivest/riddle](https://github.com/MaximeRivest/riddle) — the original
- [asivery](https://github.com/asivery) — `xovi`, `qmldiff`, `qt-resource-rebuilder`, `rm-appload`/qtfb
- [timower/rM2-stuff](https://github.com/timower/rM2-stuff) — reference for how rM2's display layer fits together

Not affiliated with reMarkable AS.
