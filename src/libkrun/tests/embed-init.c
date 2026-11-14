#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/vm_sockets.h>
#include <stdio.h>
#include <poll.h>
#include <net/if.h>
#include <sys/ioctl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/stat.h>
#include <unistd.h>

static void check(int ok, const char *what) {
    if (!ok) { printf("EMBED_FAIL %s errno=%d\n", what, errno); fflush(stdout); reboot(RB_AUTOBOOT); for (;;) pause(); }
}
static int tcp(int port) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in addr = {.sin_family=AF_INET, .sin_port=htons(port), .sin_addr.s_addr=htonl(INADDR_LOOPBACK)};
    int result = connect(fd, (void *)&addr, sizeof(addr)); int error=errno; close(fd); errno=error; return result;
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
    if (strstr(cmdline,"embed_deny_all")) { check(tcp(18080)<0 && errno==EACCES,"backend enforced rule"); puts("EMBED_DENY_ALL_OK"); }
    if (strstr(cmdline,"embed_policy")) {
        check(tcp(18080)==0,"allowed tcp"); check(tcp(18081)<0 && errno==EACCES,"denied tcp EACCES");
        int mapped=socket(AF_INET6,SOCK_STREAM,0); struct sockaddr_in6 mappedaddr={.sin6_family=AF_INET6,.sin6_port=htons(18081)}; inet_pton(AF_INET6,"::ffff:127.0.0.1",&mappedaddr.sin6_addr);
        check(connect(mapped,(void *)&mappedaddr,sizeof(mappedaddr))<0 && errno==EACCES,"mapped IPv4 denial"); close(mapped);
        int denied=socket(AF_VSOCK,SOCK_STREAM,0); struct sockaddr_vm addr={.svm_family=AF_VSOCK,.svm_cid=2,.svm_port=7001};
        check(connect(denied,(void *)&addr,sizeof(addr))<0 && errno==ECONNRESET,"denied mapped ECONNRESET"); close(denied); puts("EMBED_POLICY_OK");
    }
    if (strstr(cmdline,"embed_dgram_families")) {
        int fd=socket(AF_INET6,SOCK_DGRAM,0); struct sockaddr_in6 addr={.sin6_family=AF_INET6,.sin6_port=htons(18083),.sin6_addr=IN6ADDR_LOOPBACK_INIT};
        check(sendto(fd,"ipv6",4,0,(void *)&addr,sizeof(addr))==4,"unbound IPv6 send"); close(fd);
        puts("EMBED_DGRAM_FAMILIES_OK");
    }
    if (strstr(cmdline,"embed_fs_error")) {
        mkdir("/bad",0755); int mounted=mount("embed_bad","/bad","virtiofs",MS_RDONLY,NULL);
        if (mounted==0) { int fd=open("/bad/hello",O_RDONLY); check(fd<0 && errno==ECONNREFUSED,"failed INIT rejects access"); }
        else check(errno==EIO || errno==ECONNREFUSED,"failed INIT mount error");
        puts("EMBED_FS_ERROR_OK");
    }
    if (strstr(cmdline,"embed_udp")) {
        int fd=socket(AF_INET,SOCK_DGRAM,0); struct sockaddr_in addr={.sin_family=AF_INET,.sin_port=htons(18082),.sin_addr.s_addr=htonl(INADDR_LOOPBACK)};
        check(sendto(fd,"allowed",7,0,(void *)&addr,sizeof(addr))==7,"allowed UDP send");
        struct pollfd ready={.fd=fd,.events=POLLIN}; check(poll(&ready,1,3000)>0,"allowed UDP reply readiness"); char reply[8]; check(recv(fd,reply,sizeof(reply),0)==5 && !memcmp(reply,"reply",5),"allowed UDP reply"); close(fd);
        fd=socket(AF_INET,SOCK_DGRAM,0); addr.sin_port=htons(18081);
        check(connect(fd,(void *)&addr,sizeof(addr))<0 && errno==EACCES,"denied UDP connect EACCES"); close(fd);
        fd=socket(AF_INET,SOCK_DGRAM,0); check(sendto(fd,"denied",6,0,(void *)&addr,sizeof(addr))==6,"asynchronous denied UDP send"); close(fd); puts("EMBED_UDP_OK");
    }
    if (strstr(cmdline,"embed_disabled")) {
        check(tcp(18080)<0,"disabled tcp");
        int ctl=socket(AF_INET,SOCK_DGRAM,0); struct ifreq iface={0}; strcpy(iface.ifr_name,"lo"); iface.ifr_flags=IFF_UP|IFF_RUNNING; check(ioctl(ctl,SIOCSIFFLAGS,&iface)==0,"loopback up"); close(ctl);
        int local=socket(AF_INET,SOCK_STREAM,0); struct sockaddr_in loop={.sin_family=AF_INET,.sin_port=htons(19090),.sin_addr.s_addr=htonl(INADDR_LOOPBACK)};
        check(bind(local,(void *)&loop,sizeof(loop))==0 && listen(local,1)==0,"guest local listen");
        check(tcp(19090)==0,"guest local connect"); int accepted=accept(local,NULL,NULL); check(accepted>=0,"guest local accept"); close(accepted); close(local); puts("EMBED_LOCAL_OK");
        int fd=socket(AF_VSOCK,SOCK_STREAM,0); struct sockaddr_vm addr={.svm_family=AF_VSOCK,.svm_cid=2,.svm_port=7000};
        check(connect(fd,(void *)&addr,sizeof(addr))==0,"mapped vsock");
        check(write(fd,"ping",4)==4,"vsock send"); char result[4]; struct pollfd ready={.fd=fd,.events=POLLIN}; check(poll(&ready,1,3000)>0,"vsock readiness"); check(read(fd,result,4)==4 && !memcmp(result,"pong",4),"vsock reply"); check(write(fd,"\1",1)==1,"vsock ack"); close(fd); puts("EMBED_DISABLED_OK");
    }
    puts("EMBED_ALL_OK"); reboot(RB_AUTOBOOT); for (;;) pause();
}
