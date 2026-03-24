/*
 * arkhe PID 1 stub
 *
 * This is the simplest correct init process. It does exactly what the
 * kernel requires of PID 1 and nothing else:
 *   1. Reap zombie processes (blocking waitpid loop)
 *   2. Forward signals to the supervisor
 *   3. Exec the supervisor as the first child
 *   4. Mount /proc, /sys, /run before spawning supervisor
 *
 * No heap allocation. No stdio. No config parsing. No IPC.
 * If this process exits, the kernel panics. Keep it simple.
 *
 * Compile: cc -static -O2 -Wall -Wextra -Werror -o pid1 pid1/main.c
 */

#define _GNU_SOURCE
#include <signal.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/reboot.h>
#include <sys/wait.h>
#include <sys/stat.h>
#include <unistd.h>
#include <errno.h>
#include <linux/reboot.h>

#define SUPERVISOR_PATH "/usr/sbin/arkhd"

/* Three volatile globals. Zero heap. */
static volatile pid_t supervisor_pid = 0;
static volatile sig_atomic_t shutdown_requested = 0;
static volatile sig_atomic_t reboot_type = 0; /* 0=none, 1=poweroff, 2=reboot */

/* write() wrapper for constant strings — async-signal-safe, no stdio. */
static void msg(const char *s) {
    /* strlen equivalent for constant strings */
    const char *p = s;
    while (*p) p++;
    write(STDERR_FILENO, s, (size_t)(p - s));
}

_Noreturn static void die(const char *s) {
    msg(s);
    _exit(1);
}

static void forward_signal(int sig) {
    if (sig == SIGHUP) {
        /* SIGHUP: forward to supervisor for config reload, do NOT shut down. */
        if (supervisor_pid > 0)
            kill(supervisor_pid, sig);
        return;
    }
    /* SIGTERM → poweroff, SIGINT → reboot */
    if (sig == SIGTERM)
        reboot_type = 1;
    else if (sig == SIGINT)
        reboot_type = 2;
    shutdown_requested = 1;
    if (supervisor_pid > 0)
        kill(supervisor_pid, SIGTERM);
}

static pid_t spawn_supervisor(void) {
    pid_t pid = fork();
    if (pid < 0)
        return -1;
    if (pid == 0) {
        /* Child: exec the supervisor */
        char *argv[] = { "arkhd", NULL };
        char *envp[] = { "PATH=/usr/bin:/usr/sbin:/bin:/sbin", NULL };
        execve(SUPERVISOR_PATH, argv, envp);
        _exit(127);
    }
    return pid;
}

int main(void) {
    if (getpid() != 1)
        die("arkhe: pid1 must be run as PID 1\n");

    /* Signal handlers with sa_flags=0: no SA_RESTART so waitpid gets EINTR. */
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = forward_signal;
    sa.sa_flags = 0;
    sigaction(SIGTERM, &sa, NULL);
    sigaction(SIGINT, &sa, NULL);
    sigaction(SIGHUP, &sa, NULL);

    /* Catch orphaned processes that escape PID namespace via double-fork. */
    prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);

    /* Early mounts — idempotent (fail silently if already mounted). */
    mount("proc",  "/proc", "proc",  MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL);
    mount("sysfs", "/sys",  "sysfs", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL);
    mount("devtmpfs", "/dev", "devtmpfs", MS_NOSUID, NULL);
    mkdir("/dev/pts", 0755);
    mount("devpts", "/dev/pts", "devpts", MS_NOSUID | MS_NOEXEC, NULL);
    mount("tmpfs", "/run",  "tmpfs", MS_NOSUID | MS_NODEV, "mode=0755");
    mount("cgroup2", "/sys/fs/cgroup", "cgroup2", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL);
    /* Remount root read-write */
    mount(NULL, "/", NULL, MS_REMOUNT, NULL);
    /* Create dirs arkhd needs */
    mkdir("/run/ready", 0755);
    mkdir("/run/arkhe", 0755);
    mkdir("/run/dbus", 0755);
    mkdir("/run/user", 0755);
    mkdir("/run/user/0", 0700);
    mkdir("/var/log/arkhe", 0755);
    msg("arkhe: pid1 started, mounts ready\n");

    supervisor_pid = spawn_supervisor();
    if (supervisor_pid < 0)
        die("arkhe: failed to spawn supervisor\n");

    /*
     * Main loop: blocking waitpid reaps zombies AND blocks for signals.
     * With sa_flags=0, signals cause waitpid to return -1/EINTR — no race.
     */
    for (;;) {
        int status;
        pid_t pid = waitpid(-1, &status, 0);

        if (pid < 0) {
            if (errno == EINTR) {
                /* Signal interrupted us. Check if we should shut down. */
                if (shutdown_requested && supervisor_pid == 0)
                    break;
                continue;
            }
            if (errno == ECHILD)
                break; /* No children left. */
            continue;
        }

        if (pid == supervisor_pid) {
            if (shutdown_requested) {
                supervisor_pid = 0;
                break;
            }
            /* Supervisor died unexpectedly — restart it. */
            msg("arkhe: supervisor exited, restarting\n");
            supervisor_pid = spawn_supervisor();
            if (supervisor_pid < 0)
                die("arkhe: failed to restart supervisor\n");
        }
        /* All other children: reaped. That's our job. */
    }

    sync();
    reboot(reboot_type == 2 ? LINUX_REBOOT_CMD_RESTART : LINUX_REBOOT_CMD_POWER_OFF);
    _exit(0); /* unreachable */
}
