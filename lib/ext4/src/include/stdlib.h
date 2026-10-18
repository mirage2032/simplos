#pragma once

#include <stddef.h>

/* In src/libc.c. */
void simplos_qsort(void *base, size_t count, size_t size,
		   int (*compare)(const void *, const void *));
#define qsort simplos_qsort
