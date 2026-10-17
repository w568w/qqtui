#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>

/* Redirect only QQ's package manifest. The replacement is prepared with a JSON
 * parser by the supervisor. QQ's installed files are never opened for writing. */
static const char *redirect(const char *path, int writable) {
    const char *original = getenv("QQTUI_PACKAGE");
    const char *replacement = getenv("QQTUI_MANIFEST");
    return !writable && original && replacement && path && strcmp(path, original) == 0
        ? replacement : path;
}

#define OPEN_HOOK(name) \
int name(const char *path, int flags, ...) { \
    int (*real)(const char *, int, ...) = dlsym(RTLD_NEXT, #name); \
    mode_t mode = 0; \
    if ((flags & O_CREAT) || ((flags & O_TMPFILE) == O_TMPFILE)) { \
        va_list ap; va_start(ap, flags); mode = va_arg(ap, mode_t); va_end(ap); \
    } \
    return real(redirect(path, (flags & O_ACCMODE) != O_RDONLY), flags, mode); \
}
OPEN_HOOK(open)
OPEN_HOOK(open64)

#define FOPEN_HOOK(name) \
FILE *name(const char *path, const char *mode) { \
    FILE *(*real)(const char *, const char *) = dlsym(RTLD_NEXT, #name); \
    return real(redirect(path, strcmp(mode, "r") && strcmp(mode, "rb")), mode); \
}
FOPEN_HOOK(fopen)
FOPEN_HOOK(fopen64)
