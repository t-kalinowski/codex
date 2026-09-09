#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/wait.h>
#ifdef __APPLE__
#include <libproc.h>
#include <sys/event.h>
#define NEXT(name) name
#else
#define NEXT(name) dlsym(RTLD_NEXT, #name)
#endif

__attribute__((constructor)) static void target_loader(void) {
    const char *marker = getenv("SANDBOX_TEST_LOADER_MARKER");
    if (!marker) return;
    int fd = open(marker, O_WRONLY | O_CREAT, 0600);
    if (fd >= 0) { close(fd); _exit(120); }
    if (errno != EACCES && errno != EPERM && errno != EROFS) _exit(121);
    const char message[] = "loader restricted\n";
    if (write(1, message, sizeof(message) - 1) != sizeof(message) - 1) _exit(122);
}

static void checkpoint(pid_t pid) {
    const char *event = getenv("SANDBOX_TEST_EVENT_FD");
    if (!event) return;
    int fd = atoi(event);
    if (write(fd, &pid, sizeof(pid)) != sizeof(pid)) _exit(125);
    char byte;
    const char *release = getenv("SANDBOX_TEST_RELEASE_FD");
    if (release && read(atoi(release), &byte, 1) != 1) _exit(125);
}

static pid_t native_pid;
static pid_t native_root;
static int native_fd = -1;
static int fork_child;

static pid_t observed_fork(void) {
    pid_t (*real)(void) = NEXT(fork);
    pid_t pid = real();
    if (pid == 0) {
        fork_child = 1;
        const char *stage = getenv("SANDBOX_TEST_CHILD_STAGE");
        if (stage && strcmp(stage, "before") == 0) checkpoint(getpid());
    }
    if (pid > 0) native_root = native_pid = pid;
    if (pid > 0 && getenv("SANDBOX_TEST_GATE_SPAWN")) checkpoint(pid);
    return pid;
}

static int observed_setpgid(pid_t pid, pid_t group) {
    int (*real)(pid_t, pid_t) = NEXT(setpgid);
    int result = real(pid, group);
    const char *stage = getenv("SANDBOX_TEST_CHILD_STAGE");
    if (result == 0 && fork_child && stage && strcmp(stage, "armed") == 0)
        checkpoint(getpid());
    return result;
}

static void ready(int fd, pid_t pid) {
    native_fd = fd;
    native_pid = pid;
    const char *stage = getenv("SANDBOX_TEST_SETUP_STAGE");
    if (stage && strcmp(stage, "ready") == 0) checkpoint(pid);
    if (stage && strcmp(stage, "blocked") == 0 && kill(pid, SIGSTOP) < 0) _exit(125);
}

static ssize_t observed_send(int fd, const void *buffer, size_t size, int flags) {
    ssize_t (*real)(int, const void *, size_t, int) = NEXT(send);
    const char *stage = getenv("SANDBOX_TEST_SETUP_STAGE");
    static int observed;
    int gate = !observed && fd == native_fd && stage && strcmp(stage, "ready") != 0;
    ssize_t count = real(fd, buffer, gate && size > 8 ? 8 : size, flags);
    if (gate && count > 0) { observed = 1; checkpoint(native_pid); }
    // If cancellation at readiness was ignored, let the released target exit
    // before the next supervisor iteration can hide that mistake by killing it.
    if (fd == native_fd && stage && strcmp(stage, "ready") == 0 && count == (ssize_t)size) {
        siginfo_t info = {0};
        if (waitid(P_PID, native_root, &info, WEXITED | WNOWAIT) < 0) _exit(125);
    }
    return count;
}

#ifdef __APPLE__
static ssize_t observed_recv(int fd, void *buffer, size_t size, int flags) {
    ssize_t (*real)(int, void *, size_t, int) = NEXT(recv);
    ssize_t count = real(fd, buffer, size, flags);
    if (count == 1 && size == 1 && ((unsigned char *)buffer)[0] == 1) ready(fd, native_pid);
    return count;
}
#else
ssize_t recvmsg(int fd, struct msghdr *message, int flags) {
    ssize_t (*real)(int, struct msghdr *, int) = NEXT(recvmsg);
    ssize_t count = real(fd, message, flags);
    if (count == 1) {
        for (struct cmsghdr *c = CMSG_FIRSTHDR(message); c; c = CMSG_NXTHDR(message, c)) {
            if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_CREDENTIALS)
                ready(fd, ((struct ucred *)CMSG_DATA(c))->pid);
        }
    }
    return count;
}
#endif

static int observed_poll(struct pollfd *fds, nfds_t count, int timeout) {
    int (*real)(struct pollfd *, nfds_t, int) = NEXT(poll);
    static int observed;
    sigset_t mask;
    sigprocmask(SIG_BLOCK, NULL, &mask);
    if (!observed && sigismember(&mask, SIGTERM) && getenv("SANDBOX_TEST_OBSERVE_POLL")) { observed = 1; checkpoint(getpid()); }
    return real(fds, count, timeout);
}

static int observed_kill(pid_t pid, int number) {
    int (*real)(pid_t, int) = NEXT(kill);
    if (number == SIGKILL && getenv("SANDBOX_TEST_FAIL_KILL")) { errno = EPERM; return -1; }
    return real(pid, number);
}

static int observed_unlinkat(int fd, const char *path, int flags) {
    int (*real)(int, const char *, int) = NEXT(unlinkat);
    if (getenv("SANDBOX_TEST_FAIL_REMOVE")) { errno = EPERM; return -1; }
    return real(fd, path, flags);
}

#ifdef __APPLE__
static int observed_kevent(int queue, const struct kevent *changes, int count,
                           struct kevent *events, int capacity, const struct timespec *timeout) {
    int (*real)(int, const struct kevent *, int, struct kevent *, int, const struct timespec *) = NEXT(kevent);
    int result = real(queue, changes, count, events, capacity, timeout);
    int saved = errno;
    if (result >= 0 && getenv("SANDBOX_TEST_OBSERVE_WATCH")) {
        for (int i = 0; i < count; ++i)
            if (changes[i].filter == EVFILT_PROC && (changes[i].flags & EV_ADD)) checkpoint(changes[i].ident);
    }
    errno = saved;
    return result;
}
static int observed_children(pid_t pid, void *buffer, int size) {
    int (*real)(pid_t, void *, int) = NEXT(proc_listchildpids);
    if (getenv("SANDBOX_TEST_FAIL_DISCOVERY")) { errno = EPERM; return 0; }
    return real(pid, buffer, size);
}
#define INTERPOSE(replacement, original) \
    __attribute__((used)) static const struct { const void *a; const void *b; } \
    pair_##original __attribute__((section("__DATA,__interpose"))) = \
    {(const void *)(uintptr_t)&replacement, (const void *)(uintptr_t)&original};
INTERPOSE(observed_fork, fork)
INTERPOSE(observed_setpgid, setpgid)
INTERPOSE(observed_recv, recv)
INTERPOSE(observed_send, send)
INTERPOSE(observed_poll, poll)
INTERPOSE(observed_kevent, kevent)
INTERPOSE(observed_kill, kill)
INTERPOSE(observed_unlinkat, unlinkat)
INTERPOSE(observed_children, proc_listchildpids)
#else
pid_t fork(void) { return observed_fork(); }
int setpgid(pid_t pid, pid_t group) { return observed_setpgid(pid, group); }
ssize_t send(int fd, const void *buffer, size_t size, int flags) { return observed_send(fd, buffer, size, flags); }
int poll(struct pollfd *fds, nfds_t count, int timeout) { return observed_poll(fds, count, timeout); }
int kill(pid_t pid, int number) { return observed_kill(pid, number); }
int unlinkat(int fd, const char *path, int flags) { return observed_unlinkat(fd, path, flags); }
#endif
