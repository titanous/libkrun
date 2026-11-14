#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <unistd.h>

int main(void)
{
    mkdir("/dev", 0755);
    mkdir("/proc", 0755);
    mkdir("/sys", 0755);
    mount("devtmpfs", "/dev", "devtmpfs", 0, NULL);
    mount("proc", "/proc", "proc", 0, NULL);
    mount("sysfs", "/sys", "sysfs", 0, NULL);
    int console = open("/dev/hvc0", O_RDWR);
    if (console < 0) console = open("/dev/ttyS0", O_RDWR);
    if (console >= 0) {
        dup2(console, 0); dup2(console, 1); dup2(console, 2);
    }
    setbuf(stdout, NULL);
    volatile unsigned long counter = 41;
    printf("COOPERATIVE BEFORE counter=%lu\n", counter);
    int fd = open("/sys/power/nitro_freeze", O_WRONLY);
    if (fd < 0 || write(fd, "1", 1) != 1) {
        printf("COOPERATIVE FAIL freeze errno=%d\n", errno);
        reboot(RB_POWER_OFF);
        return 1;
    }
    counter++;
    printf("COOPERATIVE RESUMED counter=%lu\n", counter);
    close(fd);
    sync();
    reboot(RB_POWER_OFF);
    for (;;) pause();
}
