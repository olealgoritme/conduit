/* Small helpers: formatting without the CRT, safe memory copies. */
#include "nvddi.h"

ULONG cpu_index(void)
{
    return KeGetCurrentProcessorNumberEx(NULL);
}

BOOLEAN is_kernel_address(ULONG_PTR p)
{
    return (LONG_PTR)p < 0;   /* x64: upper half */
}

int str_ieq_w(const WCHAR *a, const WCHAR *b)
{
    for (;; a++, b++) {
        WCHAR x = *a, y = *b;
        if (x >= 'A' && x <= 'Z')
            x += 32;
        if (y >= 'A' && y <= 'Z')
            y += 32;
        if (x != y)
            return 0;
        if (!x)
            return 1;
    }
}

/* all pages of [p, p+len) resident and mapped */
static BOOLEAN range_valid(ULONG_PTR p, ULONG len)
{
    ULONG_PTR a = p & ~(ULONG_PTR)(PAGE_SIZE - 1);
    ULONG_PTR end = p + len;
    if (end < p)
        return FALSE;
    for (; a < end; a += PAGE_SIZE)
        if (!MmIsAddressValid((PVOID)a))
            return FALSE;
    return TRUE;
}

/*
 * Copy len bytes from src without ever faulting.
 *   <= APC_LEVEL : MmCopyMemory (handles invalid kernel and user addresses,
 *                  refuses I/O space).  direct_ok: if it refuses a kernel range
 *                  whose pages are all valid (e.g. a write-combined aperture
 *                  mapping the callee just wrote), read it directly.
 *   DISPATCH     : kernel addresses whose pages are all valid.
 *   > DISPATCH   : nothing (the caller copies small argument records itself).
 * Returns bytes copied; the rest of dst is zeroed.
 */
ULONG safe_copy(void *dst, ULONG_PTR src, ULONG len, KIRQL irql, BOOLEAN direct_ok, USHORT *flags)
{
    ULONG got = 0;
    if (!len)
        return 0;
    if (!src) {
        *flags |= NDL_BF_FAILED;
        RtlZeroMemory(dst, len);
        return 0;
    }
    if (!is_kernel_address(src))
        *flags |= NDL_BF_USER;
    if (irql <= APC_LEVEL) {
        MM_COPY_ADDRESS a;
        SIZE_T n = 0;
        NTSTATUS st;
        a.VirtualAddress = (PVOID)src;
        st = MmCopyMemory(dst, a, len, MM_COPY_MEMORY_VIRTUAL, &n);
        got = NT_SUCCESS(st) ? len : (ULONG)n;
        if (got < len && direct_ok && is_kernel_address(src) && range_valid(src + got, len - got)) {
            RtlCopyMemory((UCHAR *)dst + got, (const void *)(src + got), len - got);
            got = len;
            *flags |= NDL_BF_DIRECT;
        }
    } else if (irql == DISPATCH_LEVEL) {
        if (is_kernel_address(src) && range_valid(src, len)) {
            RtlCopyMemory(dst, (const void *)src, len);
            got = len;
        } else {
            *flags |= NDL_BF_IRQL;
        }
    } else {
        *flags |= NDL_BF_IRQL;
    }
    if (got < len) {
        RtlZeroMemory((UCHAR *)dst + got, len - got);
        *flags |= got ? NDL_BF_PARTIAL : NDL_BF_FAILED;
    }
    return got;
}

/* ------------------------------------------------------------ fmt: %s %ls %u %d %x %X %p %c %%, l/ll, width, 0 */

static void put(char *b, ULONG size, ULONG *n, char c)
{
    if (*n + 1 < size)
        b[*n] = c;
    (*n)++;
}

static void put_num(char *b, ULONG size, ULONG *n, ULONG64 v, ULONG base, int upper, int width, int zero, int neg)
{
    char tmp[24];
    int i = 0;
    const char *dig = upper ? "0123456789ABCDEF" : "0123456789abcdef";
    do {
        tmp[i++] = dig[v % base];
        v /= base;
    } while (v && i < 22);
    if (neg)
        width--;
    if (neg && zero)
        put(b, size, n, '-');
    while (width-- > i)
        put(b, size, n, zero ? '0' : ' ');
    if (neg && !zero)
        put(b, size, n, '-');
    while (i)
        put(b, size, n, tmp[--i]);
}

int fmt_v(char *b, ULONG size, const char *f, va_list ap)
{
    ULONG n = 0;
    for (; *f; f++) {
        int lng = 0, width = 0, zero = 0, wide = 0;
        if (*f != '%') {
            put(b, size, &n, *f);
            continue;
        }
        f++;
        if (*f == '0') {
            zero = 1;
            f++;
        }
        while (*f >= '0' && *f <= '9')
            width = width * 10 + (*f++ - '0');
        while (*f == 'l') {
            lng++;
            wide = 1;
            f++;
        }
        switch (*f) {
        case 's':
            if (wide) {
                const WCHAR *s = va_arg(ap, const WCHAR *);
                if (!s)
                    s = L"(null)";
                for (; *s; s++)
                    put(b, size, &n, (*s < 128) ? (char)*s : '?');
            } else {
                const char *s = va_arg(ap, const char *);
                if (!s)
                    s = "(null)";
                for (; *s; s++)
                    put(b, size, &n, *s);
            }
            break;
        case 'c':
            put(b, size, &n, (char)va_arg(ap, int));
            break;
        case 'd': {
            LONG64 v = lng >= 2 ? va_arg(ap, LONG64) : va_arg(ap, int);
            put_num(b, size, &n, v < 0 ? (ULONG64)-v : (ULONG64)v, 10, 0, width, zero, v < 0);
            break;
        }
        case 'u':
            put_num(b, size, &n, lng >= 2 ? va_arg(ap, ULONG64) : va_arg(ap, unsigned int), 10, 0, width,
                    zero, 0);
            break;
        case 'x':
        case 'X':
            put_num(b, size, &n, lng >= 2 ? va_arg(ap, ULONG64) : va_arg(ap, unsigned int), 16, *f == 'X',
                    width, zero, 0);
            break;
        case 'p':
            put(b, size, &n, '0');
            put(b, size, &n, 'x');
            put_num(b, size, &n, (ULONG64)(ULONG_PTR)va_arg(ap, void *), 16, 0, 16, 1, 0);
            break;
        case '%':
            put(b, size, &n, '%');
            break;
        default:
            if (!*f)
                f--;
            break;
        }
    }
    if (size)
        b[n < size ? n : size - 1] = 0;
    return (int)n;
}

int fmt(char *b, ULONG size, const char *f, ...)
{
    va_list ap;
    int r;
    va_start(ap, f);
    r = fmt_v(b, size, f, ap);
    va_end(ap);
    return r;
}
