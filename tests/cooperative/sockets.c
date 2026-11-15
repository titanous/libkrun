/* /init for the forwarded-socket copy test (tests/cooperative/sockets.py).
 *
 * Before the freeze it leaves forwarded sockets in each state: a connected TCP stream, a
 * pending non-blocking TCP connect, a thread inside a blocking TCP connect, a connected UDP
 * socket, an unconnected UDP socket that has sent, a thread blocked in a UDP receive, and a
 * native guest-loopback TCP pair with its listener. In the copy, each forwarded socket must
 * fail definitely on its next use (never hang, never drop silently) and recover when used
 * afresh; the native sockets must keep working. The host side runs servers on its own
 * interface address (/address): TCP echo 29101, TCP 29102 whose accept queue is full (SYNs
 * dropped), UDP echo 29103.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <net/if.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <unistd.h>

/* Below the usual ephemeral range, so other connections on the host do not hold them. */
#define TCP_ECHO 29101
#define TCP_HELD 29102
#define UDP_ECHO 29103
#define NATIVE 29200

static int failures;
static int udp_retries; /* reported: datagram loss is visible, not hidden by the retry */

static void check(int ok, const char *what, int err)
{
	printf("%s copy-socket %s errno=%d\n", ok ? "PASS" : "FAIL", what, err);
	if (!ok)
		failures++;
}

/* The host's own interface address (from /address): a UDP connect to guest loopback would
 * stay guest-local, so the host servers are reached at this address. */
static struct in_addr host;

/* NATIVE is a guest-loopback port; every other port is a host server. */
static struct sockaddr_in loopback(int port)
{
	struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(port) };

	if (port == NATIVE)
		a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
	else
		a.sin_addr = host;
	return a;
}

static int tcp_connect(int port, int nonblocking, int *err)
{
	struct sockaddr_in a = loopback(port);
	int s = socket(AF_INET, SOCK_STREAM | (nonblocking ? SOCK_NONBLOCK : 0), 0);

	*err = connect(s, (struct sockaddr *)&a, sizeof(a)) ? errno : 0;
	return s;
}

static int exchange(int s, const char *text)
{
	char buffer[64] = { 0 };
	struct pollfd p = { .fd = s, .events = POLLIN };
	size_t n = strlen(text);

	if (send(s, text, n, MSG_NOSIGNAL) != (ssize_t)n)
		return 0;
	if (poll(&p, 1, 5000) <= 0)
		return 0;
	return recv(s, buffer, sizeof(buffer), 0) == (ssize_t)n && !memcmp(buffer, text, n);
}

static int udp_socket(int connect_to)
{
	int s = socket(AF_INET, SOCK_DGRAM, 0);
	struct sockaddr_in a = loopback(UDP_ECHO);

	if (connect_to && connect(s, (struct sockaddr *)&a, sizeof(a)))
		return -1;
	return s;
}

/* UDP may drop a datagram: up to three tries. addressed: sendto, otherwise send. */
static int udp_echo(int s, const char *text, int addressed)
{
	struct sockaddr_in a = loopback(UDP_ECHO);
	char buffer[64] = { 0 };
	struct pollfd p = { .fd = s, .events = POLLIN };
	size_t n = strlen(text);

	for (int try = 0; try < 3; try++) {
		ssize_t sent = addressed ? sendto(s, text, n, 0, (struct sockaddr *)&a, sizeof(a))
					 : send(s, text, n, 0);

		if (sent != (ssize_t)n)
			return 0;
		if (poll(&p, 1, 2000) > 0)
			return recv(s, buffer, sizeof(buffer), 0) == (ssize_t)n &&
			       !memcmp(buffer, text, n);
		udp_retries++;
	}
	return 0;
}

static int udp_exchange_to(int s, const char *text)
{
	return udp_echo(s, text, 1);
}

/* Discard late duplicate replies of retried datagrams. */
static void udp_drain(int s)
{
	char buffer[64];
	struct pollfd p = { .fd = s, .events = POLLIN };

	while (poll(&p, 1, 500) > 0 && recv(s, buffer, sizeof(buffer), MSG_DONTWAIT) > 0)
		;
}

/* One message each way over a guest-local pair. */
static int roundtrip(int a, int b, const char *text)
{
	char buffer[64] = { 0 };
	struct pollfd p = { .fd = b, .events = POLLIN };
	size_t n = strlen(text);

	if (send(a, text, n, MSG_NOSIGNAL) != (ssize_t)n || poll(&p, 1, 5000) <= 0 ||
	    recv(b, buffer, sizeof(buffer), 0) != (ssize_t)n)
		return 0;
	p.fd = a;
	return send(b, buffer, n, MSG_NOSIGNAL) == (ssize_t)n && poll(&p, 1, 5000) > 0 &&
	       recv(a, buffer, sizeof(buffer), 0) == (ssize_t)n && !memcmp(buffer, text, n);
}

/* A thread that blocks in one call and reports through a pipe when it returns. */
struct waiter {
	int fd, pipe[2], ret, err, kind;
};

static void *wait_call(void *arg)
{
	struct waiter *w = arg;
	char buffer[64];

	if (w->kind == 0) {
		struct sockaddr_in a = loopback(TCP_HELD);

		w->ret = connect(w->fd, (struct sockaddr *)&a, sizeof(a));
	} else {
		w->ret = recv(w->fd, buffer, sizeof(buffer), 0);
	}
	w->err = w->ret < 0 ? errno : 0;
	if (write(w->pipe[1], "x", 1) != 1)
		abort();
	return NULL;
}

static void start_waiter(struct waiter *w, int fd, int kind)
{
	pthread_t thread;

	w->fd = fd;
	w->kind = kind;
	if (pipe(w->pipe) || pthread_create(&thread, NULL, wait_call, w))
		abort();
	pthread_detach(thread);
}

static int waiter_done(struct waiter *w, int ms)
{
	struct pollfd p = { .fd = w->pipe[0], .events = POLLIN };

	return poll(&p, 1, ms) > 0;
}

static void power_off(void)
{
	printf("COPY-SOCKETS udp_retries=%d\n", udp_retries);
	printf("COPY-SOCKETS failures=%d\n", failures);
	sync();
	reboot(RB_POWER_OFF);
	for (;;)
		pause();
}

int main(void)
{
	struct ifreq lo = { .ifr_name = "lo" };
	struct sockaddr_in native = loopback(NATIVE);
	struct waiter connect_waiter, recv_waiter;
	int conn, pending, udp_conn, udp_free, udp_wait, listener, client, server, freeze, err;
	char flag = 'N';
	int fd, one = 1;

	mkdir("/dev", 0755);
	mkdir("/proc", 0755);
	mkdir("/sys", 0755);
	mount("devtmpfs", "/dev", "devtmpfs", 0, NULL);
	mount("proc", "/proc", "proc", 0, NULL);
	mount("sysfs", "/sys", "sysfs", 0, NULL);
	fd = open("/dev/ttyS0", O_RDWR);
	if (fd >= 0) {
		dup2(fd, 0);
		dup2(fd, 1);
		dup2(fd, 2);
	}
	setvbuf(stdout, NULL, _IOLBF, 0);
	fd = socket(AF_INET, SOCK_DGRAM, 0);
	if (ioctl(fd, SIOCGIFFLAGS, &lo) == 0) {
		lo.ifr_flags |= IFF_UP;
		ioctl(fd, SIOCSIFFLAGS, &lo);
	}
	close(fd);
	{
		char text[64] = { 0 };

		fd = open("/address", O_RDONLY);
		if (fd < 0 || read(fd, text, sizeof(text) - 1) <= 0 ||
		    inet_pton(AF_INET, strtok(text, "\n"), &host) != 1) {
			check(0, "host address file", errno);
			power_off();
		}
		close(fd);
	}
	fd = open("/sys/module/kernel/parameters/tsi_hijack", O_RDONLY);
	if (fd < 0 || read(fd, &flag, 1) != 1 || flag != 'Y') {
		check(0, "socket forwarding enabled", errno);
		power_off();
	}
	close(fd);

	/* Before the freeze: every state, each checked to work. */
	conn = tcp_connect(TCP_ECHO, 0, &err);
	check(!err && exchange(conn, "before"), "before: forwarded TCP exchange", err);
	pending = tcp_connect(TCP_HELD, 1, &err);
	check(err == EINPROGRESS, "before: forwarded connect pending", err);
	udp_conn = udp_socket(1);
	check(udp_conn >= 0 && udp_echo(udp_conn, "udp", 0), "before: connected UDP exchange", errno);
	udp_drain(udp_conn);
	udp_free = udp_socket(0);
	check(udp_exchange_to(udp_free, "free"), "before: unconnected UDP exchange", errno);
	udp_wait = udp_socket(1);
	check(udp_wait >= 0 && udp_echo(udp_wait, "wait", 0), "before: UDP receiver exchange", errno);
	udp_drain(udp_wait);
	udp_drain(udp_free);
	listener = socket(AF_INET, SOCK_STREAM, 0);
	setsockopt(listener, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
	err = bind(listener, (struct sockaddr *)&native, sizeof(native)) || listen(listener, 8);
	client = tcp_connect(NATIVE, 0, &err);
	server = accept(listener, NULL, NULL);
	check(!err && server >= 0 && roundtrip(client, server, "native"),
	      "before: native loopback pair", err);
	start_waiter(&connect_waiter, socket(AF_INET, SOCK_STREAM, 0), 0);
	start_waiter(&recv_waiter, udp_wait, 1);
	usleep(500000);
	check(!waiter_done(&connect_waiter, 0), "before: blocking connect is waiting", 0);
	check(!waiter_done(&recv_waiter, 0), "before: UDP receive is waiting", 0);
	puts("COPY-SOCKETS READY");

	/* Freeze. The source VMM ends here; the copy returns from the write. */
	freeze = open("/sys/power/nitro_freeze", O_WRONLY);
	for (int i = 0; i < 1000; i++) {
		if (write(freeze, "1", 1) == 1)
			break;
		usleep(10000);
	}
	puts("COPY-SOCKETS RESUMED");

	/* Pending non-blocking connect: a definite error, soon. */
	{
		struct pollfd p = { .fd = pending, .events = POLLOUT };
		int ready = poll(&p, 1, 5000), so_error = 0;
		socklen_t len = sizeof(so_error);

		getsockopt(pending, SOL_SOCKET, SO_ERROR, &so_error, &len);
		check(ready > 0 && so_error != 0, "copy: pending connect fails", so_error);
	}
	/* The thread inside a blocking connect: returns with a definite error. */
	check(waiter_done(&connect_waiter, 5000) && connect_waiter.ret == -1 &&
		      connect_waiter.err != EINTR && connect_waiter.err != EINPROGRESS,
	      "copy: blocking connect returns an error", connect_waiter.err);
	/* The thread blocked in a UDP receive: returns, no hang. */
	check(waiter_done(&recv_waiter, 5000) && recv_waiter.ret == -1, "copy: blocked UDP receive returns",
	      recv_waiter.err);
	/* Connected forwarded TCP: definite error on use. */
	{
		struct pollfd p = { .fd = conn, .events = POLLIN };
		char buffer[8];
		int ready = poll(&p, 1, 3000);
		ssize_t got = recv(conn, buffer, sizeof(buffer), MSG_DONTWAIT);

		check(ready > 0 && got <= 0 && (got == 0 || errno != EAGAIN),
		      "copy: connected TCP reports the lost connection", got < 0 ? errno : 0);
	}
	/* Connected UDP: send fails until reconnected, then works. */
	{
		ssize_t sent = send(udp_conn, "lost", 4, 0);
		struct sockaddr_in a = loopback(UDP_ECHO);

		check(sent == -1, "copy: connected UDP send fails", sent < 0 ? errno : 0);
		err = connect(udp_conn, (struct sockaddr *)&a, sizeof(a)) ? errno : 0;
		check(!err && udp_echo(udp_conn, "again", 0), "copy: connected UDP works after reconnect", err);
	}
	/* Unconnected UDP: the first send reports the loss, the next works. */
	{
		struct sockaddr_in a = loopback(UDP_ECHO);
		ssize_t sent = sendto(udp_free, "lost", 4, 0, (struct sockaddr *)&a, sizeof(a));

		check(sent == -1, "copy: unconnected UDP first send fails", sent < 0 ? errno : 0);
		check(udp_exchange_to(udp_free, "next"), "copy: unconnected UDP next send works", errno);
	}
	/* Native sockets keep working. */
	check(roundtrip(client, server, "native"), "copy: native pair works", errno);
	{
		int c = tcp_connect(NATIVE, 0, &err), s = accept(listener, NULL, NULL);

		check(!err && s >= 0 && roundtrip(c, s, "fresh"), "copy: native listener accepts", err);
	}
	/* A new forwarded connection works. */
	{
		int c = tcp_connect(TCP_ECHO, 0, &err);

		check(!err && exchange(c, "fresh"), "copy: new forwarded TCP works", err);
	}
	power_off();
}
