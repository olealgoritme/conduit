/* The pieces of the MSVC static runtime (libcmt/libvcruntime/libcpmt) that
 * nvsdk_ngx_s.lib needs and MinGW does not have. Everything else the NGX
 * objects call is the UCRT (ucrtbase.dll), see build.sh.
 *
 * Built with clang for x86_64-w64-mingw32: _Init_thread_epoch must be a
 * native TLS variable (the MSVC code reads it through _tls_index and a
 * SECREL relocation), which GCC's emulated TLS is not.
 */
#include <windows.h>
#include <limits.h>
#include <stdint.h>
#include <stddef.h>

/* The NGX objects' malloc/free (renamed by build.sh, imported from ucrtbase). */
void *__ngxucrt_malloc(size_t n);
void __ngxucrt_free(void *p);
EXCEPTION_DISPOSITION __ngxucrt___CxxFrameHandler3(PEXCEPTION_RECORD rec, void *frame,
                                                   PCONTEXT ctx, void *dispatch);

static void shim_fatal(const char *msg)
{
    DWORD n;
    HANDLE h = GetStdHandle(STD_ERROR_HANDLE);
    WriteFile(h, msg, lstrlenA(msg), &n, NULL);
    TerminateProcess(GetCurrentProcess(), 3);
}

/* Referenced by any object that uses floating point. */
int _fltused = 0x9875;

/* /GS stack cookies. The check must leave rax and xmm0 (the caller's return
 * value) alone, hence assembly. A mismatch is a fast fail, like MSVC's. */
uint64_t __security_cookie = 0x00002B992DDFA232ULL;
__asm__(".text\n"
        ".globl __security_check_cookie\n"
        "__security_check_cookie:\n"
        "    cmpq __security_cookie(%rip), %rcx\n"
        "    jne 1f\n"
        "    ret\n"
        "1:  movl $2, %ecx\n" /* FAST_FAIL_STACK_COOKIE_CHECK_FAILURE */
        "    int $0x29\n");

/* Language-specific handlers named in the objects' unwind data. */
EXCEPTION_DISPOSITION __GSHandlerCheck(PEXCEPTION_RECORD rec, void *frame, PCONTEXT ctx,
                                       void *dispatch)
{
    (void)rec; (void)frame; (void)ctx; (void)dispatch;
    return ExceptionContinueSearch;
}

EXCEPTION_DISPOSITION __GSHandlerCheck_EH(PEXCEPTION_RECORD rec, void *frame, PCONTEXT ctx,
                                          void *dispatch)
{
    /* MSVC checks the cookie first, then hands over to the C++ handler, whose
     * FuncInfo is the first DWORD of the handler data either way. */
    return __ngxucrt___CxxFrameHandler3(rec, frame, ctx, dispatch);
}

/* Control Flow Guard check: no CFG here, a plain return that touches no
 * register (the caller's arguments are already loaded). */
__asm__(".text\n"
        ".globl ngx_shim_guard_check_nop\n"
        "ngx_shim_guard_check_nop:\n"
        "    ret\n");
void ngx_shim_guard_check_nop(void);
void (*__guard_check_icall_fptr)(void) = ngx_shim_guard_check_nop;

/* Thread-safe function-local statics (MSVC's "magic statics"). */
_Thread_local int _Init_thread_epoch = INT_MIN;
static int init_global_epoch = INT_MIN;
static SRWLOCK init_lock = SRWLOCK_INIT;
static CONDITION_VARIABLE init_cv = CONDITION_VARIABLE_INIT;

void _Init_thread_header(int *once)
{
    AcquireSRWLockExclusive(&init_lock);
    if (*once == 0) {
        *once = -1; /* the caller runs the initializer, then _Init_thread_footer */
    } else {
        while (*once == -1)
            SleepConditionVariableSRW(&init_cv, &init_lock, INFINITE, 0);
        if (*once == 0)
            *once = -1;
        else
            _Init_thread_epoch = init_global_epoch;
    }
    ReleaseSRWLockExclusive(&init_lock);
}

void _Init_thread_footer(int *once)
{
    AcquireSRWLockExclusive(&init_lock);
    ++init_global_epoch;
    *once = init_global_epoch;
    _Init_thread_epoch = init_global_epoch;
    ReleaseSRWLockExclusive(&init_lock);
    WakeAllConditionVariable(&init_cv);
}

void _Init_thread_abort(int *once)
{
    AcquireSRWLockExclusive(&init_lock);
    *once = 0;
    ReleaseSRWLockExclusive(&init_lock);
    WakeAllConditionVariable(&init_cv);
}

/* operator new(size_t) / operator delete(void *) with MSVC mangling, on the
 * same heap as the NGX objects' malloc/free. */
void *ngx_shim_new(size_t n) __asm__("??2@YAPEAX_K@Z");
void *ngx_shim_new(size_t n)
{
    void *p = __ngxucrt_malloc(n ? n : 1);
    if (!p)
        shim_fatal("ngx-smoketest: out of memory in NGX (operator new)\n");
    return p;
}

void ngx_shim_delete(void *p) __asm__("??3@YAXPEAX@Z");
void ngx_shim_delete(void *p)
{
    __ngxucrt_free(p);
}

void ngx_shim_delete_sized(void *p, size_t n) __asm__("??3@YAXPEAX_K@Z");
void ngx_shim_delete_sized(void *p, size_t n)
{
    (void)n;
    __ngxucrt_free(p);
}

/* std::_Xbad_alloc() / std::_Xlength_error(const char *): no C++ exceptions
 * cross into our code, so these end the process with a message. */
void ngx_shim_xbad_alloc(void) __asm__("?_Xbad_alloc@std@@YAXXZ");
void ngx_shim_xbad_alloc(void)
{
    shim_fatal("ngx-smoketest: std::bad_alloc in NGX\n");
}

void ngx_shim_xlength_error(const char *what) __asm__("?_Xlength_error@std@@YAXPEBD@Z");
void ngx_shim_xlength_error(const char *what)
{
    (void)what;
    shim_fatal("ngx-smoketest: std::length_error in NGX\n");
}
