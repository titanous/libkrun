#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/vm_sockets.h>
#include <stdio.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    mount("devtmpfs", "/dev", "devtmpfs", 0, NULL);
    mount("sysfs", "/sys", "sysfs", 0, NULL);
    mount("proc", "/proc", "proc", 0, NULL);
    const char *paths[] = {"/sys/firmware/devicetree/base/chosen/linux,initrd-start", "/sys/firmware/devicetree/base/chosen/linux,initrd-end"};
    for (unsigned int i = 0; i < 2; ++i) {
        unsigned char bytes[8];
        int fd = open(paths[i], O_RDONLY);
        if (fd < 0) fd = open(i ? "/proc/device-tree/chosen/linux,initrd-end" : "/proc/device-tree/chosen/linux,initrd-start", O_RDONLY);
        ssize_t n = fd < 0 ? -1 : read(fd, bytes, sizeof(bytes));
        if (fd >= 0) close(fd);
        printf("LOADER_DT_READ index=%u bytes=%ld errno=%d\n", i, (long)n, errno);
        if (n == 4 || n == 8) {
            unsigned long long value = 0;
            for (ssize_t j = 0; j < n; ++j) value = (value << 8) | bytes[j];
            printf("LOADER_INITRD_%s=0x%llx\n", i ? "END" : "START", value);
        }
    }
    FILE *iomem = fopen("/proc/iomem", "r");
    if (iomem) {
        char line[256];
        while (fgets(line, sizeof(line), iomem)) printf("LOADER_IOMEM %s", line);
        fclose(iomem);
    }
    int socket_fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    struct sockaddr_vm address = {.svm_family = AF_VSOCK, .svm_cid = 2, .svm_port = 7000};
    if (connect(socket_fd, (void *)&address, sizeof(address))) return 1;
    char byte = 0;
    if (send(socket_fd, "A", 1, MSG_NOSIGNAL) != 1 || recv(socket_fd, &byte, 1, 0) != 1 || byte != 'A') return 2;
    int trigger = open("/sys/power/nitro_freeze", O_WRONLY);
    if (trigger < 0) return 3;
    errno = 0;
    ssize_t result = write(trigger, "1\n", 2);
    int refused_errno = errno;
    close(trigger);
    if (result >= 0 || refused_errno != EINVAL) {
        printf("OVERLAP TRIGGER FAIL result=%ld errno=%d\n", (long)result, refused_errno);
        return 4;
    }
    if (send(socket_fd, "B", 1, MSG_NOSIGNAL) != 1 || recv(socket_fd, &byte, 1, 0) != 1 || byte != 'B') return 5;
    puts("OVERLAP TRIGGER PASS refused vsock-alive");
    sleep(2);
    close(socket_fd);
    reboot(RB_POWER_OFF);
    return 0;
}
