#define _GNU_SOURCE
#include <errno.h>
#include <glob.h>
#include <fcntl.h>
#include <linux/vm_sockets.h>
#include <linux/capability.h>
#include <linux/userfaultfd.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <netinet/in.h>
#include <pthread.h>
#include <poll.h>
#include <sys/wait.h>
#include <sys/ptrace.h>
#include <sys/user.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static atomic_ulong sequence;
static atomic_int stopping, errors, idle;
static uint64_t ns(clockid_t clock)
{
    struct timespec ts;
    clock_gettime(clock, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + ts.tv_nsec;
}
static void fail(const char *what)
{
    printf("CLONE FAIL %s errno=%d\n", what, errno);
    sync(); reboot(RB_POWER_OFF); for (;;) pause();
}
static int connect_host(void)
{
    int fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    struct sockaddr_vm address = { .svm_family=AF_VSOCK, .svm_cid=2, .svm_port=7000 };
    struct timeval timeout = { .tv_sec=2 };
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout));
    if (fd < 0 || connect(fd, (void *)&address, sizeof(address))) {
        printf("CLONE POLICY denied\n");
        fail("vsock connect");
    }
    return fd;
}
static void echo(int fd)
{
    char reply[4];
    if (send(fd, "echo", 4, MSG_NOSIGNAL) != 4 || recv(fd, reply, 4, MSG_WAITALL) != 4 || memcmp(reply, "echo", 4)) fail("vsock echo");
}
static void *writer(void *unused)
{
    (void)unused;
    int fd = open("/data/sequence", O_CREAT|O_RDWR|O_TRUNC, 0600);
    if (fd < 0) { atomic_fetch_add(&errors, 1); return NULL; }
    while (!atomic_load(&stopping)) {
        if(atomic_load(&idle)) { usleep(10000); continue; }
        unsigned long seq = atomic_fetch_add(&sequence, 1) + 1;
        uint64_t record[512];
        for (unsigned i=0; i<512; i++) record[i] = seq ^ (0x123456789abcdef0ULL + i);
        if (pwrite(fd, record, sizeof(record), (seq % 64) * sizeof(record)) != sizeof(record) || fdatasync(fd)) atomic_fetch_add(&errors, 1);
    }
    close(fd); return NULL;
}
static void *blocked_random_page;
static void *blocked_random_reader(void *unused)
{
    (void)unused;
    int fd=open("/dev/random",O_RDONLY);
    if(fd<0 || read(fd,blocked_random_page,32)!=32) fail("blocked random read");
    close(fd);
    return NULL;
}
static int block_random_read(pthread_t *reader)
{
    int fd=syscall(SYS_userfaultfd,O_CLOEXEC|O_NONBLOCK);
    struct uffdio_api api={.api=UFFD_API};
    if(fd<0 || ioctl(fd,UFFDIO_API,&api)) fail("random userfault api");
    blocked_random_page=mmap(NULL,4096,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    struct uffdio_register reg={.range={.start=(uintptr_t)blocked_random_page,.len=4096},.mode=UFFDIO_REGISTER_MODE_MISSING};
    if(blocked_random_page==MAP_FAILED || ioctl(fd,UFFDIO_REGISTER,&reg) || pthread_create(reader,NULL,blocked_random_reader,NULL)) fail("random userfault register");
    struct pollfd ready={.fd=fd,.events=POLLIN};
    struct uffd_msg event;
    if(poll(&ready,1,3000)!=1 || read(fd,&event,sizeof(event))!=sizeof(event) || event.event!=UFFD_EVENT_PAGEFAULT) fail("random read not blocked");
    printf("CLONE RANDOM reader blocked in copy\n");
    return fd;
}
static void unblock_random_read(int fd,pthread_t reader)
{
    struct uffdio_zeropage zero={.range={.start=(uintptr_t)blocked_random_page,.len=4096}};
    if(ioctl(fd,UFFDIO_ZEROPAGE,&zero)) fail("random fault resolve");
    struct timespec deadline;
    clock_gettime(CLOCK_REALTIME,&deadline); deadline.tv_sec+=3;
    if(pthread_timedjoin_np(reader,NULL,&deadline)) fail("random reader join");
    printf("CLONE BLOCKED_RANDOM ");
    for(unsigned i=0;i<32;i++) printf("%02x",((unsigned char *)blocked_random_page)[i]);
    printf("\n"); close(fd); munmap(blocked_random_page,4096);
}
int main(void)
{
    mkdir("/dev",0755); mkdir("/proc",0755); mkdir("/sys",0755); mkdir("/data",0755);
    mount("devtmpfs","/dev","devtmpfs",0,NULL); mount("proc","/proc","proc",0,NULL); mount("sysfs","/sys","sysfs",0,NULL);
    int console=open("/dev/ttyS0",O_RDWR);
    if(console>=0) { dup2(console,0); dup2(console,1); dup2(console,2); }
    setbuf(stdout,NULL);
    pid_t denied=fork();
    if(denied<0) fail("permission fork");
    if(!denied) {
        if(setuid(65534)) _exit(20);
        int trigger=open("/sys/power/nitro_freeze",O_WRONLY);
        if(trigger>=0) _exit(21);
        _exit(errno==EACCES ? 0 : 22);
    }
    int denied_status;
    if(waitpid(denied,&denied_status,0)!=denied || !WIFEXITED(denied_status) || WEXITSTATUS(denied_status)) fail("non-root trigger accepted");
    pid_t nocap=fork();
    if(nocap<0) fail("capability fork");
    if(!nocap) {
        struct __user_cap_header_struct header={.version=_LINUX_CAPABILITY_VERSION_3,.pid=0};
        struct __user_cap_data_struct caps[2]={{0},{0}};
        if(syscall(SYS_capset,&header,caps)) _exit(23);
        int trigger=open("/sys/power/nitro_freeze",O_WRONLY);
        if(trigger<0) _exit(24);
        ssize_t result=write(trigger,"1",1);
        _exit(result==-1 && errno==EPERM ? 0 : 25);
    }
    if(waitpid(nocap,&denied_status,0)!=nocap || !WIFEXITED(denied_status) || WEXITSTATUS(denied_status)) fail("root without CAP_SYS_ADMIN");
    puts("CLONE CAPABILITY root-without-admin refused");
    glob_t bindings;
    if(glob("/sys/bus/platform/devices/vmgenid*/driver",0,NULL,&bindings) || !bindings.gl_pathc) fail("vmgenid driver binding");
    globfree(&bindings);
    printf("CLONE VMGENID bound\n");
    if(mount("/dev/vda","/data","ext4",0,NULL)) fail("mount");
    int random_pipe[2];
    if(pipe(random_pipe)) fail("random pipe");
    pid_t reader=fork();
    if(reader<0) fail("random fork");
    if(!reader) {
        close(random_pipe[0]);
        int event=open("/sys/power/nitro_freeze",O_RDONLY);
        char state[128];
        unsigned char sample[32];
        if(event<0 || read(event,state,sizeof(state)-1)<=0) _exit(10);
        if(ptrace(PTRACE_TRACEME,0,NULL,NULL)) _exit(15);
        raise(SIGSTOP);
        if(syscall(SYS_getrandom,sample,sizeof(sample),0)!=sizeof(sample)) _exit(11);
        struct pollfd p={.fd=event,.events=POLLPRI};
        if(poll(&p,1,3000)<=0 || !(p.revents&POLLPRI)) _exit(16);
        lseek(event,0,SEEK_SET);
        ssize_t n=read(event,state,sizeof(state)-1);
        if(n<=0) _exit(12);
        state[n]=0;
        if(!strstr(state,"path=3") || !strstr(state,"seq=1")) _exit(13);
        if(write(random_pipe[1],sample,sizeof(sample))!=sizeof(sample)) _exit(14);
        _exit(0);
    }
    close(random_pipe[1]);
    int trace_status;
    if(waitpid(reader,&trace_status,0)!=reader || !WIFSTOPPED(trace_status)) fail("reader trace stop");
    if(ptrace(PTRACE_SETOPTIONS,reader,NULL,(void *)(uintptr_t)PTRACE_O_TRACESYSGOOD)) fail("trace options");
    for(;;) {
        if(ptrace(PTRACE_SYSCALL,reader,NULL,NULL)) fail("trace syscall");
        if(waitpid(reader,&trace_status,0)!=reader || !WIFSTOPPED(trace_status)) fail("reader syscall stop");
        struct user_regs_struct regs;
        if(ptrace(PTRACE_GETREGS,reader,NULL,&regs)) fail("reader regs");
        if(regs.orig_rax==SYS_getrandom && regs.rax==32) break;
    }
    printf("CLONE PENDING_READ held-at-syscall-exit\n");
    int old=connect_host(); echo(old);
    pthread_t thread;
    if(pthread_create(&thread,NULL,writer,NULL)) fail("pthread");
    while(atomic_load(&sequence)<100 && !atomic_load(&errors)) usleep(1000);
    if(atomic_load(&errors)) fail("writer before freeze");
    uint64_t before=ns(CLOCK_MONOTONIC);
    unsigned long frozen_seq=atomic_load(&sequence);
    uint64_t sentinel[4]={0x1020304050607080ULL,0x90a0b0c0d0e0f001ULL,
        0x9182736455463728ULL,0xfedcba9876543210ULL}, restored[4]={0};
    __builtin_cpu_init();
    int wide_simd=__builtin_cpu_supports("avx2");
    pthread_t blocked_reader;
    int blocked_fault=block_random_read(&blocked_reader);
    int freeze=open("/sys/power/nitro_freeze",O_WRONLY);
    printf("CLONE FREEZE seq=%lu mono=%llu realtime_ns=%llu cpus=%ld\n",frozen_seq,(unsigned long long)before,(unsigned long long)ns(CLOCK_REALTIME),sysconf(_SC_NPROCESSORS_ONLN));
    /* Keep xstate live across the syscall rather than relying on compiler spills. */
    long result;
    if(wide_simd) {
        asm volatile("vmovdqu %2, %%ymm15; syscall; vmovdqu %%ymm15, %1"
            : "=a"(result), "=m"(restored) : "m"(sentinel), "0"(1L), "D"((long)freeze), "S"("1"), "d"(1L)
            : "rcx","r11","memory","xmm15");
    } else {
        asm volatile("movdqu %2, %%xmm15; syscall; movdqu %%xmm15, %1"
            : "=a"(result), "=m"(restored) : "m"(sentinel), "0"(1L), "D"((long)freeze), "S"("1"), "d"(1L)
            : "rcx","r11","memory","xmm15");
    }
    if(result!=1 || memcmp(sentinel,restored,wide_simd ? 32 : 16)) fail("freeze or SIMD");
    printf("CLONE VECTOR width=%d preserved\n",wide_simd ? 256 : 128);
    unblock_random_read(blocked_fault,blocked_reader);
    if(ptrace(PTRACE_DETACH,reader,NULL,NULL)) fail("reader detach");
    unsigned char pending_random[32];
    struct pollfd random_ready={.fd=random_pipe[0],.events=POLLIN};
    if(poll(&random_ready,1,3000)<=0 || read(random_pipe[0],pending_random,sizeof(pending_random))!=sizeof(pending_random)) fail("resume event/random reader");
    int reader_status;
    if(waitpid(reader,&reader_status,0)!=reader || !WIFEXITED(reader_status) || WEXITSTATUS(reader_status)) fail("random reader exit");
    close(random_pipe[0]);
    printf("CLONE PENDING_RANDOM ");
    for(unsigned int i=0;i<sizeof(pending_random);i++) printf("%02x",pending_random[i]);
    printf(" EVENT=ok\n");
    unsigned char random_bytes[32];
    int random_fd=open("/dev/hwrng",O_RDONLY);
    if(random_fd<0 || read(random_fd,random_bytes,sizeof(random_bytes))!=sizeof(random_bytes)) fail("entropy restore");
    close(random_fd);
    if(syscall(SYS_getrandom,random_bytes,sizeof(random_bytes),0)!=sizeof(random_bytes)) fail("getrandom restore");
    printf("CLONE RANDOM ");
    for(unsigned int i=0;i<sizeof(random_bytes);i++) printf("%02x",random_bytes[i]);
    printf("\n");
    uint64_t after=ns(CLOCK_MONOTONIC);
    if(after<before || after-before>10000000000ULL) fail("monotonic");
    char byte;
    int old_result=recv(old,&byte,1,0);
    if(old_result>0 || (old_result<0 && errno==EAGAIN)) fail("old vsock not dead");
    close(old);
    int tcp=socket(AF_INET,SOCK_STREAM,0);
    struct sockaddr_in endpoint={.sin_family=AF_INET,.sin_port=htons(18080),.sin_addr.s_addr=htonl(0x7f000001)};
    if(tcp>=0 && !connect(tcp,(void *)&endpoint,sizeof(endpoint))) fail("internet forwarding unexpectedly enabled");
    if(tcp>=0) close(tcp);
    printf("CLONE FORWARDING disabled\n");
    FILE *interrupts=fopen("/proc/interrupts","r");
    char irqline[512];
    unsigned long irq_total=0;
    if(!interrupts) fail("interrupt counters");
    while(fgets(irqline,sizeof(irqline),interrupts)) if(strstr(irqline,"vmgenid")) {
        char *cursor=strchr(irqline,':');
        if(cursor) for(cursor++;*cursor;) {
            char *end;
            unsigned long count=strtoul(cursor,&end,10);
            if(end==cursor) { if(*cursor==' ' || *cursor=='\t') { cursor++; continue; } break; }
            irq_total+=count; cursor=end;
        }
    }
    fclose(interrupts);
    if(!irq_total) fail("vmgenid notification not observed");
    printf("CLONE VMGENID irq=%lu\n",irq_total);
    int fresh=connect_host(); echo(fresh);
    uint64_t host_time;
    if(send(fresh,"time",4,MSG_NOSIGNAL)!=4 || recv(fresh,&host_time,8,MSG_WAITALL)!=8) fail("host time");
    uint64_t realtime=ns(CLOCK_REALTIME);
    uint64_t error_ns=realtime>host_time ? realtime-host_time : host_time-realtime;
    printf("CLONE CLOCK error_ns=%llu guest=%llu host=%llu\n",(unsigned long long)error_ns,(unsigned long long)realtime,(unsigned long long)host_time);
    if(error_ns>100000000ULL) fail("wall clock exceeds 100ms");
    close(fresh);
    while(atomic_load(&sequence)<frozen_seq+100 && !atomic_load(&errors)) usleep(1000);
    atomic_store(&idle,1);
    printf("CLONE USERSPACE clock_error_ns=%llu\n",(unsigned long long)error_ns);
    /* Host samples private RSS after ten seconds with this clone alive. */
    sleep(11);
    atomic_store(&stopping,1); pthread_join(thread,NULL);
    if(atomic_load(&errors)) fail("writer after resume");
    sync();
    int fd=open("/data/sequence",O_RDONLY);
    for(unsigned slot=0;slot<64;slot++) {
        uint64_t record[512];
        if(pread(fd,record,sizeof(record),slot*sizeof(record))!=sizeof(record)) fail("readback");
        uint64_t seq=record[0]^0x123456789abcdef0ULL;
        for(unsigned i=0;i<512;i++) if(record[i]!=(seq^(0x123456789abcdef0ULL+i))) fail("record integrity");
    }
    close(fd);
    printf("CLONE PASS seq=%lu cpus=%ld mono_delta_ns=%llu realtime_ns=%llu old=%d SIMD=ok\n",atomic_load(&sequence),sysconf(_SC_NPROCESSORS_ONLN),(unsigned long long)(after-before),(unsigned long long)ns(CLOCK_REALTIME),old_result);
    if(umount("/data")) fail("unmount");
    reboot(RB_POWER_OFF); for(;;) pause();
}
