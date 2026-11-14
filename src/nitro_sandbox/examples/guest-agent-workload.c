#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/vm_sockets.h>
#include <stdint.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/timerfd.h>
#include <string.h>
#include <sys/random.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static uint64_t ns(clockid_t clock) {
    struct timespec value;
    if (clock_gettime(clock, &value)) _exit(20);
    return (uint64_t)value.tv_sec * 1000000000ULL + value.tv_nsec;
}
static int connect_host(void) {
    int fd=socket(AF_VSOCK,SOCK_STREAM|SOCK_CLOEXEC,0);
    struct sockaddr_vm address={.svm_family=AF_VSOCK,.svm_cid=2,.svm_port=7003};
    if (fd<0 || connect(fd,(struct sockaddr *)&address,sizeof(address))) _exit(21);
    return fd;
}
struct load_state { int fs, disk, stream; atomic_int stop; unsigned char expected[4096]; unsigned long writes, traffic; };
static void *write_load(void *arg) {
    struct load_state *load=arg;
    unsigned char block[4096];
    for(unsigned int n=101;!atomic_load(&load->stop);n++) {
        memset(block,(unsigned char)n,sizeof(block));
        if(pwrite(load->fs,block,sizeof(block),0)!=sizeof(block) || fsync(load->fs) || pwrite(load->disk,block,sizeof(block),0)!=sizeof(block) || fsync(load->disk)) return (void *)1;
        memcpy(load->expected,block,sizeof(block));
        load->writes++;
    }
    return NULL;
}
static void *traffic_load(void *arg) {
    struct load_state *load=arg;
    char block[1024]={0};
    while(!atomic_load(&load->stop)) {
        if(send(load->stream,block,sizeof(block),MSG_NOSIGNAL)<=0) break;
        load->traffic++;
    }
    return NULL;
}
struct timer_state { int fd; uint64_t deadline, fired_at, sleep_at, expirations, counter; };
static void *timer_wait(void *arg) {
    struct timer_state *state=arg;
    uint64_t count=0;
    if(read(state->fd,&count,sizeof(count))!=sizeof(count)) return (void *)1;
    state->expirations=count;
    state->fired_at=ns(CLOCK_MONOTONIC);
    state->counter++;
    return NULL;
}
static void *sleep_wait(void *arg) {
    struct timer_state *state=arg;
    struct timespec deadline={.tv_sec=(time_t)(state->deadline/1000000000ULL),.tv_nsec=(long)(state->deadline%1000000000ULL)};
    if(clock_nanosleep(CLOCK_MONOTONIC,TIMER_ABSTIME,&deadline,NULL)) return (void *)1;
    state->sleep_at=ns(CLOCK_MONOTONIC);
    return NULL;
}
/* Timed flood after boot: look-alike discovery lines (memory ranges, control
   page, template marker) written alternately to the console and the kernel
   log for `seconds`, then a summary line with the writer's uid. Runs as a
   thread so the agent still sees one quiescent workload process. */
struct flood_state { unsigned int seconds; unsigned int console; unsigned int kmsg; int failed; };
static void *flood(void *arg) {
    struct flood_state *state=arg;
    int console=open("/dev/console",O_WRONLY|O_NONBLOCK|O_CLOEXEC);
    int kmsg=open("/dev/kmsg",O_WRONLY|O_NONBLOCK|O_CLOEXEC);
    if(console<0 || kmsg<0) { state->failed=1; return NULL; }
    /* Announced first: the host keeps only a bounded amount of console text. */
    char start[64];
    int start_len=snprintf(start,sizeof(start),"INJECT_START uid=%u\n",(unsigned int)getuid());
    int blocking=open("/dev/console",O_WRONLY|O_CLOEXEC);
    if(blocking<0 || write(blocking,start,(size_t)start_len)!=start_len) state->failed=1;
    if(blocking>=0) close(blocking);
    uint64_t end=ns(CLOCK_MONOTONIC)+(uint64_t)state->seconds*1000000000ULL;
    for(unsigned long n=0; ns(CLOCK_MONOTONIC)<end; n++) {
        char message[256];
        int len=snprintf(message,sizeof(message),"node   0: [mem 0x%08lx-0x%08lx]\nNITRO_FREEZE_CONTROL phys=0x40000001\nNITRO_TEMPLATE_MARKER active=0\nNITRO_RECEIVER_RANGE reserved [0x70000000-0x701fffff]\n",0x80000000UL+(n%100000)*4096UL,0x80000fffUL+(n%100000)*4096UL);
        if(write(console,message,(size_t)len)>0) state->console++;
        if(write(kmsg,message,(size_t)len)>0) state->kmsg++;
    }
    char done[128];
    int len=snprintf(done,sizeof(done),"INJECT_DONE uid=%u console=%u kmsg=%u\n",(unsigned int)getuid(),state->console,state->kmsg);
    if(write(console,done,(size_t)len)<0) state->failed=1;
    close(console);
    close(kmsg);
    return NULL;
}
int main(int argc, char **argv) {
    static struct flood_state flood_state;
    if(argc>3 && !strcmp(argv[2],"inject")) {
        /* The host waits out the flood before arming and checks INJECT_DONE. */
        pthread_t flood_thread;
        flood_state.seconds=(unsigned int)strtoul(argv[3],NULL,10);
        if(pthread_create(&flood_thread,NULL,flood,&flood_state) || pthread_detach(flood_thread)) return 38;
    } else if(argc>2 && !strcmp(argv[2],"inject")) {
        const char *paths[]={"/dev/console","/dev/kmsg"};
        for(unsigned int p=0;p<2;p++) {
            int fd=open(paths[p],O_WRONLY|O_NONBLOCK|O_CLOEXEC);
            if(fd<0) return 36;
            for(unsigned int n=0;n<10000;n++) {
                char message[256];
                int len=snprintf(message,sizeof(message),"node   0: [mem 0x%08lx-0x%08lx]\nNITRO_FREEZE_CONTROL phys=0x40000001\nNITRO_TEMPLATE_MARKER active=0\n",0x80000000UL+n*4096UL,0x80000fffUL+n*4096UL);
                if(write(fd,message,(size_t)len)<0 && errno!=EAGAIN) { close(fd); return 37; }
            }
            close(fd);
        }
    }
    int fs=-1,disk=-1;
    unsigned char expected[4096],actual[4096];
    if(argc>1) {
        mkdir("/copyfs",0755);
        if(mount("copyfs","/copyfs","virtiofs",0,"")) return 30;
        fs=open("/copyfs/file",O_RDWR|O_CLOEXEC);
        disk=open("/dev/vda",O_RDWR|O_CLOEXEC);
        if(fs<0 || disk<0) return 31;
        for(unsigned int n=1;n<=100;n++) {
            memset(expected,(unsigned char)n,sizeof(expected));
            if(pwrite(fs,expected,sizeof(expected),0)!=sizeof(expected) || fsync(fs) || pwrite(disk,expected,sizeof(expected),0)!=sizeof(expected) || fsync(disk)) return 32;
        }
    }
    uint64_t nonce,fresh;
    if (getrandom(&nonce,sizeof(nonce),0)!=sizeof(nonce)) return 22;
    size_t count=16*1024*1024/sizeof(uint64_t);
    uint64_t *memory=mmap(NULL,count*8,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    if (memory==MAP_FAILED) return 23;
    for(size_t i=0;i<count;i++) memory[i]=nonce^(i*0x9e3779b97f4a7c15ULL);
    uint64_t monotonic=ns(CLOCK_MONOTONIC),realtime=ns(CLOCK_REALTIME);
    struct timer_state timers={.fd=-1,.counter=41};
    pthread_t timer_thread,sleep_thread;
    int timers_enabled=argc>3;
    if(timers_enabled) {
        unsigned long seconds=strtoul(argv[3],NULL,10);
        if(seconds<2 || seconds>3700) return 38;
        timers.fd=timerfd_create(CLOCK_MONOTONIC,TFD_CLOEXEC);
        timers.deadline=monotonic+(uint64_t)seconds*1000000000ULL;
        struct itimerspec spec={.it_value={.tv_sec=(time_t)(timers.deadline/1000000000ULL),.tv_nsec=(long)(timers.deadline%1000000000ULL)}};
        if(timers.fd<0 || timerfd_settime(timers.fd,TFD_TIMER_ABSTIME,&spec,NULL) || pthread_create(&timer_thread,NULL,timer_wait,&timers) || pthread_create(&sleep_thread,NULL,sleep_wait,&timers)) return 39;
    }
    int old=connect_host();
    struct load_state load={.fs=fs,.disk=disk,.stream=old};
    pthread_t writer,traffic;
    if(fs>=0) {
        memcpy(load.expected,expected,sizeof(expected));
        struct timeval timeout={.tv_sec=1};
        setsockopt(old,SOL_SOCKET,SO_SNDTIMEO,&timeout,sizeof(timeout));
        if(pthread_create(&writer,NULL,write_load,&load)) return 33;
    }
    dprintf(old,"READY nonce=%016lx monotonic=%lu realtime=%lu\n",nonce,monotonic,realtime);
    if(fs>=0 && pthread_create(&traffic,NULL,traffic_load,&load)) return 34;
    char byte;
    ssize_t result=read(old,&byte,1);
    int read_errno=errno;
    if(fs>=0) {
        void *writer_error=NULL;
        atomic_store(&load.stop,1);
        pthread_join(writer,&writer_error);
        pthread_join(traffic,NULL);
        if(writer_error || load.writes==0 || load.traffic==0) return 35;
        memcpy(expected,load.expected,sizeof(expected));
    }
    close(old);
    int device_bad=0;
    if(fs>=0) {
        if(pread(fs,actual,sizeof(actual),0)!=sizeof(actual) || memcmp(actual,expected,sizeof(actual))) device_bad++;
        if(pread(disk,actual,sizeof(actual),0)!=sizeof(actual) || memcmp(actual,expected,sizeof(actual))) device_bad++;
        memset(expected,0xa5,sizeof(expected));
        if(pwrite(fs,expected,sizeof(expected),0)!=sizeof(expected) || fsync(fs) || pwrite(disk,expected,sizeof(expected),0)!=sizeof(expected) || fsync(disk)) device_bad++;
        if(pread(fs,actual,sizeof(actual),0)!=sizeof(actual) || memcmp(actual,expected,sizeof(actual))) device_bad++;
        if(pread(disk,actual,sizeof(actual),0)!=sizeof(actual) || memcmp(actual,expected,sizeof(actual))) device_bad++;
    }
    if(timers_enabled) {
        void *timer_error=NULL,*sleep_error=NULL;
        pthread_join(timer_thread,&timer_error);
        pthread_join(sleep_thread,&sleep_error);
        close(timers.fd);
        if(timer_error || sleep_error || timers.expirations!=1 || timers.counter!=42 || timers.fired_at<timers.deadline || timers.sleep_at<timers.deadline) return 40;
    }
    size_t bad=0;
    for(size_t i=0;i<count;i++) if(memory[i]!=(nonce^(i*0x9e3779b97f4a7c15ULL))) bad++;
    if(getrandom(&fresh,sizeof(fresh),0)!=sizeof(fresh)) return 24;
    int device=open("/dev/vsock",O_RDONLY|O_CLOEXEC);
    unsigned int cid=0;
    if(device<0 || ioctl(device,IOCTL_VM_SOCKETS_GET_LOCAL_CID,&cid)) return 25;
    close(device);
    int report=connect_host();
    dprintf(report,"VERIFIED timers=%d expirations=%lu counter=%lu timer_late_ns=%lu sleep_late_ns=%lu bad=%zu device_bad=%d writes=%lu traffic=%lu nonce=%016lx random=%016lx cid=%u old_result=%ld old_errno=%d monotonic_before=%lu monotonic_after=%lu realtime_before=%lu realtime_after=%lu\n",timers_enabled,timers.expirations,timers.counter,timers_enabled?timers.fired_at-timers.deadline:0,timers_enabled?timers.sleep_at-timers.deadline:0,bad,device_bad,load.writes,load.traffic,nonce,fresh,cid,(long)result,read_errno,monotonic,ns(CLOCK_MONOTONIC),realtime,ns(CLOCK_REALTIME));
    close(report);
    return bad || device_bad || result>0 ? 26 : 0;
}
