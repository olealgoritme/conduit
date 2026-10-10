#!/bin/sh
# Conduit: mount the host's shared folders. Every virtiofs device whose tag is
# "conduit-NAME" is mounted at /mnt/conduit/NAME. Safe to run again; it only
# mounts what is not mounted yet.
BASE=/mnt/conduit
for d in /sys/fs/virtiofs/*; do
    [ -r "$d/tag" ] || continue
    tag=$(cat "$d/tag")
    case "$tag" in
        conduit-?*) ;;
        *) continue ;;
    esac
    name=${tag#conduit-}
    mp=$BASE/$name
    mountpoint -q "$mp" 2>/dev/null && continue
    mkdir -p "$mp"
    mount -t virtiofs "$tag" "$mp" || echo "conduit: could not mount $tag at $mp" >&2
done
exit 0
