# Lish Linux Image

The browser and native direct-boot tests use one Linux `Image`.

The current image is the Lish RV64 `virt` build published with the upstream
rv64.js demo assets:

- source: `https://github.com/ibuildthecloud/rv64.js/releases/download/demo-images-v3/modern-Image`
- SHA-256: `2d95fe4d6006d5b9975beac74e85df458bcbc76bff412baf7e718451516b7e87`
- kernel: Linux 6.12.7

`tools/fetch-kernel.sh` downloads and verifies this file. Set
`RV64_KERNEL_FILE` to use a local compatible build, or set
`RV64_KERNEL_URL` and `RV64_KERNEL_SHA256` for a different versioned build.

The image has no loadable module dependency for the Lish machine. It enables
the virtio-mmio block and network devices, the 8250 console, packet sockets,
IPv4, ext4, and `CONFIG_EXT4_USE_FOR_EXT2`. The rootfs builder creates an ext2
filesystem because `genext2fs` works without root on macOS and Linux.

The repository does not build the Linux kernel. A future kernel change must
publish a new image, update the two values in `tools/fetch-kernel.sh`, and run
the direct Alpine boot tests before changing the default.
