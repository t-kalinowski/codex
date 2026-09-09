#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <poll.h>
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

static pid_t observed_fork(void) {
    pid_t (*real)(void) = NEXT(fork);
    pid_t pid = real();
    if (pid > 0 && getenv("SANDBOX_TEST_GATE_SPAWN")) checkpoint(pid);
    return pid;
}

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
INTERPOSE(observed_poll, poll)
INTERPOSE(observed_kevent, kevent)
INTERPOSE(observed_kill, kill)
INTERPOSE(observed_unlinkat, unlinkat)
INTERPOSE(observed_children, proc_listchildpids)
#else
pid_t fork(void) { return observed_fork(); }
int poll(struct pollfd *fds, nfds_t count, int timeout) { return observed_poll(fds, count, timeout); }
int kill(pid_t pid, int number) { return observed_kill(pid, number); }
int unlinkat(int fd, const char *path, int flags) { return observed_unlinkat(fd, path, flags); }
#endif
