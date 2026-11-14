#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/vm_sockets.h>
#include <stdio.h>
#include <poll.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <unistd.h>

static void check(int ok, const char *what) {
    if (!ok) { printf("EMBED_FAIL %s errno=%d\n", what, errno); fflush(stdout); reboot(RB_AUTOBOOT); for (;;) pause(); }
}
static int tcp(int port) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in addr = {.sin_family=AF_INET, .sin_port=htons(port), .sin_addr.s_addr=htonl(INADDR_LOOPBACK)};
    int result = connect(fd, (void *)&addr, sizeof(addr)); close(fd); return result;
}
int main(void) {
    setbuf(stdout, NULL);
    mkdir("/dev",0755); mount("devtmpfs","/dev","devtmpfs",0,NULL);
    mkdir("/proc",0755); mount("proc","/proc","proc",0,NULL);
    char cmdline[4096]={0}; int c=open("/proc/cmdline",O_RDONLY); read(c,cmdline,sizeof(cmdline)-1); close(c);
    puts("EMBED_USERSPACE_OK");
    if (strstr(cmdline,"embed_devices")) {
        char pattern[512], result[512]; memset(pattern,0x5a,sizeof(pattern));
        int fd=open("/dev/vda",O_RDWR); check(fd>=0,"disk open");
        check(pwrite(fd,pattern,512,4096)==512,"disk write"); check(fsync(fd)==0,"disk sync");
        check(pread(fd,result,512,4096)==512 && !memcmp(pattern,result,512),"disk read"); close(fd); puts("EMBED_DISK_OK");
        mkdir("/tree",0755); check(mount("embed_tree","/tree","virtiofs",MS_RDONLY,NULL)==0,"fs mount");
        fd=open("/tree/hello",O_RDONLY); check(fd>=0,"fs open"); memset(result,0,sizeof(result));
        check(read(fd,result,sizeof(result))==13 && !memcmp(result,"embed-fs-data",13),"fs contents"); close(fd); puts("EMBED_FS_OK");
    }
    if (strstr(cmdline,"embed_policy")) {
        check(tcp(18080)==0,"allowed tcp"); check(tcp(18081)<0,"denied tcp"); puts("EMBED_POLICY_OK");
    }
    if (strstr(cmdline,"embed_disabled")) {
        check(tcp(18080)<0,"disabled tcp");
        int fd=socket(AF_VSOCK,SOCK_STREAM,0); struct sockaddr_vm addr={.svm_family=AF_VSOCK,.svm_cid=2,.svm_port=7000};
        check(connect(fd,(void *)&addr,sizeof(addr))==0,"mapped vsock");
        check(write(fd,"ping",4)==4,"vsock send"); char result[4]; struct pollfd ready={.fd=fd,.events=POLLIN}; check(poll(&ready,1,3000)>0,"vsock readiness"); check(read(fd,result,4)==4 && !memcmp(result,"pong",4),"vsock reply"); check(write(fd,"\1",1)==1,"vsock ack"); close(fd); puts("EMBED_DISABLED_OK");
    }
    puts("EMBED_ALL_OK"); reboot(RB_AUTOBOOT); for (;;) pause();
}
