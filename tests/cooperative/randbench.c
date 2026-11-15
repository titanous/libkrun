/* Cost of the getrandom system call in a guest (tests/cooperative/randbench-run.py).
 *
 * Runs as /init. For 1 and 8 threads, five times each, every thread calls getrandom(16 bytes) in a loop
 * for one second; prints "RANDBENCH threads=T rep=R calls_per_sec=N" and powers off. A template guest
 * (booted with cooperative_template) and an ordinary guest run the same program, so the difference is
 * the cost of the copy bookkeeping on the random read path.
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

static atomic_int go, stop;
static atomic_ulong total;

static void *loop(void *unused)
{
    (void)unused;
    unsigned char buffer[16];
    unsigned long calls = 0;
    while (!atomic_load(&go)) { }
    while (!atomic_load_explicit(&stop, memory_order_relaxed)) {
        if (syscall(SYS_getrandom, buffer, sizeof(buffer), 0) != sizeof(buffer)) {
            printf("RANDBENCH FAIL getrandom\n");
            exit(1);
        }
        calls++;
    }
    atomic_fetch_add(&total, calls);
    return NULL;
}

int main(void)
{
    mkdir("/dev", 0755); mkdir("/proc", 0755);
    mount("devtmpfs", "/dev", "devtmpfs", 0, NULL); mount("proc", "/proc", "proc", 0, NULL);
    int console = open("/dev/ttyS0", O_RDWR);
    if (console < 0) console = open("/dev/console", O_RDWR);
    if (console >= 0) { dup2(console, 0); dup2(console, 1); dup2(console, 2); }
    setbuf(stdout, NULL);
    int template = 0;
    char line[4096] = {0};
    int fd = open("/proc/cmdline", O_RDONLY);
    if (fd >= 0) { if (read(fd, line, sizeof(line) - 1) < 0) line[0] = 0; close(fd); }
    template = strstr(line, "cooperative_template") != NULL;
    printf("RANDBENCH START cpus=%ld template=%d\n", sysconf(_SC_NPROCESSORS_ONLN), template);
    static const int counts[] = { 1, 8 };
    for (unsigned c = 0; c < 2; c++) {
        for (int rep = 0; rep < 5; rep++) {
            pthread_t threads[8];
            atomic_store(&go, 0); atomic_store(&stop, 0); atomic_store(&total, 0);
            for (int i = 0; i < counts[c]; i++)
                if (pthread_create(&threads[i], NULL, loop, NULL)) { printf("RANDBENCH FAIL thread\n"); return 1; }
            struct timespec start, end, second = { 1, 0 };
            clock_gettime(CLOCK_MONOTONIC, &start);
            atomic_store(&go, 1);
            nanosleep(&second, NULL);
            atomic_store(&stop, 1);
            clock_gettime(CLOCK_MONOTONIC, &end);
            for (int i = 0; i < counts[c]; i++) pthread_join(threads[i], NULL);
            double seconds = (end.tv_sec - start.tv_sec) + (end.tv_nsec - start.tv_nsec) / 1e9;
            printf("RANDBENCH threads=%d rep=%d calls_per_sec=%.0f\n", counts[c], rep,
                   atomic_load(&total) / seconds);
        }
    }
    printf("RANDBENCH DONE\n");
    tcdrain(1);
    sleep(1);
    reboot(RB_POWER_OFF);
    for (;;) pause();
}
