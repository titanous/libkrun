#define _GNU_SOURCE
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/random.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

int main(void) {
    mkdir("/proc", 0755); mkdir("/sys", 0755); mkdir("/dev", 0755);
    mount("proc", "/proc", "proc", 0, NULL);
    mount("sysfs", "/sys", "sysfs", 0, NULL);
    mount("devtmpfs", "/dev", "devtmpfs", 0, NULL);
    int fd = open("/dev/console", O_RDWR);
    if (fd >= 0) { dup2(fd, 1); dup2(fd, 2); }
    setbuf(stdout, NULL);
    uint64_t nonce = 0, fresh = 0;
    if (getrandom(&nonce, sizeof(nonce), 0) != sizeof(nonce)) return 2;
    size_t count = 16 * 1024 * 1024 / sizeof(uint64_t);
    uint64_t *memory = mmap(NULL, count * 8, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0);
    if (memory == MAP_FAILED) return 3;
    for (size_t i = 0; i < count; i++) memory[i] = nonce ^ (i * 0x9e3779b97f4a7c15ULL);
    printf("COPY_SOURCE nonce=%016lx\n", nonce);
    usleep(100000);
    printf("COPY_FREEZE_BEGIN\n");
    fd = open("/sys/power/nitro_freeze", O_WRONLY);
    if (fd < 0 || write(fd, "1", 1) != 1) { perror("freeze"); return 4; }
    close(fd);
    size_t bad = 0;
    for (size_t i = 0; i < count; i++) if (memory[i] != (nonce ^ (i * 0x9e3779b97f4a7c15ULL))) bad++;
    if (getrandom(&fresh, sizeof(fresh), 0) != sizeof(fresh)) return 5;
    struct timespec ts; clock_gettime(CLOCK_REALTIME, &ts);
    printf("COPY_VERIFY ok=%d bad=%zu nonce=%016lx random=%016lx realtime=%ld.%09ld\n", bad == 0, bad, nonce, fresh, ts.tv_sec, ts.tv_nsec);
    sleep(5);
    reboot(RB_POWER_OFF);
    return 0;
}
