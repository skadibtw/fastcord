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

/* Cancellation is monotonic for one owned worker. Reset it on the Rust
 * caller before spawning, never on entry to a potentially delayed worker. */
#ifdef _WIN32
#include <windows.h>
static volatile LONG stop_requested;
void fastcord_byedpi_prepare(void) { InterlockedExchange(&stop_requested, 0); }
void fastcord_byedpi_request_stop(void) { InterlockedExchange(&stop_requested, 1); }
int fastcord_byedpi_stopping(void)
{
    return InterlockedCompareExchange(&stop_requested, 0, 0) != 0;
}
#else
#include <stdatomic.h>
static atomic_int stop_requested;
void fastcord_byedpi_prepare(void)
{
    atomic_store_explicit(&stop_requested, 0, memory_order_release);
}
void fastcord_byedpi_request_stop(void)
{
    atomic_store_explicit(&stop_requested, 1, memory_order_release);
}
int fastcord_byedpi_stopping(void)
{
    return atomic_load_explicit(&stop_requested, memory_order_acquire);
}
#endif

int fastcord_byedpi_run(int argc, char **argv)
{
    if (fastcord_byedpi_stopping()) return 0;
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
