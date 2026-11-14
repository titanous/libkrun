#include <fcntl.h>
#include <stdio.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <unistd.h>
int main(void)
{
    setvbuf(stdout,NULL,_IONBF,0);
    mount("sysfs","/sys","sysfs",0,NULL);
    int fd=open("/sys/power/nitro_freeze",O_WRONLY);
    puts("TIMING SOURCE ready");
    if(fd<0 || write(fd,"1",1)!=1) {
        puts("TIMING FAIL freeze");
        reboot(RB_POWER_OFF);
        return 1;
    }
    puts("TIMING FIRST USER");
    sleep(12);
    puts("TIMING PASS");
    reboot(RB_POWER_OFF);
    return 0;
}
