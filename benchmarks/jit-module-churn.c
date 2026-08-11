// Stress the emulator's JIT module lifecycle with executable-page churn.
// This is a freestanding Linux RV64 program. It does not require libc.
#include <stddef.h>
#include <stdint.h>

enum {
    PAGE_SIZE = 4096,
    SYS_WRITE = 64,
    SYS_EXIT = 93,
    SYS_CLOCK_GETTIME = 113,
    SYS_MUNMAP = 215,
    SYS_MMAP = 222,
    SYS_RISCV_FLUSH_ICACHE = 259,
    PROT_RWX = 7,
    MAP_PRIVATE_ANONYMOUS = 0x22,
};

struct timespec {
    int64_t seconds;
    int64_t nanoseconds;
};

static long syscall6(long number, long arg0, long arg1, long arg2, long arg3, long arg4,
                     long arg5) {
    register long a0 __asm__("a0") = arg0;
    register long a1 __asm__("a1") = arg1;
    register long a2 __asm__("a2") = arg2;
    register long a3 __asm__("a3") = arg3;
    register long a4 __asm__("a4") = arg4;
    register long a5 __asm__("a5") = arg5;
    register long a7 __asm__("a7") = number;
    __asm__ volatile("ecall"
                     : "+r"(a0)
                     : "r"(a1), "r"(a2), "r"(a3), "r"(a4), "r"(a5), "r"(a7)
                     : "memory");
    return a0;
}

static void write_all(const char *data, size_t length) {
    while (length != 0) {
        long written = syscall6(SYS_WRITE, 1, (long)data, (long)length, 0, 0, 0);
        if (written <= 0) {
            return;
        }
        data += written;
        length -= (size_t)written;
    }
}

static size_t string_length(const char *text) {
    size_t length = 0;
    while (text[length] != '\0') {
        length++;
    }
    return length;
}

static void write_string(const char *text) {
    write_all(text, string_length(text));
}

static void write_u64(uint64_t value) {
    char digits[20];
    size_t length = 0;
    do {
        digits[length++] = (char)('0' + value % 10);
        value /= 10;
    } while (value != 0);
    for (size_t left = 0, right = length - 1; left < right; left++, right--) {
        char temporary = digits[left];
        digits[left] = digits[right];
        digits[right] = temporary;
    }
    write_all(digits, length);
}

static uint64_t monotonic_ns(void) {
    struct timespec time = {0, 0};
    if (syscall6(SYS_CLOCK_GETTIME, 1, (long)&time, 0, 0, 0, 0) < 0) {
        return 0;
    }
    return (uint64_t)time.seconds * 1000000000ULL + (uint64_t)time.nanoseconds;
}

static uint64_t parse_u64(const char *text, uint64_t fallback) {
    uint64_t value = 0;
    if (text == NULL || *text == '\0') {
        return fallback;
    }
    while (*text != '\0') {
        if (*text < '0' || *text > '9') {
            return fallback;
        }
        value = value * 10 + (uint64_t)(*text++ - '0');
    }
    return value == 0 ? fallback : value;
}

static uint32_t addi_a0(int immediate) {
    return ((uint32_t)immediate & 0xfffU) << 20 | 10U << 15 | 10U << 7 | 0x13U;
}

typedef uint64_t (*generated_function)(uint64_t);

static int benchmark(uintptr_t *initial_stack) {
    int argc = (int)initial_stack[0];
    char **argv = (char **)&initial_stack[1];
    uint64_t pages = argc > 1 ? parse_u64(argv[1], 1024) : 1024;
    uint64_t rounds = argc > 2 ? parse_u64(argv[2], 8) : 8;
    uint64_t calls = argc > 3 ? parse_u64(argv[3], 2304) : 2304;
    if (pages > 16384 || rounds > 1000 || calls > 1000000) {
        write_string("JIT_BENCH error=argument_out_of_range\n");
        return 2;
    }

    uint64_t length = pages * PAGE_SIZE;
    long mapped = syscall6(SYS_MMAP, 0, (long)length, PROT_RWX, MAP_PRIVATE_ANONYMOUS, -1, 0);
    if (mapped < 0) {
        write_string("JIT_BENCH error=mmap_failed\n");
        return 3;
    }
    uint8_t *code = (uint8_t *)(uintptr_t)mapped;
    uint64_t checksum = 0;

    write_string("JIT_BENCH start pages=");
    write_u64(pages);
    write_string(" rounds=");
    write_u64(rounds);
    write_string(" calls=");
    write_u64(calls);
    write_string("\n");

    uint64_t total_start = monotonic_ns();
    for (uint64_t round = 0; round < rounds; round++) {
        for (uint64_t page = 0; page < pages; page++) {
            int immediate = (int)((page * 17 + round * 131) % 2047) + 1;
            uint32_t *body = (uint32_t *)(code + page * PAGE_SIZE);
            body[0] = addi_a0(immediate);
            body[1] = 0x00008067U; // jalr x0, 0(ra)
        }
        __asm__ volatile("fence.i" ::: "memory");
        syscall6(SYS_RISCV_FLUSH_ICACHE, (long)code, (long)(code + length), 0, 0, 0, 0);

        uint64_t start = monotonic_ns();
        uint64_t round_checksum = 0;
        uint64_t expected_checksum = 0;
        for (uint64_t page = 0; page < pages; page++) {
            generated_function function = (generated_function)(code + page * PAGE_SIZE);
            uint64_t value = page ^ round;
            int immediate = (int)((page * 17 + round * 131) % 2047) + 1;
            for (uint64_t call = 0; call < calls; call++) {
                value = function(value);
            }
            round_checksum ^= value + page;
            expected_checksum ^= (page ^ round) + calls * (uint64_t)immediate + page;
        }
        if (round_checksum != expected_checksum) {
            write_string("JIT_BENCH error=checksum_mismatch round=");
            write_u64(round);
            write_string(" actual=");
            write_u64(round_checksum);
            write_string(" expected=");
            write_u64(expected_checksum);
            write_string("\n");
            syscall6(SYS_MUNMAP, (long)code, (long)length, 0, 0, 0, 0);
            return 4;
        }
        uint64_t elapsed = monotonic_ns() - start;
        checksum ^= round_checksum;
        write_string("JIT_BENCH round=");
        write_u64(round);
        write_string(" elapsed_ms=");
        write_u64(elapsed / 1000000);
        write_string(" checksum=");
        write_u64(round_checksum);
        write_string("\n");
    }

    write_string("JIT_BENCH done elapsed_ms=");
    write_u64((monotonic_ns() - total_start) / 1000000);
    write_string(" checksum=");
    write_u64(checksum);
    write_string("\n");
    syscall6(SYS_MUNMAP, (long)code, (long)length, 0, 0, 0, 0);
    return 0;
}

__attribute__((noreturn)) void start_c(uintptr_t *initial_stack) {
    int status = benchmark(initial_stack);
    syscall6(SYS_EXIT, status, 0, 0, 0, 0, 0);
    for (;;) {
        __asm__ volatile("wfi");
    }
}

__asm__(".global _start\n"
        ".type _start,@function\n"
        "_start:\n"
        "mv a0, sp\n"
        "andi sp, sp, -16\n"
        "tail start_c\n");
