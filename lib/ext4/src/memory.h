/*
 * lwext4 calls these (CONFIG_USE_USER_MALLOC) without declaring them, so
 * build.rs includes this header into every one of its files. They are
 * defined in src/memory.rs, on Rust's allocator.
 */
#include <stddef.h>

void *ext4_user_malloc(size_t size);
void *ext4_user_calloc(size_t count, size_t size);
void *ext4_user_realloc(void *pointer, size_t size);
void ext4_user_free(void *pointer);
