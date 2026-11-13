/*
 * ORIGIN is the load address every exynos_4412_rs application uses (bootcmd /
 * uart_boot: the image moves itself down here from where it was started).
 *
 * Layout: image + .bss (two 4 MiB framebuffers) + the stacks of the four cores,
 * then the TLSF heap. The heap may overlap uart_boot's staging area at
 * 0x50000000: the image has relocated itself before heap::init runs.
 * The board has 1 GiB of DRAM (0x40000000..0x80000000; the HAL's DRAM_SIZE
 * used to say 2 GiB). 40 MiB for the image + .bss + stacks (~31 MiB used),
 * then a 960 MiB heap: RAM ends at 0x7E808000, leaving ~24 MiB at the top
 * unused (1 GiB Exynos 4412 boards' secure world usually reserves the last
 * MiB). The heap holds the resource shadow (64 MB), the object pool (16 MB
 * default, platform.rs), cached font bitmaps, decoded JPEGs, sprites, layer
 * surfaces, display lists. The GPU maps all of it (HAL gpu MAP_TABLES).
 */
MEMORY
{
    RAM : ORIGIN = 0x40008000, LENGTH = 1000M
}

__heap_size = 960M;

/* embedded-sdmmc reads and writes 512-byte blocks on the stack (storage.rs). */
__svc_stack_size = 64K;
