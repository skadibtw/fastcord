/* Force-included before every byedpi source when the compiler is MSVC. */
#pragma once
#include <BaseTsd.h>
#include <malloc.h>
#include <string.h>
typedef SSIZE_T ssize_t;
#define strncasecmp _strnicmp
#define strcasecmp _stricmp
#define __attribute__(x)
