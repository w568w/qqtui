#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <limits.h>
#include <link.h>
#include <stdatomic.h>
#include <sys/socket.h>

typedef int (*metrics_fn)(char **, int, int);
static _Atomic(metrics_fn) original_metrics;
static int bridge_metrics(char **output, int capacity, int width) {
    metrics_fn original = atomic_load(&original_metrics);
    if (output && (capacity <= 0 || width <= 0)) return -1;
    Dl_info module;
    char directory[PATH_MAX];
    if (!dladdr((void *)bridge_metrics, &module) ||
        !realpath(module.dli_fname, directory)) return original(output, capacity, width);
    char *slash = strrchr(directory, '/');
    if (!slash) return original(output, capacity, width);
    size_t prefix = slash - directory + 1;
    int count = original(NULL, 0, 0);
    if (count <= 0 || count > 1024) return original(output, capacity, width);
    char **names = calloc(count, sizeof(*names));
    char *storage = calloc(count, PATH_MAX);
    if (!names || !storage) { free(names); free(storage); return original(output, capacity, width); }
    for (int i = 0; i < count; i++) names[i] = storage + (size_t)i * PATH_MAX;
    int fetched = original(names, count, PATH_MAX);
    if (fetched < count) count = fetched;
    int kept = 0;
    for (int i = 0; i < count; i++) {
        names[i][PATH_MAX - 1] = 0;
        if (!strncmp(names[i], directory, prefix)) continue;
        if (output && kept < capacity && output[kept]) {
            size_t length = strlen(names[i]);
            size_t size = length < (size_t)width ? length : (size_t)width - 1;
            memcpy(output[kept], names[i] + length - size, size);
            output[kept][size] = 0;
        }
        kept++;
    }
    free(storage); free(names);
    return output && kept > capacity ? capacity : kept;
}

static int crash_report_connect(int socket, const struct sockaddr *address, socklen_t length) {
    (void)socket; (void)address; (void)length;
    // Audit modules have their own libc namespace; set the caller's errno.
    void *base = dlmopen(LM_ID_BASE, NULL, RTLD_LAZY);
    int *(*caller_errno)(void) = base ? dlsym(base, "__errno_location") : NULL;
    if (base) dlclose(base);
    if (caller_errno) *caller_errno() = EACCES;
    return -1;
}

unsigned int la_version(unsigned int version) { return version < LAV_CURRENT ? version : LAV_CURRENT; }
unsigned int la_objopen(struct link_map *map, Lmid_t ns, uintptr_t *cookie) {
    (void)ns;
    *cookie = (uintptr_t)map;
    return LA_FLG_BINDTO | LA_FLG_BINDFROM;
}
uintptr_t la_symbind64(Elf64_Sym *symbol, unsigned int index,
                     uintptr_t *from, uintptr_t *to, unsigned int *flags,
                     const char *name) {
    (void)index; (void)to; (void)flags;
    if (!strcmp(name, "NodeContextifyContextMetrics1")) {
        atomic_store(&original_metrics, (metrics_fn)symbol->st_value);
        return (uintptr_t)bridge_metrics;
    }
    if (!strcmp(name, "connect") && from && *from) {
        const char *library = ((struct link_map *)*from)->l_name;
        const char *base = strrchr(library, '/');
        if (!strcmp(base ? base + 1 : library, "libbugly.so"))
            return (uintptr_t)crash_report_connect;
    }
    return symbol->st_value;
}

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
