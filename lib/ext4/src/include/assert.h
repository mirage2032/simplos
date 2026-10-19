#pragma once

/* In src/lib.rs: panics. */
void simplos_ext4_assert_failed(const char *file, int line);
#define assert(e) ((e) ? (void)0 : simplos_ext4_assert_failed(__FILE__, __LINE__))
