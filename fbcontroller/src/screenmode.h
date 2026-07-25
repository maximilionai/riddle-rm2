// screenmode.h -- rM2 fast-waveform support for the riddlefb overlay.
//
// xochitl 3.9.3 selects the panel waveform per-region from a QML item type
// registered as `EPScreenMode` (C++ class EPScreenModeItem) in module
// `com.remarkable 1.0`. The item has a single new property:
//
//     EPScreenModeItem::Mode { Pen=0, Mono=1, Gray=2, Color=3,
//                              Quality=4, TextAndImage=5, Animation=6 }
//
// Its updatePaintNode() emits an EPScreenModeNode carrying { mode, rect } into
// the scene graph; xochitl's EPRenderer accumulates one QRegion per mode class
// and EPFramebufferSwtcon::update() then picks the SWTCON waveform from which
// region contains the damage. With no EPScreenMode covering it, damage falls
// through to waveform 3 (Gc16Fast) -- the slow grayscale one we currently get.
//
// So: park an EPScreenMode over the FBController and ink gets the fast
// waveform. No reverse-engineered addresses are used; everything below is
// public Qt API plus the QML type name.

#pragma once

class QQuickItem;

namespace qtfb {
namespace screenmode {

// Values of EPScreenModeItem::Mode, verified from the binary's QMetaObject.
enum Mode {
    ModePen          = 0,
    ModeMono         = 1,
    ModeGray         = 2,
    ModeColor        = 3,
    ModeQuality      = 4,
    ModeTextAndImage = 5,
    ModeAnimation    = 6,
};

// Purely read-only diagnostic. Walks every route to the `EPFramebuffer`
// singleton and the `EPScreenMode` type and dumps their complete QMetaObject
// (class name, superclass chain, methods with parameter types, properties,
// enums with key/value pairs) to stderr. Calls nothing on either object.
// Safe to run unconditionally; every step is guarded and failure is logged,
// never fatal.
void dumpProbe(QQuickItem *anchor);

// Creates an `EPScreenMode { mode: <mode> }` via QQmlComponent, parents it onto
// `host`, and keeps it filling `host`. Returns the created item, or nullptr if
// anything went wrong (already logged). Never throws, never aborts.
//   uri  -- QML module providing EPScreenMode; defaults to "com.remarkable".
QQuickItem *attach(QQuickItem *host, int mode, const char *uri = nullptr);

} // namespace screenmode
} // namespace qtfb
