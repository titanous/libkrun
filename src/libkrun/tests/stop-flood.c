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
    mkdir("/proc", 0755);
    mount("proc", "/proc", "proc", 0, 0);
    char cmd[4096] = {0};
    int proc = open("/proc/cmdline", O_RDONLY);
    if (proc < 0) return 5;
    ssize_t cmd_bytes = read(proc, cmd, sizeof(cmd) - 1);
    close(proc);
    if (cmd_bytes <= 0) return 6;
    char path[128] = "/dev/hvc1";
    char *argument = strstr(cmd, "pressure_port=");
    if (argument) {
        argument += strlen("pressure_port=");
        size_t length = strcspn(argument, " \n");
        if (length >= sizeof(path)) return 8;
        memcpy(path, argument, length);
        path[length] = 0;
    }
    int fd = -1;
    for (int i = 0; i < 300 && fd < 0; i++) {
        fd = open(path, O_WRONLY);
        if (fd < 0) usleep(10000);
    }
    if (fd < 0) return 2;
    int pressure = strstr(cmd, "console_pressure") != 0;
    for (int cpu = 0; cpu < (pressure ? 8 : 1); cpu++) if (fork() == 0) {
        cpu_set_t set;
        CPU_ZERO(&set);
        CPU_SET(pressure ? cpu : 1, &set);
        sched_setaffinity(0, sizeof(set), &set);
        char data[4096];
        memset(data, 'X', sizeof(data));
        for (;;) write(fd, data, sizeof(data));
    }
    if (pressure) {
        int timer = -1;
        for (int i = 0; i < 300 && timer < 0; i++) {
            timer = open("/dev/hvc0", O_WRONLY);
            if (timer < 0) usleep(10000);
        }
        if (timer < 0) return 7;
        for (int tick = 0; tick < 30; tick++) {
            if (timer >= 0) write(timer, "TICK\n", 5);
            usleep(100000);
        }
        reboot(RB_POWER_OFF);
        for (;;) pause();
    }
    usleep(1000000);
    reboot(RB_POWER_OFF);
    for (;;) pause();
}
