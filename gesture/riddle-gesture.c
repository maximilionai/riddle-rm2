/* riddle-gesture - toggle riddle with a five-finger tap.
 *
 * Reads the multitouch device passively (NO EVIOCGRAB, so xochitl and riddle
 * both keep seeing touch). Counts simultaneous MT slots; a tap of >= FINGERS
 * that is released within TAP_MS toggles riddle on or off.
 *
 * Deliberately a separate process rather than a change to riddlefb.so: the
 * FBController item is hidden while riddle is not running, so it would not
 * receive the very gesture meant to start it -- and a bug here must not be
 * able to take xochitl's UI down with it.
 */
#include <errno.h>
#include <fcntl.h>
#include <linux/input.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define TOUCH_DEV "/dev/input/event2"
#define RIDDLE_DIR "/home/root/riddle"
#define RIDDLE_BIN "/home/root/riddle/riddle"
#define RIDDLE_LOG "/tmp/riddle.log"
#define QTFB_KEY "777001"

#define MAX_SLOTS 32
#define FINGERS 5
#define TAP_MS 2500    /* fingers must lift within this to count as a tap */
#define DEBOUNCE_MS 2500

static volatile sig_atomic_t child_exited = 0;
static pid_t riddle_pid = 0;

static void on_sigchld(int sig) { (void) sig; child_exited = 1; }

static long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000L + ts.tv_nsec / 1000000L;
}

static void reap(void) {
    int st;
    pid_t p;
    while ((p = waitpid(-1, &st, WNOHANG)) > 0) {
        if (p == riddle_pid) {
            fprintf(stderr, "[gesture] riddle (pid %d) exited\n", (int) p);
            riddle_pid = 0;
        }
    }
    child_exited = 0;
}

static int riddle_running(void) {
    if (riddle_pid <= 0) return 0;
    if (kill(riddle_pid, 0) == 0) return 1;
    riddle_pid = 0;
    return 0;
}

static void start_riddle(void) {
    pid_t p = fork();
    if (p < 0) { perror("[gesture] fork"); return; }
    if (p == 0) {
        setsid();
        if (chdir(RIDDLE_DIR) != 0) _exit(127);
        int fd = open(RIDDLE_LOG, O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (fd >= 0) { dup2(fd, 1); dup2(fd, 2); if (fd > 2) close(fd); }
        setenv("QTFB_KEY", QTFB_KEY, 1);
        execl(RIDDLE_BIN, "riddle", (char *) NULL);
        _exit(127);
    }
    riddle_pid = p;
    fprintf(stderr, "[gesture] started riddle pid %d\n", (int) p);
}

static void stop_riddle(void) {
    if (riddle_pid <= 0) return;
    fprintf(stderr, "[gesture] stopping riddle pid %d\n", (int) riddle_pid);
    kill(riddle_pid, SIGTERM);
    for (int i = 0; i < 40; i++) {         /* up to ~4s for a clean exit */
        if (kill(riddle_pid, 0) != 0) { riddle_pid = 0; return; }
        usleep(100000);
    }
    kill(riddle_pid, SIGKILL);
    riddle_pid = 0;
}

int main(void) {
    setvbuf(stderr, NULL, _IOLBF, 0);
    signal(SIGCHLD, on_sigchld);

    int fd = open(TOUCH_DEV, O_RDONLY);
    if (fd < 0) { perror("[gesture] open " TOUCH_DEV); return 1; }

    char name[128] = {0};
    ioctl(fd, EVIOCGNAME(sizeof(name)), name);
    fprintf(stderr, "[gesture] watching %s (%s) for %d-finger taps\n",
            TOUCH_DEV, name, FINGERS);

    int tracking[MAX_SLOTS];
    for (int i = 0; i < MAX_SLOTS; i++) tracking[i] = -1;

    int slot = 0, peak = 0;
    long peak_at = 0, last_fire = 0;
    struct input_event ev;

    for (;;) {
        ssize_t n = read(fd, &ev, sizeof(ev));
        if (n != (ssize_t) sizeof(ev)) {
            if (n < 0 && errno == EINTR) { if (child_exited) reap(); continue; }
            if (n < 0) { perror("[gesture] read"); break; }
            /* Never swallow this silently: a struct-size mismatch would
             * otherwise look identical to "no touches happened". */
            static int warned = 0;
            if (!warned) {
                warned = 1;
                fprintf(stderr, "[gesture] WARNING short read: got %d bytes, expected %d\n",
                        (int) n, (int) sizeof(ev));
            }
            continue;
        }
        if (child_exited) reap();

        /* Diagnostic: dump the first events verbatim so we can tell an
         * exclusive grab (nothing arrives at all) from a parsing mistake. */
        static int dumped = 0;
        if (dumped < 60) {
            dumped++;
            fprintf(stderr, "[raw] type=0x%02x code=0x%02x value=%d\n",
                    ev.type, ev.code, ev.value);
        }

        if (ev.type == EV_ABS && ev.code == ABS_MT_SLOT) {
            if (ev.value >= 0 && ev.value < MAX_SLOTS) slot = ev.value;
        } else if (ev.type == EV_ABS && ev.code == ABS_MT_TRACKING_ID) {
            tracking[slot] = ev.value;
        } else if (ev.type == EV_SYN && ev.code == SYN_REPORT) {
            int active = 0;
            for (int i = 0; i < MAX_SLOTS; i++)
                if (tracking[i] >= 0) active++;

            if (active > peak) { peak = active; peak_at = now_ms(); }

            if (active == 0 && peak > 0) {
                long held = now_ms() - peak_at;
                /* Log every touch sequence so we can see what the panel
                 * actually reports, not just the ones that qualify. */
                fprintf(stderr, "[gesture] touch seq: peak=%d held=%ldms%s\n",
                        peak, held,
                        (peak >= FINGERS && held <= TAP_MS) ? "  -> TAP" : "");
                if (peak >= FINGERS && held <= TAP_MS) {
                    long t = now_ms();
                    if (t - last_fire > DEBOUNCE_MS) {
                        last_fire = t;
                        fprintf(stderr, "[gesture] %d-finger tap (%ldms)\n", peak, held);
                        if (riddle_running()) stop_riddle();
                        else start_riddle();
                    }
                }
                peak = 0;
            }
        }
    }
    close(fd);
    return 1;
}
