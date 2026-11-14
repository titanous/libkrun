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
    int socket_fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    struct sockaddr_vm address = {.svm_family = AF_VSOCK, .svm_cid = 2, .svm_port = 7000};
    if (connect(socket_fd, (void *)&address, sizeof(address))) return 1;
    char byte = 0;
    if (send(socket_fd, "A", 1, MSG_NOSIGNAL) != 1 || recv(socket_fd, &byte, 1, 0) != 1 || byte != 'A') return 2;
    errno = 0;
    int trigger = open("/sys/power/nitro_freeze", O_WRONLY);
    if (trigger >= 0 || errno != ENOENT) {
        puts("ORDINARY TRIGGER FAIL present");
        return 3;
    }
    if (send(socket_fd, "B", 1, MSG_NOSIGNAL) != 1 || recv(socket_fd, &byte, 1, 0) != 1 || byte != 'B') return 4;
    puts("ORDINARY TRIGGER PASS absent vsock-alive");
    sleep(2);
    close(socket_fd);
    reboot(RB_POWER_OFF);
    return 0;
}
