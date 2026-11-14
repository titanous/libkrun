#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/reboot.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdio.h>
#include <sys/socket.h>
#include <linux/vm_sockets.h>
#ifndef REASON
#define REASON 1
#endif
int main(void){setbuf(stdout,NULL);mkdir("/proc",0755);mount("proc","/proc","proc",0,0);puts("TERMINAL_READY");int s=socket(AF_VSOCK,SOCK_STREAM,0);struct sockaddr_vm a={.svm_family=AF_VSOCK,.svm_cid=2,.svm_port=7997};if(connect(s,(void*)&a,sizeof(a))){puts("TERMINAL_HANDSHAKE_FAIL");for(;;)pause();}char go;if(read(s,&go,1)!=1){puts("TERMINAL_HANDSHAKE_FAIL");for(;;)pause();}if(REASON==4){return 1;}else if(REASON==1)reboot(RB_POWER_OFF);else if(REASON==2)reboot(RB_AUTOBOOT);else if(REASON==3)reboot(RB_HALT_SYSTEM);for(;;)pause();}
