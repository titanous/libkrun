#include <stdint.h>
#include <sys/wait.h>
#include <time.h>

/* One application write per payload; transport fragmentation must be lossless. */
static void large_write(int fd, size_t size, unsigned seed) {
    unsigned char *buf=malloc(size); check(buf!=NULL,"large malloc");
    for (size_t i=0;i<size;i++) buf[i]=(unsigned char)((i*31+seed)%251);
    ssize_t written=write(fd,buf,size);
    printf("LARGE_WRITE wanted=%zu written=%zd errno=%d\n",size,written,errno);
    check(written==(ssize_t)size,"large single write"); free(buf);
}
static void large_read(int fd, size_t size, unsigned seed, int slow) {
    unsigned char buf[16384]; size_t offset=0; uint64_t sum=0;
    while (offset<size) {
        size_t want=size-offset; if (want>sizeof(buf)) want=sizeof(buf);
        ssize_t n=read(fd,buf,want); check(n>0,"large read");
        for (ssize_t j=0;j<n;j++) {
            check(buf[j]==(unsigned char)(((offset+(size_t)j)*31+seed)%251),"large content");
            sum+=buf[j];
        }
        offset+=(size_t)n;
        if (slow) usleep(100);
    }
    printf("LARGE_READ bytes=%zu checksum=%llu\n",size,(unsigned long long)sum);
}
static void large_stream(size_t size, int direction, int slow, unsigned seed) {
    int fd=socket(AF_VSOCK,SOCK_STREAM,0); check(fd>=0,"large socket");
    struct sockaddr_vm addr={.svm_family=AF_VSOCK,.svm_cid=2,.svm_port=7000};
    check(connect(fd,(void *)&addr,sizeof(addr))==0,"large connect");
    struct timeval timeout={.tv_sec=30};
    setsockopt(fd,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout));
    setsockopt(fd,SOL_SOCKET,SO_SNDTIMEO,&timeout,sizeof(timeout));
    check(write(fd,&seed,sizeof(seed))==sizeof(seed),"large seed");
    pid_t writer=-1;
    if (direction==2) { writer=fork(); check(writer>=0,"large fork writer"); if (!writer) { large_write(fd,size,seed); _exit(0); } }
    if (direction==0) large_write(fd,size,seed);
    else large_read(fd,size,seed+17,slow);
    if (writer>0) { int status; check(waitpid(writer,&status,0)==writer && status==0,"large writer status"); }
    /* Interleave small messages after a large payload on the same stream. */
    for (int i=0;i<128;i++) {
        unsigned char b=(unsigned char)i, reply;
        check(write(fd,&b,1)==1,"small write");
        check(read(fd,&reply,1)==1 && reply==b,"small reply");
    }
    check(write(fd,"!",1)==1,"large ack"); close(fd);
}
static void large_test(const char *mode) {
    size_t size; int direction,streams,slow;
    check(sscanf(mode,"embed_large_%zu_%d_%d_%d",&size,&direction,&streams,&slow)==4,"large mode");
    pid_t small=fork(); check(small>=0,"small fork");
    if (!small) {
        int fd=socket(AF_VSOCK,SOCK_STREAM,0); struct sockaddr_vm addr={.svm_family=AF_VSOCK,.svm_cid=2,.svm_port=7000};
        check(connect(fd,(void *)&addr,sizeof(addr))==0,"small connect");
        unsigned seed=0; check(write(fd,&seed,4)==4,"small seed");
        for (unsigned i=0;i<4096;i++) { unsigned char b=(unsigned char)i,reply; check(write(fd,&b,1)==1 && read(fd,&reply,1)==1 && b==reply,"interleaved small"); }
        check(write(fd,"!",1)==1,"small ack"); close(fd); _exit(0);
    }
    for (int i=0;i<streams;i++) {
        pid_t child=fork(); check(child>=0,"large fork stream");
        if (!child) { large_stream(size,direction,slow,(unsigned)i+1); _exit(0); }
    }
    for (int i=0;i<streams+1;i++) { int status; check(wait(&status)>0 && status==0,"large stream status"); }
    puts("EMBED_LARGE_OK");
}
