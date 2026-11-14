#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/reboot.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <sys/mman.h>
#ifndef EXTENDED
#define EXTENDED 0
#endif
#ifndef DAX
#define DAX 0
#endif
int main(void) {
    setbuf(stdout,NULL); mkdir("/dev",0755); mkdir("/mnt",0755);
    mount("devtmpfs","/dev","devtmpfs",0,0);
    if(mount("testfs","/mnt","virtiofs",0,DAX ? "dax=always" : NULL)){printf("FS_FAIL mount errno=%d\n",errno);goto done;}
    if(EXTENDED){
        char path[64],expected[32],data[65536];
        for(int i=0;i<1000;i++){snprintf(path,sizeof(path),"/mnt/f%04d",i);snprintf(expected,sizeof(expected),"file-%04d-bytes!!",i);int fd=open(path,O_RDONLY);if(fd<0||read(fd,data,16)!=16||memcmp(data,expected,16)){puts("FS_FAIL thousand");goto done;}close(fd);}
        int fd=open("/mnt/large",O_RDONLY); if(fd<0){puts("FS_FAIL large-open");goto done;}
        unsigned long offset=0;while(offset<64UL*1024*1024){int n=read(fd,data,sizeof(data));if(n<=0){puts("FS_FAIL large-read");goto done;}for(int j=0;j<n;j++)if((unsigned char)data[j]!=(offset+j)%251){puts("FS_FAIL large-content");goto done;}offset+=n;}
        if(DAX){unsigned char *p=mmap(NULL,64UL*1024*1024,PROT_READ,MAP_PRIVATE,fd,0);if(p==MAP_FAILED){puts("FS_FAIL large-mmap");goto done;}for(unsigned long i=0;i<64UL*1024*1024;i++)if(p[i]!=i%251){puts("FS_FAIL dax-content");goto done;}munmap(p,64UL*1024*1024);puts("NITRO_DAX_OK");}close(fd);
        fd=open("/mnt/new",O_CREAT|O_RDWR|O_EXCL,0644);if(fd<0||write(fd,"written",7)!=7){puts("FS_FAIL create-write");goto done;}close(fd);
        if(rename("/mnt/new","/mnt/renamed")){puts("FS_FAIL rename");goto done;}fd=open("/mnt/renamed",O_RDONLY);if(fd<0||read(fd,data,7)!=7||memcmp(data,"written",7)){puts("FS_FAIL written-content");goto done;}close(fd);if(unlink("/mnt/renamed")){puts("FS_FAIL unlink");goto done;}
        puts("NITRO_FS_EXTENDED_OK");puts("NITRO_FS_OK");umount("/mnt");goto done;
    }
    int f=open("/mnt/hello",O_RDONLY); char bytes[32]={0};
    if(f<0 || read(f,bytes,13)!=13 || memcmp(bytes,"embed-fs-data",13)){puts("FS_FAIL content");goto done;}
    if(DAX){void *p=mmap(NULL,13,PROT_READ,MAP_PRIVATE,f,0); if(p==MAP_FAILED || memcmp(p,"embed-fs-data",13)){puts("FS_FAIL mmap");goto done;} munmap(p,13); puts("NITRO_DAX_OK");}
    close(f); puts("NITRO_FS_OK"); umount("/mnt");
    done: sync(); reboot(RB_POWER_OFF); for(;;)pause();
}
