// qtfb wire protocol -- reMarkable 2 port.
//
// Ported from asivery/rm-appload src/qtfb/common.h.
//
// !!! ABI WARNING !!!
// Upstream relies on the *natural* struct layout of the compiler. That is fine
// on the Paper Pro (aarch64) but WRONG on the rM2 (armv7, ILP32): upstream's
// InitMessageResponseContents uses `size_t shmSize`, which is 8 bytes on
// aarch64 and 4 bytes on armv7. With the natural armv7 layout a ServerMessage
// would be 24 bytes with shmSize at offset 8, whereas every existing qtfb
// client (including riddle/src/qtfb.rs) expects 32 bytes with shmSize at
// offset 16.
//
// This header therefore *pins* the layout to the aarch64 one by replacing
// `size_t` with an explicit uint64_t plus an explicit padding word, and
// asserts every offset at compile time. Do not "simplify" these structs.
#pragma once

#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/types.h>

#define QTFB_DEFAULT_FRAMEBUFFER 245209899
#define SOCKET_PATH "/tmp/qtfb.sock"
// shm_open() name; the client opens /dev/shm/qtfb_<key>.
#define FORMAT_SHM(var, key) char var[24]; snprintf(var, sizeof(var), "/qtfb_%d", (int) (key))

#define RM2_WIDTH 1404
#define RM2_HEIGHT 1872
#define RMPP_WIDTH 1620
#define RMPP_HEIGHT 2160
#define RMPPM_WIDTH 954
#define RMPPM_HEIGHT 1696

#define MESSAGE_INITIALIZE 0
#define MESSAGE_UPDATE 1
#define MESSAGE_CUSTOM_INITIALIZE 2
#define MESSAGE_TERMINATE 3
#define MESSAGE_USERINPUT 4
#define MESSAGE_SET_REFRESH_MODE 5
#define MESSAGE_REQUEST_FULL_REFRESH 6

// Pixel formats.
//
// CANONICAL FORMAT ON THIS DEVICE: FBFMT_RMPP_RGB565 (3), reinterpreted with
// rM2 geometry -- 1404x1872, RGB565 little-endian, 2 bytes/pixel, stride 2808
// (2808 % 4 == 0, so Qt's 4-byte scanline alignment adds no padding), shm size
// 5,256,576 bytes. This is locked with riddle/src/qtfb.rs; do not renumber it.
//
// FBFMT_RM2FB (0) and FBFMT_RMPPM_RGB565 (6) are accepted as exact aliases of
// it, so a client that sends either still negotiates the same buffer.
#define FBFMT_RM2FB 0
#define FBFMT_RMPP_RGB888 1
#define FBFMT_RMPP_RGBA8888 2
#define FBFMT_RMPP_RGB565 3
#define FBFMT_RMPPM_RGB888 4
#define FBFMT_RMPPM_RGBA8888 5
#define FBFMT_RMPPM_RGB565 6

#define UPDATE_ALL 0
#define UPDATE_PARTIAL 1

#define REFRESH_MODE_UFAST 0
#define REFRESH_MODE_FAST 1
#define REFRESH_MODE_ANIMATE 2
#define REFRESH_MODE_CONTENT 3
#define REFRESH_MODE_UI 4

#define DEFAULT_WAVEFORM_MODE REFRESH_MODE_UI

#define INPUT_TOUCH_PRESS 0x10
#define INPUT_TOUCH_RELEASE 0x11
#define INPUT_TOUCH_UPDATE 0x12

#define INPUT_PEN_PRESS 0x20
#define INPUT_PEN_RELEASE 0x21
#define INPUT_PEN_UPDATE 0x22

#define INPUT_BTN_PRESS 0x30
#define INPUT_BTN_RELEASE 0x31

#define INPUT_VKB_PRESS 0x40
#define INPUT_VKB_RELEASE 0x41

#define INPUT_BTN_X_LEFT 0
#define INPUT_BTN_X_HOME 1
#define INPUT_BTN_X_RIGHT 2

#define INPUT_VKB_SHIFTMOD 0x100000
#define INPUT_VKB_CTRLMOD 0x200000
#define INPUT_VKB_ALTMOD 0x400000
#define INPUT_VKB_DEL 0x7f
#define INPUT_VKB_PGUP 0x80
#define INPUT_VKB_PGDOWN 0x81
#define INPUT_VKB_DOWN 0x82
#define INPUT_VKB_UP 0x83
#define INPUT_VKB_LEFT 0x84
#define INPUT_VKB_RIGHT 0x85
#define INPUT_VKB_HOME 0x86
#define INPUT_VKB_END 0x87

namespace qtfb {

typedef int32_t FBKey;

struct InitMessageContents {
    int32_t framebufferKey;  // +0
    uint8_t framebufferType; // +4
};

struct CustomInitMessageContents {
    int32_t framebufferKey;  // +0
    uint8_t framebufferType; // +4
    uint16_t width;          // +6
    uint16_t height;         // +8
};

struct InitMessageResponseContents {
    int32_t shmKeyDefined; // +0
    int32_t _pad0;         // +4  (implicit on aarch64; explicit here)
    uint64_t shmSize;      // +8  (upstream `size_t`; pinned to 64 bit)
};

struct UpdateRegionMessageContents {
    int32_t type; // +0
    int32_t x, y, w, h; // +4 +8 +12 +16
};

struct UserInputContents {
    int32_t inputType; // +0
    int32_t devId;     // +4
    int32_t x, y, d;   // +8 +12 +16
};

struct ClientMessage {
    uint8_t type;
    union {
        struct InitMessageContents init;
        struct UpdateRegionMessageContents update;
        struct CustomInitMessageContents customInit;
        int32_t refreshMode;
    };
};

struct ServerMessage {
    uint8_t type;
    union {
        struct InitMessageResponseContents init;
        struct UserInputContents userInput;
    };
};

// ---------------------------------------------------------------------------
// Wire-format guarantees. These are the exact offsets riddle/src/qtfb.rs uses.
// ---------------------------------------------------------------------------
static_assert(sizeof(uint64_t) == 8, "uint64_t must be 8 bytes");
static_assert(alignof(uint64_t) == 8,
              "uint64_t must be 8-byte aligned (AAPCS); otherwise ServerMessage "
              "will not be 32 bytes and every qtfb client will mis-parse it");

// ClientMessage: 24 bytes.
static_assert(sizeof(ClientMessage) == 24, "ClientMessage must be 24 bytes");
static_assert(offsetof(ClientMessage, init) == 4, "");
static_assert(offsetof(ClientMessage, update) == 4, "");
static_assert(offsetof(ClientMessage, customInit) == 4, "");
static_assert(offsetof(ClientMessage, refreshMode) == 4, "");
static_assert(offsetof(InitMessageContents, framebufferKey) == 0, "");
static_assert(offsetof(InitMessageContents, framebufferType) == 4, "");
static_assert(offsetof(CustomInitMessageContents, width) == 6, "");
static_assert(offsetof(CustomInitMessageContents, height) == 8, "");
static_assert(offsetof(UpdateRegionMessageContents, x) == 4, "");
static_assert(offsetof(UpdateRegionMessageContents, y) == 8, "");
static_assert(offsetof(UpdateRegionMessageContents, w) == 12, "");
static_assert(offsetof(UpdateRegionMessageContents, h) == 16, "");

// ServerMessage: 32 bytes, union at +8.
static_assert(sizeof(ServerMessage) == 32, "ServerMessage must be 32 bytes");
static_assert(offsetof(ServerMessage, init) == 8, "");
static_assert(offsetof(ServerMessage, userInput) == 8, "");
static_assert(sizeof(InitMessageResponseContents) == 16, "");
static_assert(offsetof(InitMessageResponseContents, shmKeyDefined) == 0, "");
static_assert(offsetof(InitMessageResponseContents, shmSize) == 8, "");
static_assert(offsetof(UserInputContents, devId) == 4, "");
static_assert(offsetof(UserInputContents, x) == 8, "");
static_assert(offsetof(UserInputContents, y) == 12, "");
static_assert(offsetof(UserInputContents, d) == 16, "");

} // namespace qtfb
