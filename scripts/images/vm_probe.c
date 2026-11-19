/* Dedicated Linux guest init for the BusyBox RAFS correctness probe.
 * Build statically; this is a test workload, never production guest init. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/ioctl.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

static void finish(int line) {
    int saved = errno;
    int fd = open("/pvisor-vm-probe-result", O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (fd >= 0) {
        if (line) dprintf(fd, "FAIL line=%d errno=%d\n", line, saved);
        else dprintf(fd, "PASS\n");
        fsync(fd);
        close(fd);
    }
    sync();
    /* Same exit-status ioctl and reboot handshake as pvisor-guest. */
    fd = open("/", O_RDONLY | O_DIRECTORY);
    if (fd >= 0) {
        int code = line ? 1 : 0;
        if (syncfs(fd) < 0) code = 125;
        ioctl(fd, 0x7602, code);
        close(fd);
    }
    reboot(RB_AUTOBOOT);
    _exit(111);
}
#define CHECK(value) do { if (!(value)) finish(__LINE__); } while (0)

int main(void) {
    struct stat before, after;
    char data[256] = {0};
    const char value[] = {0, (char)255, 7};
    mkdir("/proc", 0755);
    mount("proc", "/proc", "proc", 0, NULL);
    CHECK(lstat("/lib64", &before) == 0 && S_ISLNK(before.st_mode));
    CHECK(before.st_uid == 0 && before.st_gid == 0);
    CHECK(readlink("/lib64", data, sizeof(data)) == 3 && !memcmp(data, "lib", 3));
    int fd = open("/bin/busybox", O_RDONLY);
    CHECK(fd >= 0 && read(fd, data, 4) == 4 && !memcmp(data, "\177ELF", 4));
    close(fd);
    pid_t child = fork();
    CHECK(child >= 0);
    if (!child) { execl("/bin/busybox", "busybox", "true", NULL); _exit(112); }
    int status;
    CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    fd = open("/pvisor-probe-file", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    CHECK(fd >= 0 && write(fd, "private", 7) == 7 && fsync(fd) == 0);
    close(fd);
    CHECK(setxattr("/pvisor-probe-file", "user.probe", value, sizeof(value), XATTR_CREATE) == 0);
    CHECK(getxattr("/pvisor-probe-file", "user.probe", data, sizeof(data)) == (ssize_t)sizeof(value));
    CHECK(!memcmp(data, value, sizeof(value)));
    CHECK(removexattr("/pvisor-probe-file", "user.probe") == 0);
    CHECK(symlink("/pvisor-probe-file", "/pvisor-probe-link") == 0);
    CHECK(lchown("/pvisor-probe-link", 2345, 3456) == 0);
    CHECK(lstat("/pvisor-probe-link", &before) == 0 && before.st_uid == 2345 && before.st_gid == 3456);
    CHECK(link("/pvisor-probe-link", "/pvisor-probe-alias") == 0);
    CHECK(lstat("/pvisor-probe-link", &before) == 0 && lstat("/pvisor-probe-alias", &after) == 0);
    CHECK(before.st_ino == after.st_ino && after.st_nlink == 2 && S_ISLNK(after.st_mode));
    CHECK(unlink("/pvisor-probe-link") == 0);
    CHECK(rename("/pvisor-probe-alias", "/pvisor-probe-moved") == 0);
    CHECK(lstat("/pvisor-probe-moved", &after) == 0 && after.st_nlink == 1 && after.st_uid == 2345 && after.st_gid == 3456);
    fd = open("/pvisor-probe-moved", O_RDONLY);
    CHECK(fd >= 0 && read(fd, data, sizeof(data)) == 7 && !memcmp(data, "private", 7));
    close(fd);
    finish(0);
}
