/*
 * What lwext4 needs of a C library besides what any compiler's runtime has
 * (memcpy, memmove, memset, memcmp and strlen: Rust's compiler_builtins, in
 * the kernel). Named apart from the host's C library, which the tests link,
 * so that they run these too; src/include maps lwext4's calls to them.
 */
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

int simplos_strcmp(const char *a, const char *b)
{
	while (*a && *a == *b)
		a++, b++;
	return (unsigned char)*a - (unsigned char)*b;
}

int simplos_strncmp(const char *a, const char *b, size_t n)
{
	for (; n; n--, a++, b++)
		if (*a != *b || !*a)
			return (unsigned char)*a - (unsigned char)*b;
	return 0;
}

char *simplos_strcpy(char *dest, const char *src)
{
	char *d = dest;
	while ((*d++ = *src++))
		;
	return dest;
}

/* An insertion sort: lwext4 only sorts a directory block's entries, a few
 * hundred at most. */
void simplos_qsort(void *base, size_t count, size_t size,
		   int (*compare)(const void *, const void *))
{
	char *items = base;
	for (size_t i = 1; i < count; i++) {
		for (size_t j = i; j && compare(items + (j - 1) * size, items + j * size) > 0; j--) {
			char *a = items + (j - 1) * size, *b = items + j * size;
			for (size_t k = 0; k < size; k++) {
				char t = a[k];
				a[k] = b[k];
				b[k] = t;
			}
		}
	}
}
