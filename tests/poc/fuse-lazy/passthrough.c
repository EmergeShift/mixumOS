// 最小 FUSE 惰性挂载 PoC：getattr/readdir/open/read 全部按需访问源目录，
// 不做预拷贝；每次 read 记录到 /tmp/fuse-reads.log。
#define FUSE_USE_VERSION 31
#define _FILE_OFFSET_BITS 64

#include <fuse3/fuse.h>
#include <errno.h>
#include <dirent.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static char source_root[4096];

static void full_path(char *out, size_t size, const char *path) {
    snprintf(out, size, "%s%s", source_root, path);
}

static int fs_getattr(const char *path, struct stat *st,
                      struct fuse_file_info *fi) {
    (void)fi;
    char full[4096];
    full_path(full, sizeof(full), path);
    if (lstat(full, st) != 0) return -errno;
    return 0;
}

static int fs_readdir(const char *path, void *buf, fuse_fill_dir_t filler,
                      off_t offset, struct fuse_file_info *fi,
                      enum fuse_readdir_flags flags) {
    (void)offset;
    (void)fi;
    (void)flags;
    char full[4096];
    full_path(full, sizeof(full), path);
    DIR *dir = opendir(full);
    if (!dir) return -errno;
    filler(buf, ".", NULL, 0, 0);
    filler(buf, "..", NULL, 0, 0);
    struct dirent *entry;
    while ((entry = readdir(dir)) != NULL) {
        if (strcmp(entry->d_name, ".") == 0 || strcmp(entry->d_name, "..") == 0)
            continue;
        struct stat st;
        char child[4096];
        snprintf(child, sizeof(child), "%s/%s", path, entry->d_name);
        full_path(full, sizeof(full), child);
        if (lstat(full, &st) == 0) filler(buf, entry->d_name, &st, 0, 0);
    }
    closedir(dir);
    return 0;
}

static int fs_open(const char *path, struct fuse_file_info *fi) {
    char full[4096];
    full_path(full, sizeof(full), path);
    int fd = open(full, fi->flags);
    if (fd < 0) return -errno;
    fi->fh = fd;
    return 0;
}

static int fs_read(const char *path, char *buf, size_t size, off_t offset,
                   struct fuse_file_info *fi) {
    FILE *log = fopen("/tmp/fuse-reads.log", "a");
    if (log) {
        fprintf(log, "read %s off=%lld size=%zu\n", path,
                (long long)offset, size);
        fclose(log);
    }
    ssize_t n = pread(fi->fh, buf, size, offset);
    if (n < 0) return -errno;
    return (int)n;
}

static int fs_release(const char *path, struct fuse_file_info *fi) {
    (void)path;
    close(fi->fh);
    return 0;
}

static struct fuse_operations ops = {
    .getattr = fs_getattr,
    .readdir = fs_readdir,
    .open = fs_open,
    .read = fs_read,
    .release = fs_release,
};

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s <source> <mountpoint> [fuse args]\n", argv[0]);
        return 2;
    }
    snprintf(source_root, sizeof(source_root), "%s", argv[1]);
    // 把 mountpoint 及其后的参数交给 fuse_main。
    for (int i = 1; i < argc - 1; i++) argv[i] = argv[i + 1];
    return fuse_main(argc - 1, argv, &ops, NULL);
}
