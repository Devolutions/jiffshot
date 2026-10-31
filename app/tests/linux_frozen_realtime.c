/* Keep screenshot names predictable for a no-overwrite regression. */
#define _GNU_SOURCE
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

int clock_gettime(clockid_t clock_id, struct timespec *value) {
    if (clock_id == CLOCK_REALTIME) {
        value->tv_sec = 1700000000;
        value->tv_nsec = 123456789;
        return 0;
    }
    return (int)syscall(SYS_clock_gettime, clock_id, value);
}
