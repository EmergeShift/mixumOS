/* 静态计数器:跨内核 CRIU 实验的被测进程。
 * 编译: cc -static -O2 -o counter static_counter.c
 * 运行后写 /tmp/criu-pid 与 /tmp/criu-counter,每秒递增。 */
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static void become_session_leader(void) {
    if (setsid() >= 0) {
        return;
    }
    if (fork() == 0) {
        setsid();
        return;
    }
    _exit(0);
}

int main(void) {
    become_session_leader();
    for (int fd = 3; fd < 256; fd++) {
        close(fd);
    }
    FILE *pidfile = fopen("/tmp/criu-pid", "w");
    if (pidfile) {
        fprintf(pidfile, "%d", getpid());
        fclose(pidfile);
    }
    FILE *counter = fopen("/tmp/criu-counter", "w");
    if (!counter) {
        perror("open counter");
        return 1;
    }
    for (long i = 0;; i++) {
        rewind(counter);
        fprintf(counter, "%ld", i);
        fflush(counter);
        sleep(1);
    }
}
