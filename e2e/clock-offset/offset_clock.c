/* Moves the wall clock of a process by CLOCK_OFFSET_S seconds (test only,
 * LD_PRELOAD). The monotonic clock and every other call stay as they are.
 * libfaketime did the same job, but it made the nodes 5-10 times slower. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdlib.h>
#include <sys/time.h>
#include <time.h>

static int (*real_clock_gettime)(clockid_t, struct timespec *);
static int (*real_gettimeofday)(struct timeval *restrict, void *restrict);
static time_t (*real_time)(time_t *);
static long offset_s;

__attribute__((constructor)) static void init(void) {
    real_clock_gettime = dlsym(RTLD_NEXT, "clock_gettime");
    real_gettimeofday = dlsym(RTLD_NEXT, "gettimeofday");
    real_time = dlsym(RTLD_NEXT, "time");
    const char *s = getenv("CLOCK_OFFSET_S");
    offset_s = s ? strtol(s, NULL, 10) : 0;
}

int clock_gettime(clockid_t id, struct timespec *ts) {
    int r = real_clock_gettime(id, ts);
    if (r == 0 && (id == CLOCK_REALTIME || id == CLOCK_REALTIME_COARSE))
        ts->tv_sec += offset_s;
    return r;
}

int gettimeofday(struct timeval *restrict tv, void *restrict tz) {
    int r = real_gettimeofday(tv, tz);
    if (r == 0)
        tv->tv_sec += offset_s;
    return r;
}

time_t time(time_t *t) {
    time_t now = real_time(NULL) + offset_s;
    if (t)
        *t = now;
    return now;
}
