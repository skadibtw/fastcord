#include <stdio.h>
#include <string.h>
#include "getopt.h"

char *optarg;
int optind = 1, opterr = 1, optopt;
static const char *next;

static int long_option(int argc, char *const argv[], const struct option *longopts, int *longindex)
{
    const char *name = argv[optind] + 2, *eq = strchr(name, '=');
    size_t len = eq ? (size_t)(eq - name) : strlen(name);
    optind++;
    for (int i = 0; longopts[i].name; i++) {
        if (strlen(longopts[i].name) != len || strncmp(longopts[i].name, name, len))
            continue;
        if (longindex)
            *longindex = i;
        optarg = 0;
        if (eq) {
            if (longopts[i].has_arg == no_argument)
                return '?';
            optarg = (char *)eq + 1;
        } else if (longopts[i].has_arg == required_argument) {
            if (optind >= argc)
                return '?';
            optarg = argv[optind++];
        }
        if (longopts[i].flag) {
            *longopts[i].flag = longopts[i].val;
            return 0;
        }
        return longopts[i].val;
    }
    if (opterr)
        fprintf(stderr, "unknown option: %s\n", argv[optind - 1]);
    return '?';
}

int getopt_long(int argc, char *const argv[], const char *optstring,
    const struct option *longopts, int *longindex)
{
    if (!next || !*next) {
        next = 0;
        if (optind >= argc || argv[optind][0] != '-' || !argv[optind][1])
            return -1;
        if (!strcmp(argv[optind], "--")) {
            optind++;
            return -1;
        }
        if (argv[optind][1] == '-')
            return long_option(argc, argv, longopts, longindex);
        next = argv[optind++] + 1;
    }
    int c = *next++;
    const char *spec = c == ':' ? 0 : strchr(optstring, c);
    optarg = 0;
    if (!spec) {
        optopt = c;
        if (opterr)
            fprintf(stderr, "unknown option: -%c\n", c);
        return '?';
    }
    if (spec[1] == ':') {
        if (*next) {
            optarg = (char *)next;
        } else if (spec[2] != ':') {
            if (optind >= argc) {
                optopt = c;
                return '?';
            }
            optarg = argv[optind++];
        }
        next = 0;
    }
    return c;
}
