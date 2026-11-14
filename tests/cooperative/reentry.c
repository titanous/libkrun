#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <unistd.h>
static pthread_barrier_t start;
static int busy, refused, failed;
static void *trigger(void *unused)
{
    (void)unused;
    int fd=open("/sys/power/nitro_freeze",O_WRONLY);
    pthread_barrier_wait(&start);
    errno=0;
    ssize_t result=write(fd,"1",1);
    int error=errno;
    close(fd);
    if(result==-1 && error==EBUSY) __atomic_fetch_add(&busy,1,__ATOMIC_SEQ_CST);
    else if(result==-1 && error==EINVAL) __atomic_fetch_add(&refused,1,__ATOMIC_SEQ_CST);
    else __atomic_fetch_add(&failed,1,__ATOMIC_SEQ_CST);
    return NULL;
}
int main(void)
{
    setbuf(stdout,NULL);
    mount("proc","/proc","proc",0,NULL); mount("sysfs","/sys","sysfs",0,NULL);
    for(int round=0;round<8 && !busy;round++) {
        pthread_t a,b;
        pthread_barrier_init(&start,NULL,2);
        pthread_create(&a,NULL,trigger,NULL); pthread_create(&b,NULL,trigger,NULL);
        pthread_join(a,NULL); pthread_join(b,NULL);
        pthread_barrier_destroy(&start);
    }
    printf("REENTRY busy=%d refused=%d failed=%d\n",busy,refused,failed);
    puts(busy && refused && !failed ? "REENTRY PASS" : "REENTRY FAIL");
    reboot(RB_POWER_OFF); return 0;
}
