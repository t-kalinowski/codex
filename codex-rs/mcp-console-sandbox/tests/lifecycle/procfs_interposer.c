#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static char *library;
static int inner;

__attribute__((constructor)) static void capture(int argc, char **argv) {
    const char *value = getenv("LD_PRELOAD");
    if (value) library = strdup(value);
    for (int i = 1; i < argc; ++i)
        if (strcmp(argv[i], "--apply-seccomp-then-exec") == 0) inner = 1;
}

int execvp(const char *file, char *const argv[]) {
    int (*real)(const char *, char *const []) = dlsym(RTLD_NEXT, "execvp");
    // Trusted test instrumentation crosses the production loader sanitation
    // only for the explicit native hook. No target or policy is substituted.
    if (argv[0] && argv[1] && strcmp(argv[1], "--target-setup-fd") == 0 && library)
        if (setenv("LD_PRELOAD", library, 1) < 0) _exit(125);
    return real(file, argv);
}

ssize_t readlink(const char *path, char *buffer, size_t size) {
    ssize_t (*real)(const char *, char *, size_t) = dlsym(RTLD_NEXT, "readlink");
    if (inner && strcmp(path, "/proc/self") == 0) {
        const char mismatched_pid[] = "2147483647";
        size_t count = sizeof(mismatched_pid) - 1;
        if (count > size) count = size;
        memcpy(buffer, mismatched_pid, count);
        return count;
    }
    return real(path, buffer, size);
}
