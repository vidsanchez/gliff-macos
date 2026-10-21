// Native bundle entry point: no GTK, permissions, tap or GUI initialization.
#include <mach-o/dyld.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void fail(const char *message) { perror(message); exit(1); }
#ifndef GLIFF_VERSION
#define GLIFF_VERSION "development"
#endif
static void private_dir(const char *path) {
    if (mkdir(path, 0700) && errno != EEXIST) fail("cache directory");
    struct stat info;
    if (lstat(path, &info) || !S_ISDIR(info.st_mode) || info.st_uid != getuid() || (info.st_mode & 077)) {
        errno = EACCES; fail("unsafe cache directory");
    }
}
static void envpath(const char *name, const char *root, const char *suffix) {
    char value[PATH_MAX];
    if (snprintf(value, sizeof(value), "%s/%s", root, suffix) >= (int)sizeof(value)) {
        errno = ENAMETOOLONG; fail(name);
    }
    if (setenv(name, value, 1)) fail(name);
}
int main(int argc, char **argv) {
    (void)argc;
    char raw[PATH_MAX], executable[PATH_MAX], resources[PATH_MAX];
    uint32_t size = sizeof(raw);
    if (_NSGetExecutablePath(raw, &size) || !realpath(raw, executable)) fail("bundle path");
    char *slash = strrchr(executable, '/');
    if (!slash) { errno = EINVAL; fail("bundle layout"); }
    *slash = '\0'; // Contents/MacOS
    if (snprintf(resources, sizeof(resources), "%s/../Resources", executable) >= (int)sizeof(resources)) {
        errno = ENAMETOOLONG; fail("resources");
    }
    envpath("XDG_DATA_DIRS", resources, "share");
    envpath("GSETTINGS_SCHEMA_DIR", resources, "share/glib-2.0/schemas");
    envpath("GIO_MODULE_DIR", resources, "lib/gio/modules");
    setenv("GIO_EXTRA_MODULES", "", 1);
    envpath("GTK_PATH", resources, "lib/gtk-4.0");
    setenv("GTK_DATA_PREFIX", resources, 1);
    setenv("GTK_EXE_PREFIX", resources, 1);
    envpath("FONTCONFIG_FILE", resources, "etc/fonts/fonts.conf");
    envpath("FONTCONFIG_PATH", resources, "etc/fonts");
    envpath("GDK_PIXBUF_MODULEDIR", resources, "lib/gdk-pixbuf-2.0/2.10.0/loaders");
    // Loader caches contain absolute paths. Generate a private runtime cache,
    // never modify the signed bundle or depend on the packager's Homebrew path.
    char cache[PATH_MAX], directory[PATH_MAX];
    const char *home = getenv("HOME");
    if (!home) { errno = EINVAL; fail("HOME"); }
    if (snprintf(directory, sizeof(directory), "%s/Library/Caches/io.github.vidsanchez.gliff-macos", home) >= (int)sizeof(directory)) {
        errno = ENAMETOOLONG; fail("cache directory");
    }
    private_dir(directory);
    size_t end = strlen(directory);
    if (snprintf(directory + end, sizeof(directory) - end, "/%s", GLIFF_VERSION) >= (int)(sizeof(directory) - end)) {
        errno = ENAMETOOLONG; fail("cache version");
    }
    private_dir(directory);
    if (snprintf(cache, sizeof(cache), "%s/loaders-XXXXXX", directory) >= (int)sizeof(cache)) {
        errno = ENAMETOOLONG; fail("loader cache");
    }
    int fd = mkstemp(cache);
    if (fd < 0) fail("loader cache");
    close(fd);
    setenv("GDK_PIXBUF_MODULE_FILE", cache, 1);
    char helper[PATH_MAX];
    snprintf(helper, sizeof(helper), "%s/gdk-pixbuf-query-loaders", executable);
    pid_t child = fork();
    if (child < 0) fail("loader fork");
    if (!child) { execl(helper, helper, "--update-cache", NULL); _exit(127); }
    int status;
    if (waitpid(child, &status, 0) < 0 || !WIFEXITED(status) || WEXITSTATUS(status)) {
        unlink(cache); errno = EIO; fail("loader cache generation");
    }
    // The non-secret versioned cache can be removed when all Gliff instances exit.
    // Keeping it allows GdkPixbuf's lazy module initialization after exec.
    char binary[PATH_MAX];
    snprintf(binary, sizeof(binary), "%s/gliff-bin", executable);
    argv[0] = binary;
    execv(binary, argv);
    unlink(cache);
    fail("gliff exec");
}
