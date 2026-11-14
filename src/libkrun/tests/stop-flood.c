#define _GNU_SOURCE
#include <unistd.h>
#include <fcntl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/reboot.h>
#include <sched.h>
#include <string.h>

/* Eight-vCPU acceptance: keep a named console busy while PID1 powers off.
 * The host deliberately holds the pipe reader open without reading it. */
int main(void) {
    mkdir("/dev", 0755);
    mount("devtmpfs", "/dev", "devtmpfs", 0, 0);
    int fd = -1;
    for (int i = 0; i < 300 && fd < 0; i++) {
        fd = open("/dev/hvc1", O_WRONLY);
        if (fd < 0) usleep(10000);
    }
    if (fd < 0) return 2;
    if (fork() == 0) {
        cpu_set_t set;
        CPU_ZERO(&set);
        CPU_SET(1, &set);
        sched_setaffinity(0, sizeof(set), &set);
        char data[4096];
        memset(data, 'X', sizeof(data));
        for (;;) write(fd, data, sizeof(data));
    }
    usleep(1000000);
    reboot(RB_POWER_OFF);
    for (;;) pause();
}
