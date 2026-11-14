/* A refused cooperative freeze must recover devices and userspace. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <unistd.h>

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    mount("proc", "/proc", "proc", 0, NULL);
    mount("sysfs", "/sys", "sysfs", 0, NULL);
    for (int attempt = 0; attempt < 2; attempt++) {
        int fd = open("/sys/power/nitro_freeze", O_WRONLY);
        if (fd < 0) {
            puts("REFUSAL FAIL open");
            reboot(RB_POWER_OFF);
            return 1;
        }
        errno = 0;
        ssize_t result = write(fd, "1", 1);
        int error = errno;
        close(fd);
        if (result != -1 || error != EINVAL) {
            printf("REFUSAL FAIL result=%zd errno=%d\n", result, error);
            reboot(RB_POWER_OFF);
            return 1;
        }
        printf("REFUSAL RECOVERED attempt=%d\n", attempt);
        usleep(10000);
    }
    puts("REFUSAL PASS running after repeated refusal");
    reboot(RB_POWER_OFF);
    return 0;
}
