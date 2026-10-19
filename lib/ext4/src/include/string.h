#pragma once

#include <stddef.h>

void *memcpy(void *dest, const void *src, size_t n);
void *memmove(void *dest, const void *src, size_t n);
void *memset(void *s, int c, size_t n);
int memcmp(const void *a, const void *b, size_t n);
size_t strlen(const char *s);

/* In src/libc.c. */
int simplos_strcmp(const char *a, const char *b);
int simplos_strncmp(const char *a, const char *b, size_t n);
char *simplos_strcpy(char *dest, const char *src);
#define strcmp simplos_strcmp
#define strncmp simplos_strncmp
#define strcpy simplos_strcpy
