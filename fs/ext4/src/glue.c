/*
 * The C side of the ext4 crate: what is simpler next to lwext4's headers
 * than mirrored in Rust. Rust only ever holds pointers to these structures.
 */
#include <string.h>

#include <stddef.h>

#include <ext4.h>
#include <ext4_blockdev.h>
#include <ext4_fs.h>
#include <ext4_inode.h>
#include <ext4_super.h>

/* In src/lib.rs: move `count` 512-byte sectors between the device and `buf`. */
int simplos_ext4_read(void *device, void *buf, uint64_t sector, uint32_t count);
int simplos_ext4_write(void *device, const void *buf, uint64_t sector, uint32_t count);

static int device_open(struct ext4_blockdev *bd)
{
	(void)bd;
	return EOK;
}

static int device_close(struct ext4_blockdev *bd)
{
	(void)bd;
	return EOK;
}

static int device_read(struct ext4_blockdev *bd, void *buf, uint64_t sector, uint32_t count)
{
	return simplos_ext4_read(bd->bdif->p_user, buf, sector, count);
}

static int device_write(struct ext4_blockdev *bd, const void *buf, uint64_t sector,
			uint32_t count)
{
	return simplos_ext4_write(bd->bdif->p_user, buf, sector, count);
}

/* A block device of `sectors` 512-byte sectors, read and written through
 * `device`, which is handed back to the two functions above. */
struct ext4_blockdev *simplos_ext4_device_new(void *device, uint64_t sectors)
{
	struct ext4_blockdev_iface *iface = ext4_calloc(1, sizeof(*iface));
	struct ext4_blockdev *bd = ext4_calloc(1, sizeof(*bd));
	uint8_t *sector = ext4_calloc(1, 512);
	if (!iface || !bd || !sector) {
		ext4_free(iface);
		ext4_free(bd);
		ext4_free(sector);
		return NULL;
	}
	iface->open = device_open;
	iface->bread = device_read;
	iface->bwrite = device_write;
	iface->close = device_close;
	iface->ph_bsize = 512;
	iface->ph_bcnt = sectors;
	iface->ph_bbuf = sector;
	iface->p_user = device;
	bd->bdif = iface;
	bd->part_offset = 0;
	bd->part_size = sectors * 512;
	return bd;
}

void simplos_ext4_device_free(struct ext4_blockdev *bd)
{
	ext4_free(bd->bdif->ph_bbuf);
	ext4_free(bd->bdif);
	ext4_free(bd);
}

/* `flags` are open(2)'s, which lwext4 shares. ext4_fopen2, unlike ext4_fopen,
 * journals the file's creation. */
ext4_file *simplos_ext4_file_open(const char *path, int flags, int *error)
{
	ext4_file *file = ext4_calloc(1, sizeof(*file));
	if (!file) {
		*error = ENOMEM;
		return NULL;
	}
	*error = ext4_fopen2(file, path, flags);
	if (*error != EOK) {
		ext4_free(file);
		return NULL;
	}
	return file;
}

uint32_t simplos_ext4_file_inode(ext4_file *file)
{
	return file->inode;
}

/* Make the next read or write start at `offset`. ext4_fseek would check it
 * against the size the file had when this handle last looked. */
void simplos_ext4_file_seek(ext4_file *file, uint64_t offset)
{
	file->fpos = offset;
}

int simplos_ext4_file_close(ext4_file *file)
{
	int error = ext4_fclose(file);
	ext4_free(file);
	return error;
}

ext4_dir *simplos_ext4_dir_open(const char *path, int *error)
{
	ext4_dir *dir = ext4_calloc(1, sizeof(*dir));
	if (!dir) {
		*error = ENOMEM;
		return NULL;
	}
	*error = ext4_dir_open(dir, path);
	if (*error != EOK) {
		ext4_free(dir);
		return NULL;
	}
	return dir;
}

void simplos_ext4_dir_close(ext4_dir *dir)
{
	ext4_dir_close(dir);
	ext4_free(dir);
}

/* Copy the next entry's name (up to `capacity` bytes), type and inode out,
 * and return the name's length; -1 after the last entry. */
int simplos_ext4_dir_next(ext4_dir *dir, char *name, size_t capacity, uint8_t *type,
			  uint32_t *inode)
{
	const ext4_direntry *entry = ext4_dir_entry_next(dir);
	if (!entry)
		return -1;
	size_t length = entry->name_length < capacity ? entry->name_length : capacity;
	memcpy(name, entry->name, length);
	*type = entry->inode_type;
	*inode = entry->inode;
	return (int)length;
}

/* The number of the inode at `path`. */
int simplos_ext4_inode_of(const char *path, uint32_t *inode)
{
	struct ext4_inode raw;
	return ext4_raw_inode_fill(path, inode, &raw);
}

/* What stat(2) tells of an inode; src/lib.rs has the same structure. */
struct simplos_ext4_stat {
	uint32_t mode, uid, gid, links;
	uint64_t size, blocks;
	uint32_t accessed, modified, changed;
};

/* What stat(2) tells of inode `inode` of the file system mounted at
 * `mount_point`. lwext4 has no call for this, but hands out the superblock,
 * which lives inside the file system's structure. */
int simplos_ext4_stat(const char *mount_point, uint32_t inode, struct simplos_ext4_stat *st)
{
	struct ext4_sblock *sb;
	struct ext4_inode_ref ref;
	int r = ext4_get_sblock(mount_point, &sb);
	if (r != EOK)
		return r;
	struct ext4_fs *fs = (struct ext4_fs *)((char *)sb - offsetof(struct ext4_fs, sb));
	r = ext4_fs_get_inode_ref(fs, inode, &ref);
	if (r != EOK)
		return r;
	st->mode = ext4_inode_get_mode(sb, ref.inode);
	st->uid = ext4_inode_get_uid(ref.inode);
	st->gid = ext4_inode_get_gid(ref.inode);
	st->links = ext4_inode_get_links_cnt(ref.inode);
	st->size = ext4_inode_get_size(sb, ref.inode);
	st->blocks = ext4_inode_get_blocks_count(sb, ref.inode);
	st->accessed = ext4_inode_get_access_time(ref.inode);
	st->modified = ext4_inode_get_modif_time(ref.inode);
	st->changed = ext4_inode_get_change_inode_time(ref.inode);
	return ext4_fs_put_inode_ref(&ref);
}

/* Whether the journal holds changes not yet written to their places. lwext4
 * ignores this flag and would mount such a file system as if it were clean. */
int simplos_ext4_needs_recovery(const char *mount_point)
{
	struct ext4_sblock *sb;
	if (ext4_get_sblock(mount_point, &sb) != EOK)
		return 1;
	return ext4_sb_feature_incom(sb, EXT4_FINCOM_RECOVER) ? 1 : 0;
}

/* Whether the file system has a read-only compatible feature lwext4 doesn't
 * know, which it can read but not write: asked to mount such a file system
 * read-write, lwext4 quietly mounts it read-only. */
int simplos_ext4_read_only(const char *mount_point)
{
	struct ext4_sblock *sb;
	if (ext4_get_sblock(mount_point, &sb) != EOK)
		return 1;
	return (ext4_get32(sb, features_read_only) & ~CONFIG_SUPPORTED_FRO_COM) != 0;
}

/* Whether the file system has a journal, which then holds every change
 * before it's made where it goes. */
int simplos_ext4_journaled(const char *mount_point)
{
	struct ext4_sblock *sb;
	if (ext4_get_sblock(mount_point, &sb) != EOK)
		return 0;
	return ext4_sb_feature_com(sb, EXT4_FCOM_HAS_JOURNAL) ? 1 : 0;
}
