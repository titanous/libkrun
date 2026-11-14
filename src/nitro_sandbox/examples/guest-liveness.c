#include <sys/socket.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <linux/vm_sockets.h>
#include <unistd.h>
#include <stdio.h>
#include <fcntl.h>
#include <string.h>
#ifndef MODE
#define MODE 0
#endif
int main(void) {
    setbuf(stdout, NULL);
    mkdir("/dev",0755); mkdir("/proc",0755);
    mount("devtmpfs","/dev","devtmpfs",0,0);
    mount("proc","/proc","proc",0,0);
    puts("LIVENESS_GUEST_READY");
    if (MODE == 1) {
        int s = socket(AF_VSOCK, SOCK_STREAM, 0);
        struct sockaddr_vm a = {.svm_family=AF_VSOCK,.svm_cid=2,.svm_port=7999};
        if (connect(s,(void*)&a,sizeof(a))) { puts("HEARTBEAT_CONNECT_FAIL"); }
        else { for(int i=0;i<10;i++){ write(s,"H",1); usleep(200000); } puts("HEARTBEAT_SENT"); }
    } else if (MODE == 2) {
        int f=open("/proc/sys/kernel/sysrq",O_WRONLY); if(f>=0){write(f,"1",1);close(f);}
        f=open("/proc/sysrq-trigger",O_WRONLY); if(f>=0){write(f,"c",1);close(f);}
        puts("PANIC_TRIGGER_FAILED");
    } else if (MODE == 3) {
        char bytes[4096]; memset(bytes,'x',sizeof(bytes));
        for(int i=0;i<4096;i++) write(1,bytes,sizeof(bytes));
        puts("FLOOD_DONE");
    }
    for (;;) pause();
}
