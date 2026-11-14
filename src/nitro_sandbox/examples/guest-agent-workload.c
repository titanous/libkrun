#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/vm_sockets.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
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
int main(int argc, char **argv) {
    (void)argv;
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
    int old=connect_host();
    dprintf(old,"READY nonce=%016lx monotonic=%lu realtime=%lu\n",nonce,monotonic,realtime);
    char byte;
    ssize_t result=read(old,&byte,1);
    int read_errno=errno;
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
    size_t bad=0;
    for(size_t i=0;i<count;i++) if(memory[i]!=(nonce^(i*0x9e3779b97f4a7c15ULL))) bad++;
    if(getrandom(&fresh,sizeof(fresh),0)!=sizeof(fresh)) return 24;
    int device=open("/dev/vsock",O_RDONLY|O_CLOEXEC);
    unsigned int cid=0;
    if(device<0 || ioctl(device,IOCTL_VM_SOCKETS_GET_LOCAL_CID,&cid)) return 25;
    close(device);
    int report=connect_host();
    dprintf(report,"VERIFIED bad=%zu device_bad=%d nonce=%016lx random=%016lx cid=%u old_result=%ld old_errno=%d monotonic_before=%lu monotonic_after=%lu realtime_before=%lu realtime_after=%lu\n",bad,device_bad,nonce,fresh,cid,(long)result,read_errno,monotonic,ns(CLOCK_MONOTONIC),realtime,ns(CLOCK_REALTIME));
    close(report);
    return bad || device_bad || result>0 ? 26 : 0;
}
