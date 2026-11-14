/* Minimal live userspace fixture for host stop and kill accounting. */
#include <stdio.h>
#include <unistd.h>
int main(void) {
    setbuf(stdout, NULL);
    puts("NITRO_IDLE_READY");
    for (;;) pause();
}
