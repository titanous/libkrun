/* Port of treefs c70edfb guest test, with finite Nitro poweroff. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <unistd.h>
#ifndef DAX
#define DAX 0
#endif
static void check(int ok,const char *what){if(!ok){printf("TREEFS_FAIL %s errno=%d\n",what,errno);fflush(stdout);reboot(RB_POWER_OFF);for(;;)pause();}}
int main(void){setbuf(stdout,NULL);puts("TREEFS_INIT");mkdir("/dev",0755);mount("devtmpfs","/dev","devtmpfs",0,NULL);mkdir("/ro",0755);mkdir("/workspace",0755);
check(mount("tree_ro","/ro","virtiofs",MS_RDONLY,DAX?"dax=always":NULL)==0,"ro mount");check(mount("tree_rw","/workspace","virtiofs",0,NULL)==0,"rw mount");char buf[32]={0};int fd=open("/ro/hello",O_RDONLY);check(fd>=0&&read(fd,buf,sizeof(buf))==5&&!memcmp(buf,"hello",5),"ro readiness");void *mapped=mmap(NULL,5,PROT_READ,MAP_PRIVATE,fd,0);check(mapped!=MAP_FAILED&&!memcmp(mapped,"hello",5),"mmap read");munmap(mapped,5);close(fd);puts("TREEFS_READ_READY");check(mkdir("/workspace/files",0755)==0,"mkdir");
for(int i=0;i<1000;i++){char path[64];snprintf(path,sizeof(path),"/ro/f%04d",i);fd=open(path,O_RDONLY);memset(buf,0,sizeof(buf));check(fd>=0&&read(fd,buf,sizeof(buf))==5&&!memcmp(buf,"hello",5),"1000 source files");close(fd);}
fd=open("/ro/large",O_RDONLY);check(fd>=0,"large open");unsigned char block[65536];unsigned long offset=0;while(offset<64UL*1024*1024){int n=read(fd,block,sizeof(block));check(n>0,"large read");for(int j=0;j<n;j++)check(block[j]==(offset+j)%251,"large content");offset+=n;}unsigned char *large=mmap(NULL,64UL*1024*1024,PROT_READ,MAP_PRIVATE,fd,0);check(large!=MAP_FAILED,"large mmap");for(unsigned long i=0;i<64UL*1024*1024;i++)check(large[i]==i%251,"large mmap content");munmap(large,64UL*1024*1024);close(fd);puts("TREEFS_LARGE_OK");
for(int i=0;i<3000;i++){char path[80],target[80],data[32];snprintf(path,sizeof(path),"/workspace/files/f%04d",i);snprintf(target,sizeof(target),"/workspace/files/r%04d",i);int n=snprintf(data,sizeof(data),"data-%04d",i);fd=open(path,O_CREAT|O_RDWR|O_EXCL,0644);check(fd>=0&&write(fd,data,n)==n,"create write");check(write(fd,"!",1)==1,"append");if(i%100==0)check(fsync(fd)==0,"file sync");close(fd);check(rename(path,target)==0,"rename");if(i%3==0)check(unlink(target)==0,"unlink");}
check(symlink("../ro/hello","/workspace/link")==0,"symlink");sync();puts("TREEFS_REPLACE_READY");for(int attempt=0;attempt<1000;attempt++){fd=open("/workspace/replaced",O_RDONLY);if(fd>=0){close(fd);break;}usleep(10000);}check(umount("/workspace")==0,"replacement unmount");check(mount("tree_rw","/workspace","virtiofs",0,NULL)==0,"replacement remount");fd=open("/workspace/replaced",O_RDONLY);memset(buf,0,sizeof(buf));check(fd>=0&&read(fd,buf,sizeof(buf))==3&&!memcmp(buf,"new",3),"replacement contents");close(fd);fd=open("/workspace/files/r0001",O_RDONLY);check(fd>=0,"local edit survives replacement");close(fd);puts("TREEFS_ALL_OK");sync();reboot(RB_POWER_OFF);for(;;)pause();}
