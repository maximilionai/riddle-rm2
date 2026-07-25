# Device reference — reMarkable 2, OS 3.9.3.1986

Raw facts measured on the device. Kept separate from the README so it stays
readable. Every value here was read off real hardware, not inferred.

## Identity

```
uname -m                       armv7l
/etc/os-release                ID=codex  VERSION="4.0.258 (kirkstone)"   <- Yocto distro version
update.conf                    REMARKABLE_RELEASE_VERSION=3.9.3.1986     <- actual OS version
xochitl --version              tags/releases/3.9.3-device-RC 728f5ad
/lib/libc.so.6                 glibc 2.35
libQt6Core.so.6.5.2 banner     Qt 6.5.2 (arm-little_endian-ilp32-eabi-hardfloat
                               shared (dynamic) release build; by GCC 11.3.0)
cores                          2
```

`os-release` has **no `IMG_VERSION`**, which is why a qmldiff hashtab built here
correctly has no version record.

## Storage / partitions

```
/dev/mmcblk2p2, p3      A/B OS partitions (~270 MB each, normally ~95% full — this is normal)
/dev/mmcblk2p4          /home  (6.6 GB)
```

Install everything under `/home/root`. The systemd drop-in necessarily lives in
`/etc` (OS partition) — so an A/B update leaves it behind.

u-boot environment (`/sbin/fw_printenv`, note `/sbin` is not on the SSH PATH):

```
active_partition=3      fallback_partition=2
upgrade_available=0     bootcount=0   bootlimit=1
```

`upgrade_available=0` means a plain reboot or power loss stays on the current
partition. Recovery from an unwanted switch: `fw_setenv active_partition <n> && reboot`.
Booting the other slot does not erase the first.

## Input devices

```
event0   30370000.snvs:snvs-powerkey
event1   Wacom I2C Digitizer
event2   pt_mt
```

Pen (`event1`), via `EVIOCGABS`:

```
ABS_X          0..20966   (res 100)
ABS_Y          0..15725   (res 100)
ABS_PRESSURE   0..4095
ABS_DISTANCE   0..255
ABS_TILT_X     -9000..9000
ABS_TILT_Y     -9000..9000
keys: BTN_TOOL_PEN, BTN_TOOL_RUBBER, BTN_TOUCH, BTN_STYLUS, BTN_STYLUS2
```

Touch (`event2`):

```
ABS_MT_POSITION_X   0..1403      ABS_MT_POSITION_Y   0..1871
ABS_MT_SLOT         0..31        ABS_MT_TRACKING_ID  0..65535
ABS_MT_PRESSURE     0..255
```

Touch positions are already 1:1 with pixels; the pen is not (see README).
Nothing holds an exclusive `EVIOCGRAB` on any of them by default, so a passive
reader can watch touch while xochitl uses it — which is how the gesture daemon works.

**`struct input_event` is 16 bytes here** (32-bit `time_t`), with `type`/`code`/`value`
at offsets 8/10/12. Code written for a 64-bit kernel assumes 24 bytes at 16/18/20.

## Display

```
/sys/class/graphics/fb0/virtual_size    260,23936
bits_per_pixel                          32
stride                                  1040
```

That is SWTCON's *pan buffer* geometry, not the panel. The visible panel is
1404×1872. Writing greyscale to `/dev/fb0` shows nothing without waveform generation.

Panel waveform files: `/usr/share/remarkable/*.wbf` (~80 of them); the one selected
for this unit is copied to `/var/lib/uboot/*.wbf` at boot.

Temperature sensor used by the waveform selection:
`/sys/class/hwmon/hwmon1/name` → `sy7636a_temperature`.

**No vendor e-ink library exists**: no `libquill*`, no `libqsgepaper*`, nothing in
`/usr/lib` matching `quill|epaper|swtcon|waveform`. SWTCON is inside the xochitl
binary. There is also no `/usr/lib/plugins/scenegraph/`, which distinguishes this
version from OS ≥ 3.20 where `libqsgepaper.so` exists and exports symbols.

## No development files

```
/usr/include/qt6      absent
/usr/include/QtCore   absent
/usr/lib/cmake        absent
strings libQt6Core.so.6.5.2 | grep -c QT_FEATURE   →  0
```

So the Qt feature configuration cannot be recovered from the device. Use the SDK.

## Waveform selection (from disassembly of `EPFramebufferSwtcon::update`)

Waveform is chosen by testing the damaged region against per-mode regions
contributed by `EPScreenMode` items in the scene graph:

| damage matches | waveform |
|---|---|
| a fast region | 1 (DU) |
| one region class | 6 (GLF) |
| full-refresh flag | 2 (GC16) |
| **nothing (default)** | **3 (Gc16Fast)** |

`EPScreenMode` — QML type in `com.remarkable 1.0`, C++ class `EPScreenModeItem`,
single property `mode`:

```
Pen=0  Mono=1  Gray=2  Color=3  Quality=4  TextAndImage=5  Animation=6
```

It must be visible and have non-zero size to contribute a node.

`EPFramebuffer` (the QML singleton) is **not** useful: its whole interface is a
read-only `backend` property (`Tcon=0, Swtcon=1, Desktop=2`), and `clearGhosting()` /
`scheduleGhostRemoval()` are no-ops on rM2.

Undocumented environment variable read by xochitl: **`EPFB_SPLIT_UPDATES=1`** — emit
one panel update per mode-region rather than one conservative waveform for the whole
damaged area.

## Service behaviour

```
xochitl.service:  Restart=on-failure  WatchdogSec=60
                  StartLimitBurst=4   StartLimitIntervalSec=600
```

Anything that hangs or crashes xochitl produces a restart loop. Deploy scripts should
compare `NRestarts` against a **post-restart baseline** — a manual `systemctl restart`
does not increment it.

`systemctl is-system-running` reporting `degraded` is normal on this device:
`systemd-networkd-wait-online.service` times out ~2 minutes after every boot because
Wi-Fi comes up late.

## Quirks worth knowing

- The device **auto-suspends**. Wi-Fi firmware answers ping while the SoC sleeps, so
  "ping works but SSH times out" means asleep, not broken. Wake it with the power button.
- busybox: no `evtest`, no `python`, no `timeout`, no `xxd`. `head -3` and `sort -h`
  are unsupported; use `head -n 3`.
- `scp` can be unreliable against the stock dropbear before a key is installed; serving
  files over plain HTTP from the host is a workaround. The device cannot do HTTPS to
  GitHub with busybox `wget` (TLS failure) — but applications bundling their own
  certificate roots (e.g. rustls + webpki-roots) work fine.
- `/sbin` is not on the non-login SSH `PATH`; call `/sbin/fw_printenv` by full path.
