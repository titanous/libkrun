#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/reboot.h>
#include <unistd.h>
static int ro,rw;
static atomic_int stopped,errors;
static void fail(const char *s){printf("CLONE FS FAIL %s errno=%d\n",s,errno);reboot(RB_POWER_OFF);for(;;)pause();}
static void *reader(void *unused){(void)unused;char b[32];while(!atomic_load(&stopped)){ssize_t n=pread(ro,b,sizeof(b),0);if(n<0&&errno==EAGAIN)continue;if(n!=sizeof(b)||b[0]!='A')atomic_fetch_add(&errors,1);}return NULL;}
int main(void){
 mkdir("/dev",0755);mkdir("/proc",0755);mkdir("/sys",0755);mkdir("/ro",0755);mkdir("/rw",0755);
 mount("devtmpfs","/dev","devtmpfs",0,NULL);mount("proc","/proc","proc",0,NULL);mount("sysfs","/sys","sysfs",0,NULL);
 int console=open("/dev/ttyS0",O_RDWR);if(console>=0){dup2(console,0);dup2(console,1);dup2(console,2);}setbuf(stdout,NULL);
 char command[1024]={0};int cmd=open("/proc/cmdline",O_RDONLY);read(cmd,command,sizeof(command)-1);close(cmd);
 const char *options=strstr(command,"clone_dax")?"dax=always":"dax=never";
 if(mount("clone_ro","/ro","virtiofs",MS_RDONLY,options)||mount("clone_rw","/rw","virtiofs",0,options))fail("mount");
 ro=open("/ro/file",O_RDONLY);rw=open("/rw/file",O_RDWR);if(ro<0||rw<0)fail("open");
 char *mapping=mmap(NULL,4096,PROT_READ,MAP_SHARED,ro,0);if(mapping==MAP_FAILED||mapping[0]!='A')fail("mmap before");
 int watch=inotify_init1(IN_NONBLOCK);if(watch<0||inotify_add_watch(watch,"/rw/file",IN_MODIFY)<0)fail("watch");
 pthread_t thread;if(pthread_create(&thread,NULL,reader,NULL))fail("reader");
 int freeze=open("/sys/power/nitro_freeze",O_WRONLY);if(freeze<0)fail("trigger");
 puts("CLONE FS FREEZE");if(write(freeze,"1",1)!=1)fail("freeze");close(freeze);
 char b[32];if(pread(ro,b,sizeof(b),0)!=sizeof(b)||b[0]!='A'||pread(rw,b,sizeof(b),0)!=sizeof(b)||b[0]!='A'||mapping[0]!='A')fail("open content after");
 if(pwrite(rw,"Z",1,0)!=1)fail("write after");
 char events[512];int notified=0;for(int i=0;i<1000;i++){ssize_t n=read(watch,events,sizeof(events));if(n>0){for(char *p=events;p<events+n;){struct inotify_event *e=(void *)p;if(e->mask&IN_MODIFY)notified=1;if(e->mask&IN_Q_OVERFLOW)fail("overflow");p+=sizeof(*e)+e->len;}if(notified)break;}usleep(1000);}
 if(!notified)fail("inotify after");
 atomic_store(&stopped,1);pthread_join(thread,NULL);if(atomic_load(&errors))fail("reader content");
 munmap(mapping,4096);close(ro);close(rw);close(watch);
 puts("CLONE FS PASS open=ok reader=ok mmap=ok inotify=ok");reboot(RB_POWER_OFF);for(;;)pause();
}
