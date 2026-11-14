#include <fcntl.h>
#include <stdio.h>
#include <sys/mount.h>
#include <unistd.h>
int main(void)
{
    mount("devtmpfs","/dev","devtmpfs",0,NULL);
    int fd=open("/dev/ttyS0",O_RDWR);
    if(fd>=0) { dup2(fd,1); dup2(fd,2); }
    puts("ORDINARY USERSPACE READY"); fflush(stdout);
    for(;;) pause();
}
