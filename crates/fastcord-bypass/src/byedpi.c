/* The only entry points Rust uses. byedpi's `main` is renamed to
 * `byedpi_main` and its `signal` calls to `byedpi_signal` by build.rs, so the
 * engine never takes over the application's signal handlers. */
#include "params.h"

int byedpi_main(int argc, char **argv);
extern int optind;
#if defined(__APPLE__) || defined(__FreeBSD__) || defined(__OpenBSD__)
extern int optreset;
#endif

typedef void (*byedpi_handler)(int);

byedpi_handler byedpi_signal(int sig, byedpi_handler handler)
{
    (void)sig;
    (void)handler;
    return 0;
}

#ifdef _WIN32
/* The upstream Windows build registers a service; an embedded engine never is one. */
int register_winsvc(int argc, char *argv[])
{
    (void)argc;
    (void)argv;
    return 0;
}
#endif

/* byedpi keeps its options in one global; restore the compiled-in defaults
 * so a later run does not inherit the previous strategy. */
static struct params defaults;
static int saved;

/* ponytail: a volatile flag, not a C11 atomic (MSVC C11 atomics are
 * experimental); the waking connect orders it in practice. */
volatile int fastcord_stop_requested;

int fastcord_byedpi_run(int argc, char **argv)
{
    fastcord_stop_requested = 0;
    if (!saved) {
        defaults = params;
        saved = 1;
    } else {
        params = defaults;
    }
#if defined(__GLIBC__)
    optind = 0;
#else
    optind = 1;
#endif
#if defined(__APPLE__) || defined(__FreeBSD__) || defined(__OpenBSD__)
    optreset = 1;
#endif
    return byedpi_main(argc, argv);
}

/* Read by the patched on_accept once the caller wakes the event loop. */
void fastcord_byedpi_request_stop(void)
{
    fastcord_stop_requested = 1;
}
