#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/vm_sockets.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static atomic_ulong sequence;
static atomic_int stopping, errors, idle;
static uint64_t ns(clockid_t clock)
{
    struct timespec ts;
    clock_gettime(clock, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + ts.tv_nsec;
}
static void fail(const char *what)
{
    printf("CLONE FAIL %s errno=%d\n", what, errno);
    sync(); reboot(RB_POWER_OFF); for (;;) pause();
}
static int connect_host(void)
{
    int fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    struct sockaddr_vm address = { .svm_family=AF_VSOCK, .svm_cid=2, .svm_port=7000 };
    struct timeval timeout = { .tv_sec=2 };
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout));
    if (fd < 0 || connect(fd, (void *)&address, sizeof(address))) fail("vsock connect");
    return fd;
}
static void echo(int fd)
{
    char reply[4];
    if (send(fd, "echo", 4, MSG_NOSIGNAL) != 4 || recv(fd, reply, 4, MSG_WAITALL) != 4 || memcmp(reply, "echo", 4)) fail("vsock echo");
}
static void *writer(void *unused)
{
    (void)unused;
    int fd = open("/data/sequence", O_CREAT|O_RDWR|O_TRUNC, 0600);
    if (fd < 0) { atomic_fetch_add(&errors, 1); return NULL; }
    while (!atomic_load(&stopping)) {
        if(atomic_load(&idle)) { usleep(10000); continue; }
        unsigned long seq = atomic_fetch_add(&sequence, 1) + 1;
        uint64_t record[512];
        for (unsigned i=0; i<512; i++) record[i] = seq ^ (0x123456789abcdef0ULL + i);
        if (pwrite(fd, record, sizeof(record), (seq % 64) * sizeof(record)) != sizeof(record) || fdatasync(fd)) atomic_fetch_add(&errors, 1);
    }
    close(fd); return NULL;
}
int main(void)
{
    mkdir("/dev",0755); mkdir("/proc",0755); mkdir("/sys",0755); mkdir("/data",0755);
    mount("devtmpfs","/dev","devtmpfs",0,NULL); mount("proc","/proc","proc",0,NULL); mount("sysfs","/sys","sysfs",0,NULL);
    int console=open("/dev/ttyS0",O_RDWR);
    if(console>=0) { dup2(console,0); dup2(console,1); dup2(console,2); }
    setbuf(stdout,NULL);
    if(mount("/dev/vda","/data","ext4",0,NULL)) fail("mount");
    int old=connect_host(); echo(old);
    pthread_t thread;
    if(pthread_create(&thread,NULL,writer,NULL)) fail("pthread");
    while(atomic_load(&sequence)<100 && !atomic_load(&errors)) usleep(1000);
    if(atomic_load(&errors)) fail("writer before freeze");
    uint64_t before=ns(CLOCK_MONOTONIC);
    unsigned long frozen_seq=atomic_load(&sequence);
    uint64_t sentinel[2]={0x1020304050607080ULL,0x90a0b0c0d0e0f001ULL}, restored[2];
    int freeze=open("/sys/power/nitro_freeze",O_WRONLY);
    printf("CLONE FREEZE seq=%lu mono=%llu realtime_ns=%llu cpus=%ld\n",frozen_seq,(unsigned long long)before,(unsigned long long)ns(CLOCK_REALTIME),sysconf(_SC_NPROCESSORS_ONLN));
    /* Keep xstate live across the syscall rather than relying on compiler spills. */
    long result;
    asm volatile("movdqu %2, %%xmm15; syscall; movdqu %%xmm15, %1"
        : "=a"(result), "=m"(restored) : "m"(sentinel), "0"(1L), "D"((long)freeze), "S"("1"), "d"(1L)
        : "rcx","r11","memory","xmm15");
    if(result!=1 || memcmp(sentinel,restored,sizeof(sentinel))) fail("freeze or SIMD");
    unsigned char random_bytes[32];
    int random_fd=open("/dev/hwrng",O_RDONLY);
    if(random_fd<0 || read(random_fd,random_bytes,sizeof(random_bytes))!=sizeof(random_bytes)) fail("entropy restore");
    close(random_fd);
    if(syscall(SYS_getrandom,random_bytes,sizeof(random_bytes),0)!=sizeof(random_bytes)) fail("getrandom restore");
    printf("CLONE RANDOM ");
    for(unsigned int i=0;i<sizeof(random_bytes);i++) printf("%02x",random_bytes[i]);
    printf("\n");
    uint64_t after=ns(CLOCK_MONOTONIC);
    if(after<before || after-before>10000000000ULL) fail("monotonic");
    char byte;
    int old_result=recv(old,&byte,1,0);
    if(old_result>0 || (old_result<0 && errno==EAGAIN)) fail("old vsock not dead");
    close(old);
    int fresh=connect_host(); echo(fresh);
    uint64_t host_time;
    if(send(fresh,"time",4,MSG_NOSIGNAL)!=4 || recv(fresh,&host_time,8,MSG_WAITALL)!=8) fail("host time");
    uint64_t realtime=ns(CLOCK_REALTIME);
    uint64_t error_ns=realtime>host_time ? realtime-host_time : host_time-realtime;
    printf("CLONE CLOCK error_ns=%llu guest=%llu host=%llu\n",(unsigned long long)error_ns,(unsigned long long)realtime,(unsigned long long)host_time);
    if(error_ns>100000000ULL) fail("wall clock exceeds 100ms");
    close(fresh);
    while(atomic_load(&sequence)<frozen_seq+100 && !atomic_load(&errors)) usleep(1000);
    atomic_store(&idle,1);
    printf("CLONE USERSPACE clock_error_ns=%llu\n",(unsigned long long)error_ns);
    /* Host samples private RSS after ten seconds with this clone alive. */
    sleep(11);
    atomic_store(&stopping,1); pthread_join(thread,NULL);
    if(atomic_load(&errors)) fail("writer after resume");
    sync();
    int fd=open("/data/sequence",O_RDONLY);
    for(unsigned slot=0;slot<64;slot++) {
        uint64_t record[512];
        if(pread(fd,record,sizeof(record),slot*sizeof(record))!=sizeof(record)) fail("readback");
        uint64_t seq=record[0]^0x123456789abcdef0ULL;
        for(unsigned i=0;i<512;i++) if(record[i]!=(seq^(0x123456789abcdef0ULL+i))) fail("record integrity");
    }
    close(fd);
    printf("CLONE PASS seq=%lu cpus=%ld mono_delta_ns=%llu realtime_ns=%llu old=%d SIMD=ok\n",atomic_load(&sequence),sysconf(_SC_NPROCESSORS_ONLN),(unsigned long long)(after-before),(unsigned long long)ns(CLOCK_REALTIME),old_result);
    if(umount("/data")) fail("unmount");
    reboot(RB_POWER_OFF); for(;;) pause();
}
