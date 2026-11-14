/* Static guest-side acceptance workload; built on the arm64 host. */
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/reboot.h>
#include <linux/vm_sockets.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <stdlib.h>
static void fail(const char *s) { printf("NITRO_SMOKE_FAIL %s errno=%d\n",s,errno); fflush(stdout); sync(); reboot(RB_POWER_OFF); _exit(1); }
int main(void) {
 setbuf(stdout,NULL);
#ifdef NEVER_START
 for (;;) pause();
#endif
 mkdir("/dev",0755); mkdir("/proc",0755); mkdir("/sys",0755); mkdir("/mnt",0755);
 mount("devtmpfs","/dev","devtmpfs",0,NULL); mount("proc","/proc","proc",0,NULL); mount("sysfs","/sys","sysfs",0,NULL);
 for(int n=0; access("/dev/vda",F_OK) && n<500; n++) usleep(10000);
 if(mount("/dev/vda","/mnt","ext4",0,NULL)) fail("mount");
 const char text[]="libkrun-nitro-ext4-verified";
 int f=open("/mnt/verified",O_CREAT|O_TRUNC|O_RDWR,0600); if(f<0) fail("open disk");
 if(write(f,text,sizeof(text))!=sizeof(text)||fsync(f)||lseek(f,0,SEEK_SET)<0) fail("write disk");
 char b[128]={0}; if(read(f,b,sizeof(text))!=sizeof(text)||memcmp(b,text,sizeof(text))) fail("read disk"); close(f);
 printf("NITRO_DISK_OK\n");
 int s=socket(AF_VSOCK,SOCK_STREAM,0); if(s<0) fail("vsock socket");
 struct timeval tv={5,0}; setsockopt(s,SOL_SOCKET,SO_RCVTIMEO,&tv,sizeof(tv)); setsockopt(s,SOL_SOCKET,SO_SNDTIMEO,&tv,sizeof(tv));
 struct sockaddr_vm a={.svm_family=AF_VSOCK,.svm_cid=2,.svm_port=7000};
#ifdef DENY_MAPPED
 if(connect(s,(void*)&a,sizeof(a))==0) fail("policy unexpectedly allowed mapped connection");
 printf("NITRO_POLICY_DENY_OK\n"); close(s); umount("/mnt"); sync(); reboot(RB_POWER_OFF); return 1;
#else
 if(connect(s,(void*)&a,sizeof(a))) fail("vsock connect");
#endif
 if(write(s,text,sizeof(text))!=sizeof(text)) fail("vsock write");
 size_t n=0; while(n<sizeof(text)) { ssize_t r=read(s,b+n,sizeof(text)-n); if(r<=0) fail("vsock read"); n+=r; }
 if(memcmp(b,text,sizeof(text))) fail("vsock data"); close(s); printf("NITRO_VSOCK_OK\n");
 int c=-1; for(int n=0;n<500 && c<0;n++){ c=open("/dev/vport2p0",O_WRONLY|O_NONBLOCK); if(c<0)c=open("/dev/hvc0",O_WRONLY|O_NONBLOCK); if(c<0)usleep(10000); }
 if(c<0) fail("console open"); if(write(c,"NITRO_VIRTIO_CONSOLE_OK\n",23)!=23) fail("console write"); close(c);
 if(umount("/mnt"))fail("unmount"); printf("NITRO_SMOKE_OK\n"); sync();
#ifdef REBOOT_GUEST
 reboot(RB_AUTOBOOT);
#else
 reboot(RB_POWER_OFF);
#endif
 return 1;
}
