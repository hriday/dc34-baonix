#!/bin/sh
# PID 1 for the guest initramfs. Kept deliberately short: everything here
# runs before there is any way to debug it.
#
# `CONFIG_DEVTMPFS_MOUNT=y` is set in nix/guest/kernel.config, but
# drivers/base/Kconfig says outright: "This option does not affect initramfs
# based booting, here the devtmpfs filesystem always needs to be mounted
# manually after the rootfs is mounted." So nothing has mounted /dev when
# this script starts, and every device node except one is missing.
#
# The exception is the one that matters, and it is worth being precise about
# because the obvious story is wrong. It is tempting to say that /dev/console
# does not exist yet, that console_on_rootfs() therefore failed, that it
# printed "Warning: unable to open an initial console", and that init runs
# with no fd 0/1/2 at all — so the mount and the exec below are what rescue
# the output. That was this file's original justification and it is false:
# the kernel unpacks its own built-in usr/default_cpio_list into rootfs
# *before* our initrd, and that list contains
#
#     nod /dev/console 0600 0 0 c 5 1
#
# so /dev/console is already there and init already has stdio. The warning
# appears zero times in a real boot log; it was looked for.
#
# The two lines stay anyway, for reasons that survive that correction:
#
#   - the mount is what populates /dev with everything *other* than console,
#     which is the actual point of devtmpfs here;
#   - it keeps working for a kernel built with a custom
#     CONFIG_INITRAMFS_SOURCE, which would not carry the built-in node;
#   - and the redirect is correct either way — if the mount fails, the
#     built-in node is still visible underneath it, so the exec still
#     succeeds.
#
# Two lines to stop depending on an implementation detail of the kernel's
# default cpio. Baking our own /dev/console into the archive instead is not
# an option: mknod needs privileges the nix build sandbox does not have.
#
# PATH is set explicitly because the kernel hands init an environment of only
# HOME and TERM (`envp_init` in init/main.c). busybox ash would supply a
# default, but the very first command below runs while a "command not found"
# is still invisible, so this does not rely on that.
export PATH=/bin

mount -t devtmpfs none /dev
exec >/dev/console 2>&1 </dev/console

mount -t proc none /proc
mount -t sysfs none /sys

echo
# The badge's display is 16 columns wide, so there is no room for one
# combined banner line and this is spread across several short ones. The
# kernel release still comes from uname, and the machine name is still the
# `model` property of the device tree the kernel actually booted with
# (NUL-terminated, hence the tr) — truncated to 16 characters rather than
# wrapped, since it does not need to survive intact to be evidence the guest
# read it back at runtime.
#
# Sixteen, not eighteen. An earlier revision of this file cut to 18, from a
# reading of the font's glyph width that left out its 1px kern; the real
# advance is 8px and 128/8 is 16. `badge/app/src/oled.rs` carries the
# derivation and the measurement. Every `cut` width below is that 16, and a
# 32-character store hash consequently lands on exactly two full rows.
echo "riscv64 Linux"
echo "$(uname -r)"
echo "$(tr -d '\0' < /proc/device-tree/model | cut -c1-16)"
echo
echo "/nix/store:"
for p in /nix/store/*; do
    n=$(basename "$p")
    echo "${n%%-*}"
    echo "  ${n#*-}" | cut -c1-16
done

exec /bin/sh
