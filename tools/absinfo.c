/* Dump EVIOCGABS ranges for an evdev node. Read-only.
 * Built with the reMarkable SDK to confirm the toolchain end-to-end on-device.
 *   usage: absinfo /dev/input/event1
 */
#include <fcntl.h>
#include <linux/input.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

static const char *axis_name(int a) {
    switch (a) {
    case ABS_X:        return "ABS_X";
    case ABS_Y:        return "ABS_Y";
    case ABS_PRESSURE: return "ABS_PRESSURE";
    case ABS_DISTANCE: return "ABS_DISTANCE";
    case ABS_TILT_X:   return "ABS_TILT_X";
    case ABS_TILT_Y:   return "ABS_TILT_Y";
    case ABS_MT_POSITION_X: return "ABS_MT_POSITION_X";
    case ABS_MT_POSITION_Y: return "ABS_MT_POSITION_Y";
    case ABS_MT_SLOT:       return "ABS_MT_SLOT";
    case ABS_MT_TRACKING_ID:return "ABS_MT_TRACKING_ID";
    case ABS_MT_PRESSURE:   return "ABS_MT_PRESSURE";
    default: return NULL;
    }
}

int main(int argc, char **argv) {
    const char *path = argc > 1 ? argv[1] : "/dev/input/event1";
    int fd = open(path, O_RDONLY);
    if (fd < 0) { perror(path); return 1; }

    char name[256] = {0};
    if (ioctl(fd, EVIOCGNAME(sizeof(name)), name) >= 0)
        printf("device: %s  (%s)\n", path, name);

    for (int a = 0; a < ABS_CNT; a++) {
        struct input_absinfo ai;
        if (ioctl(fd, EVIOCGABS(a), &ai) < 0) continue;
        if (ai.minimum == 0 && ai.maximum == 0) continue;
        const char *n = axis_name(a);
        if (n) printf("  %-20s min=%-8d max=%-8d res=%d\n", n, ai.minimum, ai.maximum, ai.resolution);
        else   printf("  axis 0x%02x%12s min=%-8d max=%-8d res=%d\n", a, "", ai.minimum, ai.maximum, ai.resolution);
    }
    close(fd);
    return 0;
}
