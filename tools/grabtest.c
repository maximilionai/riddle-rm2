/* Is an input device exclusively grabbed by someone else? EVIOCGRAB returns
 * EBUSY if so. Grab is released immediately either way. */
#include <fcntl.h>
#include <linux/input.h>
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <sys/ioctl.h>
#include <unistd.h>
int main(int argc, char **argv) {
    for (int i = 1; i < argc; i++) {
        int fd = open(argv[i], O_RDONLY);
        if (fd < 0) { printf("%-22s open failed: %s\n", argv[i], strerror(errno)); continue; }
        char name[128] = {0};
        ioctl(fd, EVIOCGNAME(sizeof(name)), name);
        if (ioctl(fd, EVIOCGRAB, 1) == 0) {
            ioctl(fd, EVIOCGRAB, 0);
            printf("%-22s %-24s FREE (not grabbed)\n", argv[i], name);
        } else {
            printf("%-22s %-24s GRABBED by another process (%s)\n", argv[i], name, strerror(errno));
        }
        close(fd);
    }
    return 0;
}
