/* Guest driver-reset regression, retained from the backend review reproduction. */
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/reboot.h>
#include <linux/vm_sockets.h>
#include <unistd.h>
#include <stdio.h>
#include <fcntl.h>
#include <glob.h>
#include <string.h>
#include <errno.h>
static int attempt(void){int s=socket(AF_VSOCK,SOCK_STREAM,0);struct sockaddr_vm a={.svm_family=AF_VSOCK,.svm_cid=2,.svm_port=7000};int r=connect(s,(void*)&a,sizeof(a));int e=errno;close(s);printf("CONNECT result=%d errno=%d\n",r,e);return r;}
static int put(const char*p,const char*s){int f=open(p,O_WRONLY);if(f<0)return -1;int r=write(f,s,strlen(s));close(f);return r<0?-1:0;}
int main(){setbuf(stdout,0);mkdir("/dev",0755);mkdir("/sys",0755);mount("devtmpfs","/dev","devtmpfs",0,0);mount("sysfs","/sys","sysfs",0,0);puts("BEFORE_RESET");if(attempt()==0)puts("PRE_RESET_UNEXPECTED_ALLOW");else puts("PRE_RESET_DENIED");glob_t g={0};glob("/sys/bus/virtio/drivers/vmw_vsock_virtio_transport/virtio*",0,0,&g);if(g.gl_pathc!=1){printf("RESET_FIXTURE_FAIL count=%zu\n",g.gl_pathc);goto done;}char name[64];snprintf(name,sizeof(name),"%s",strrchr(g.gl_pathv[0],'/')+1);if(put("/sys/bus/virtio/drivers/vmw_vsock_virtio_transport/unbind",name)||put("/sys/bus/virtio/drivers/vmw_vsock_virtio_transport/bind",name)){puts("RESET_FIXTURE_FAIL bind");goto done;}usleep(100000);puts("AFTER_RESET");if(attempt()==0)puts("POST_RESET_ALLOWED");else puts("POST_RESET_DENIED");done:sync();reboot(RB_POWER_OFF);for(;;)pause();}
