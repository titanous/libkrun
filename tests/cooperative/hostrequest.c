/* Minimal agent fixture: the launcher arms before allowing guest execution.
 * Production agents quiesce and wait for authorization over the control channel.
 */
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>
int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    puts("HOSTREQUEST USER READY");
    int fd = open("/sys/power/nitro_freeze", O_WRONLY);
    if (fd < 0 || write(fd, "1", 1) != 1) {
        perror("agent freeze trigger");
        return 1;
    }
    close(fd);
    for (;;) pause();
}
