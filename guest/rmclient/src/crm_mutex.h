/* SPDX-License-Identifier: MIT */
/* The client lock: C11 threads where libc has them, SRW locks on Windows
 * (MinGW-w64 has no <threads.h>). */
#ifndef CRM_MUTEX_H
#define CRM_MUTEX_H

#ifdef _WIN32

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>

typedef SRWLOCK crm_mutex;

static inline int  crm_mutex_init(crm_mutex *m)    { InitializeSRWLock(m); return 0; }
static inline void crm_mutex_destroy(crm_mutex *m) { (void)m; }
static inline void crm_mutex_lock(crm_mutex *m)    { AcquireSRWLockExclusive(m); }
static inline void crm_mutex_unlock(crm_mutex *m)  { ReleaseSRWLockExclusive(m); }

#else

#include <threads.h>

typedef mtx_t crm_mutex;

static inline int  crm_mutex_init(crm_mutex *m)    { return mtx_init(m, mtx_plain) == thrd_success ? 0 : -1; }
static inline void crm_mutex_destroy(crm_mutex *m) { mtx_destroy(m); }
static inline void crm_mutex_lock(crm_mutex *m)    { mtx_lock(m); }
static inline void crm_mutex_unlock(crm_mutex *m)  { mtx_unlock(m); }

#endif

#endif /* CRM_MUTEX_H */
