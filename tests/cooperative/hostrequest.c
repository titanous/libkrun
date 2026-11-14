/* The host, not this process, requests cooperative capture. */
#include <stdio.h>
#include <unistd.h>
int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    puts("HOSTREQUEST USER READY");
    for (;;) pause();
}
