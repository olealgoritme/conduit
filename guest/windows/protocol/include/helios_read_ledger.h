/* C mirror of the scanout READ LEDGER escapes (guest/windows/protocol/src/escape.rs, section
 * "D4a scanout acquire"; KMD side guest/windows/kmd_render/src/adapter/read_ledger.rs).
 *
 *   HELIOS_ESCAPE_MAP_READ_LEDGER (0x000E)  PROBE / MAP / UNMAP the read-only ledger page
 *   HELIOS_ESCAPE_SCANOUT_EVENT   (0x000F)  PROBE / REGISTER / UNREGISTER a retirement event
 *
 * The Rust file is the source of truth; every constant, size and offset below is asserted on
 * both sides (a protocol crate test parses this file). Little-endian, no pointers, identical
 * for 32-bit (WoW64) and 64-bit callers.
 *
 * WHAT THE LEDGER IS
 *
 * The KMD keeps one slot per Venus resource id for which a host read of the resource's
 * contents is (or was just) in flight: a windowed Blt copy, a DXGK flip's flush, the direct
 * RESOURCE_FLUSH. `issued` counts the host reads enqueued for the claim, `retired` counts
 * their terminal outcomes (success, failure, transport latch: every read retires exactly
 * once). A host read of the resource is in flight iff, for a stable sample of its slot,
 *
 *     retired < issued
 *
 * The page is mapped READ-ONLY into the calling process. Nothing in the KMD waits on it: it
 * is a claim a consumer can read, not a wall. A consumer that is about to overwrite a
 * resource a Present may still be reading (a swap-chain buffer handed back to the app, a
 * DXVK snapshot, an NVK image) arms its own wait from this predicate.
 *
 * THE WAIT CONDITION
 *
 *   1. Make the Present call (D3DKMTPresent / pfnPresentCb). The KMD takes the ledger claim
 *      of the Blt source inside the call, so the claim is published before it returns.
 *   2. AFTER the Present returned, sample the source's slot with helios_read_ledger_lookup():
 *      (generation, issued, retired).
 *   3. If no slot is found, see "Full ledger" below: not busy only if nothing overflowed.
 *   4. If found and retired >= issued, no host read of that claim is in flight: reuse the
 *      resource now.
 *   5. Otherwise wait until retired >= issued, where `issued` is the value sampled in step 2
 *      (do NOT re-sample issued: a later Present of the same resource raises it again and
 *      this wait would chase it). Re-read the slot to check; either poll with a bounded
 *      sleep, or register a HELIOS_ESCAPE_SCANOUT_EVENT and wait on it with a timeout.
 *      The event is level-triggered and lossy by design (see HeliosEscapeScanoutEvent):
 *      every wake, event or timeout, re-reads the ledger and acts on the counters, never on
 *      the wake itself.
 *   6. On a re-read, a slot that is gone (no slot for the resid) or that carries a different
 *      generation than the one sampled means the sampled claim was recycled. The KMD
 *      recycles a slot only at quiescence (issued == retired), so the sampled reads are
 *      retired: stop waiting. (Same rule for a transport reset: it clears every claim and
 *      never reuses a generation.)
 *
 * FULL LEDGER (RdOvf)
 *
 * There are 65 slots (HELIOS_READ_LEDGER_SLOTS: 64 WindowedBlt readers plus the one globally
 * serialized direct RESOURCE_FLUSH reader). When a read needs a claim and every slot is live
 * (a claim stays live until quiescent), the KMD runs the read UNLEDGERED: it never refuses or
 * delays the read, it increments `slot_overflow` (the `RdOvf` diagnostic counter) instead.
 * Therefore "no slot for this resid" means "not busy" ONLY when no overflow happened that
 * could have hit it. The counter is page-wide and cumulative (it returns to 0 only at a
 * transport reset), so a consumer that needs the exact answer samples
 * helios_read_ledger_overflow() before the Present and again with the lookup, and treats
 * "no slot AND the counter changed" as UNKNOWN (wait out a bounded time, or fall back to
 * its legacy synchronization); a consumer that does not track it must treat any nonzero
 * value as "ledger unreliable" and fall back. A nonzero RdOvf is a loud failure, not a
 * tuning knob: the read ran, it just cannot be waited for through this page.
 *
 * NOT THE ScanoutReleased / ReleaseBook MECHANISM
 *
 * This ledger says "the host has finished READING this resource's contents" (the KMD's own
 * host reads, tracked per resource id and per flush/copy token). It is not the host's
 * ScanoutReleased event, the KMD's ReleaseBook (the flip book that tells when a scanout
 * buffer replaced by a later flip is no longer displayed: docs/foreign-scanout.md), nor any
 * display-engine fence. A buffer that is no longer read can still be on screen; a buffer
 * that was released from scanout can still be the source of a queued copy. Do not use one to
 * answer the other's question.
 *
 * MEMORY ORDER
 *
 * Every field is written by the KMD with Release stores (a fresh claim publishes generation
 * and counters before `resid`; a free clears `resid` first) and must be read with Acquire
 * loads. The reader protocol below is a seqlock-style double check of the slot identity. The
 * helpers here use the GCC/clang __atomic builtins; on x86/x64 an acquire load is a plain
 * load that the compiler must not reorder or elide (hence "volatile or acquire"), on weaker
 * architectures it is a real barrier. A 64-bit field must be read with ONE 64-bit load: on
 * 32-bit x86 GCC/clang emit an atomic fild/movq for __atomic_load_n, MSVC x86 gets a
 * hi/lo/hi stable read (valid because the counters are monotonic within a claim). All 64-bit
 * slot fields sit at 8-byte offsets of a page-aligned mapping, so no load straddles a cache
 * line. The page is read-only: never use a read-modify-write (lock cmpxchg) on it. */
#ifndef HELIOS_READ_LEDGER_H
#define HELIOS_READ_LEDGER_H
#include <stddef.h>
#include <stdint.h>

/* Escape header (protocol/src/escape.rs, HeliosEscapeHeader). Same values as
 * helios_foreign.h, so the two may be included together. */
#define HELIOS_ESCAPE_MAGIC 0x48454C53u /* 'HELS' */
#define HELIOS_ESCAPE_VERSION 1u

#define HELIOS_ESCAPE_MAP_READ_LEDGER 0x000Eu
#define HELIOS_ESCAPE_SCANOUT_EVENT 0x000Fu

/* "HLRL" little-endian: first word of the mapped page. Anything else (or a version or slot
 * count mismatch) means the feature is OFF: page torn down or driver mismatch. */
#define HELIOS_READ_LEDGER_MAGIC 0x4C524C48u
/* Version 2 is not prefix-compatible with version 1. */
#define HELIOS_READ_LEDGER_VERSION 2u
#define HELIOS_READ_LEDGER_SLOTS 65u
#define HELIOS_READ_LEDGER_PAGE_BYTES 2112u /* of the 4096-byte page; the rest is zero */

/* op: HeliosEscapeMapReadLedger.op takes PROBE / MAP / UNMAP, HeliosEscapeScanoutEvent.op
 * takes PROBE / REGISTER / UNREGISTER. PROBE: a supporting KMD answers out_state =
 * HELIOS_SCANOUT_ACQ_PROBE_ACK; an old KMD fails the escape with STATUS_NOT_IMPLEMENTED
 * (unknown verb). That failure is the capability signal: latch the feature OFF. */
#define HELIOS_SCANOUT_ACQ_OP_PROBE 0u
#define HELIOS_SCANOUT_ACQ_OP_MAP 1u
#define HELIOS_SCANOUT_ACQ_OP_UNMAP 2u
#define HELIOS_SCANOUT_ACQ_OP_REGISTER 1u
#define HELIOS_SCANOUT_ACQ_OP_UNREGISTER 2u

/* Capability bits: HeliosEscapeMapReadLedger.out_size of a PROBE reply (op 0). A KMD from
 * before the capability word (22.22.222.0) answers PROBE with out_size == 0: read ledger
 * only, no other capability. The absence of a bit is the negative signal. */
#define HELIOS_SCANOUT_CAP_READ_LEDGER (1u << 0)
/* The KMD honours HELIOS_PRESENT_PRIVATE_FLAG_SNAPSHOT (bind/flush the UMD's snapshot
 * resource on the DMA-flip path). Never set that flag without this bit. */
#define HELIOS_SCANOUT_CAP_SNAPSHOT_BIND (1u << 1)
/* The KMD accepts registered monotonic present-stream markers. Keep the legacy CPU gate
 * unless this bit and PRESENT_STREAM registration both succeed. */
#define HELIOS_SCANOUT_CAP_ASYNC_PRESENT_STREAM (1u << 2)
/* The KMD accepts a typed UMD snapshot source for a windowed DXGK_PRESENTFLAGS.Blt (the
 * copy source, never a scanout bind target). */
#define HELIOS_SCANOUT_CAP_WINDOWED_BLT_SNAPSHOT (1u << 3)
/* SNAPSHOT_STATUS includes deferred WindowedBlt CPU mirrors and context stashes. */
#define HELIOS_SCANOUT_CAP_SNAPSHOT_STATUS (1u << 4)
/* The KMD honours the flush gate record (helios_flush_gate.h). Same value as there. */
#define HELIOS_SCANOUT_CAP_FLUSH_GATE (1u << 5)

/* out_state of both escapes. */
#define HELIOS_SCANOUT_ACQ_OK 0u
#define HELIOS_SCANOUT_ACQ_PROBE_ACK 1u
/* UNREGISTER / UNMAP: nothing matched the caller's device. */
#define HELIOS_SCANOUT_ACQ_NOT_FOUND 2u
/* REGISTER: the event table is full (KMD counter AqRgF). The caller still gates its rewrites
 * on the ledger, but with no event its signaler sees a retirement only on its 1 ms poll:
 * loud, never wedged. */
#define HELIOS_SCANOUT_ACQ_TABLE_FULL 3u

struct HeliosEscapeHeader {
   uint32_t magic;    /* HELIOS_ESCAPE_MAGIC */
   uint32_t cmd_type; /* HELIOS_ESCAPE_* */
   uint32_t version;  /* HELIOS_ESCAPE_VERSION */
   uint32_t size;     /* total escape buffer size in bytes */
};

/* HELIOS_ESCAPE_MAP_READ_LEDGER. 40 bytes. PASSIVE, non-blocking.
 * MAP maps the ledger page read-only into the calling process and returns the user VA
 * (owner-keyed KMD-side; reclaimed on device destroy and process death). UNMAP drops the
 * caller's mapping. */
struct HeliosEscapeMapReadLedger {
   struct HeliosEscapeHeader hdr;
   uint64_t out_user_va; /* out (MAP): user VA of the HeliosReadLedgerPage mapping */
   uint32_t op;          /* in: HELIOS_SCANOUT_ACQ_OP_{PROBE,MAP,UNMAP} */
   uint32_t out_size;    /* out: MAP mapping size in bytes; PROBE capability bits */
   uint32_t out_state;   /* out: HELIOS_SCANOUT_ACQ_* */
   uint32_t _pad;
};

/* HELIOS_ESCAPE_SCANOUT_EVENT. 32 bytes. PASSIVE, non-blocking.
 *
 * REGISTER: event_handle is a usermode AUTO-RESET event (CreateEvent) of the calling
 * process; the KMD references it (EVENT_MODIFY_STATE, UserMode) and signals it on EVERY
 * scanout-read retirement until unregistered. PERSISTENT: a signal does not consume the
 * registration. The consumer must be level-triggered: wake on the event OR a bounded
 * timeout, re-read the ledger, act on the counters. Lost or coalesced wakeups are then a
 * bounded hiccup, never a hang. Entries are owner-tagged and reclaimed at device destroy and
 * StopDevice. UNREGISTER: same event_handle; the KMD drops the registration and its
 * reference and never signals it again. */
struct HeliosEscapeScanoutEvent {
   struct HeliosEscapeHeader hdr;
   uint64_t event_handle; /* in: usermode event handle, zero-extended to 64 bits */
   uint32_t op;           /* in: HELIOS_SCANOUT_ACQ_OP_{PROBE,REGISTER,UNREGISTER} */
   uint32_t out_state;    /* out: HELIOS_SCANOUT_ACQ_* */
};

/* One generation-qualified host-read claim. resid == 0 and generation == 0 both mean no
 * valid claim. issued counts the reads enqueued for this claim (the gate-semaphore value
 * space), retired counts their terminal outcomes. The KMD recycles a slot only at
 * quiescence (issued == retired), also when the backing allocation stays live, and then
 * gives the replacement claim a fresh, never reused global generation. */
struct HeliosReadLedgerSlot {
   uint32_t resid;      /* Venus resource id owning the slot; 0 = free */
   uint32_t _pad0;
   uint64_t generation; /* nonzero claim identity, never reset at a transport reset */
   uint64_t issued;
   uint64_t retired;
};

/* The one nonpaged 4 KiB page HELIOS_ESCAPE_MAP_READ_LEDGER maps read-only. */
struct HeliosReadLedgerPage {
   uint32_t magic;      /* HELIOS_READ_LEDGER_MAGIC */
   uint32_t version;    /* HELIOS_READ_LEDGER_VERSION */
   uint32_t slot_count; /* HELIOS_READ_LEDGER_SLOTS */
   uint32_t reserved0;
   struct HeliosReadLedgerSlot slots[HELIOS_READ_LEDGER_SLOTS];
   uint32_t slot_overflow; /* RdOvf: claims refused because every slot was live */
   uint32_t reserved1[3];
};

#if defined(__cplusplus)
static_assert(sizeof(HeliosEscapeHeader) == 16, "escape header");
static_assert(sizeof(HeliosEscapeMapReadLedger) == 40, "MAP_READ_LEDGER size");
static_assert(offsetof(HeliosEscapeMapReadLedger, out_user_va) == 16, "MAP_READ_LEDGER va");
static_assert(offsetof(HeliosEscapeMapReadLedger, op) == 24, "MAP_READ_LEDGER op");
static_assert(offsetof(HeliosEscapeMapReadLedger, out_size) == 28, "MAP_READ_LEDGER size field");
static_assert(offsetof(HeliosEscapeMapReadLedger, out_state) == 32, "MAP_READ_LEDGER state");
static_assert(sizeof(HeliosEscapeScanoutEvent) == 32, "SCANOUT_EVENT size");
static_assert(offsetof(HeliosEscapeScanoutEvent, event_handle) == 16, "SCANOUT_EVENT handle");
static_assert(offsetof(HeliosEscapeScanoutEvent, op) == 24, "SCANOUT_EVENT op");
static_assert(offsetof(HeliosEscapeScanoutEvent, out_state) == 28, "SCANOUT_EVENT state");
static_assert(sizeof(HeliosReadLedgerSlot) == 32, "ledger slot");
static_assert(offsetof(HeliosReadLedgerSlot, generation) == 8, "slot generation");
static_assert(offsetof(HeliosReadLedgerSlot, issued) == 16, "slot issued");
static_assert(offsetof(HeliosReadLedgerSlot, retired) == 24, "slot retired");
static_assert(offsetof(HeliosReadLedgerPage, slots) == 16, "ledger slots");
static_assert(offsetof(HeliosReadLedgerPage, slot_overflow) == 2096, "ledger overflow");
static_assert(sizeof(HeliosReadLedgerPage) == HELIOS_READ_LEDGER_PAGE_BYTES, "ledger page");
#else
_Static_assert(sizeof(struct HeliosEscapeHeader) == 16, "escape header");
_Static_assert(sizeof(struct HeliosEscapeMapReadLedger) == 40, "MAP_READ_LEDGER size");
_Static_assert(offsetof(struct HeliosEscapeMapReadLedger, out_user_va) == 16, "MAP_READ_LEDGER va");
_Static_assert(offsetof(struct HeliosEscapeMapReadLedger, op) == 24, "MAP_READ_LEDGER op");
_Static_assert(offsetof(struct HeliosEscapeMapReadLedger, out_size) == 28, "MAP_READ_LEDGER size field");
_Static_assert(offsetof(struct HeliosEscapeMapReadLedger, out_state) == 32, "MAP_READ_LEDGER state");
_Static_assert(sizeof(struct HeliosEscapeScanoutEvent) == 32, "SCANOUT_EVENT size");
_Static_assert(offsetof(struct HeliosEscapeScanoutEvent, event_handle) == 16, "SCANOUT_EVENT handle");
_Static_assert(offsetof(struct HeliosEscapeScanoutEvent, op) == 24, "SCANOUT_EVENT op");
_Static_assert(offsetof(struct HeliosEscapeScanoutEvent, out_state) == 28, "SCANOUT_EVENT state");
_Static_assert(sizeof(struct HeliosReadLedgerSlot) == 32, "ledger slot");
_Static_assert(offsetof(struct HeliosReadLedgerSlot, generation) == 8, "slot generation");
_Static_assert(offsetof(struct HeliosReadLedgerSlot, issued) == 16, "slot issued");
_Static_assert(offsetof(struct HeliosReadLedgerSlot, retired) == 24, "slot retired");
_Static_assert(offsetof(struct HeliosReadLedgerPage, slots) == 16, "ledger slots");
_Static_assert(offsetof(struct HeliosReadLedgerPage, slot_overflow) == 2096, "ledger overflow");
_Static_assert(sizeof(struct HeliosReadLedgerPage) == HELIOS_READ_LEDGER_PAGE_BYTES, "ledger page");
#endif

/* ---- Reader helpers ---------------------------------------------------------------- */

#if defined(__GNUC__) || defined(__clang__)
static inline uint32_t helios_rl_load32(const uint32_t *p)
{
   return __atomic_load_n(p, __ATOMIC_ACQUIRE);
}
static inline uint64_t helios_rl_load64(const uint64_t *p)
{
   return __atomic_load_n(p, __ATOMIC_ACQUIRE);
}
/* Orders every earlier load before every later load (the seqlock read fence). */
static inline void helios_rl_fence_acquire(void)
{
   __atomic_thread_fence(__ATOMIC_ACQUIRE);
}
#elif defined(_MSC_VER) && (defined(_M_X64) || defined(_M_IX86))
#include <intrin.h>
/* x86/x64 loads are already acquire; the volatile access and the compiler barrier stop the
 * compiler from caching or reordering them. */
static inline uint32_t helios_rl_load32(const uint32_t *p)
{
   uint32_t v = *(const volatile uint32_t *)p;
   _ReadWriteBarrier();
   return v;
}
static inline uint64_t helios_rl_load64(const uint64_t *p)
{
#if defined(_M_X64)
   uint64_t v = *(const volatile uint64_t *)p;
   _ReadWriteBarrier();
   return v;
#else
   /* No atomic 64-bit load without a write: read hi, lo, hi until the high word is stable.
    * Valid because the counters are monotonic within a claim, and a generation is
    * revalidated by the caller anyway. */
   const volatile uint32_t *w = (const volatile uint32_t *)p;
   uint32_t hi, lo;
   do {
      hi = w[1];
      lo = w[0];
   } while (hi != w[1]);
   _ReadWriteBarrier();
   return ((uint64_t)hi << 32) | lo;
#endif
}
static inline void helios_rl_fence_acquire(void)
{
   _ReadWriteBarrier();
}
#else
#error "helios_read_ledger.h: no acquire-load primitive for this compiler/architecture"
#endif

/* Is the mapped page a ledger this header understands? Acquire loads of the header words.
 * False means the feature is OFF (torn down, other version, other slot count): never trust
 * a slot. The header survives a transport reset, so a valid page stays valid. */
static inline int helios_read_ledger_page_valid(const struct HeliosReadLedgerPage *page)
{
   return page != NULL && helios_rl_load32(&page->magic) == HELIOS_READ_LEDGER_MAGIC &&
          helios_rl_load32(&page->version) == HELIOS_READ_LEDGER_VERSION &&
          helios_rl_load32(&page->slot_count) == HELIOS_READ_LEDGER_SLOTS;
}

/* The page-wide overflow counter (RdOvf): claims refused because every slot was live.
 * Sample it before the Present and with the lookup; a change means a read may have run
 * unledgered in between, so "no slot" is then NOT proof of "not busy" (see FULL LEDGER).
 * It returns to 0 at a transport reset, so compare for inequality, not for "greater". */
static inline uint32_t helios_read_ledger_overflow(const struct HeliosReadLedgerPage *page)
{
   return helios_rl_load32(&page->slot_overflow);
}

#define HELIOS_READ_LEDGER_LOOKUP_INVALID (-1) /* page invalid: feature off */
#define HELIOS_READ_LEDGER_LOOKUP_NONE 0       /* no live claim for resid */
#define HELIOS_READ_LEDGER_LOOKUP_FOUND 1      /* *gen, *issued, *retired are a stable sample */

/* Lock-free reader of one resource's claim (protocol/src/escape.rs, HeliosReadLedgerSlot).
 *
 * Returns HELIOS_READ_LEDGER_LOOKUP_FOUND with a stable sample of the claim:
 *   *gen      the claim's generation (nonzero, unique per claim for the life of the
 *             driver; compare it across samples to detect a recycled claim),
 *   *issued   host reads enqueued for the claim,
 *   *retired  host reads retired.
 * `issued` is read before `retired`, so the pair satisfies: every read counted in `issued`
 * was enqueued before the sample's end, and `retired >= issued` proves each of them done
 * (`retired` may run ahead of `issued` when a later read of the same claim was enqueued and
 * retired between the two loads; that is still "done" for the reads in `issued`).
 * Busy test and wait condition: busy iff retired < issued; wait until a later sample of the
 * SAME generation shows retired >= the issued sampled here (see THE WAIT CONDITION at the
 * top of this file).
 *
 * Returns HELIOS_READ_LEDGER_LOOKUP_NONE when no slot is live for resid (or resid is 0).
 * That is "not busy" only if the overflow counter did not change (see FULL LEDGER).
 * Returns HELIOS_READ_LEDGER_LOOKUP_INVALID when the page header does not match.
 * Any out pointer may be NULL. The out values are written only on FOUND.
 *
 * Protocol per slot (the KMD writes a claim as: resid = 0, generation, retired, issued,
 * resid = R, all Release; and frees one as: resid = 0, issued, retired, generation = 0):
 *   1. acquire-load resid; no match, next slot;
 *   2. acquire-load generation (g), then acquire-load resid AGAIN: it must still match.
 *      This second look is not redundant with the final one: a reader that matched resid
 *      against an OLD claim and then loaded the generation of the NEW claim being set up
 *      would otherwise pair it with counters not yet written; having seen the new generation
 *      it must see resid = 0 or the finished new claim, never the old match;
 *   3. acquire-load issued, then retired;
 *   4. acquire fence, then revalidate: generation == g and resid still matches.
 * A mismatch anywhere is a same-resid re-claim or a free/recycle racing the sample, NOT a
 * verdict: the whole scan is restarted (the claim may also have moved to another slot).
 * Each restart is caused by a distinct KMD claim or free, so the loop makes progress
 * whenever the KMD does; it is not bounded by a count on purpose, because giving up would
 * have to pick between "busy" and "not busy" and neither is safe. A slot with generation 0
 * is not a claim and is skipped. */
static inline int helios_read_ledger_lookup(const struct HeliosReadLedgerPage *page,
                                            uint32_t resid, uint64_t *gen,
                                            uint64_t *issued, uint64_t *retired)
{
   uint32_t i;
   int restart;

   if (!helios_read_ledger_page_valid(page))
      return HELIOS_READ_LEDGER_LOOKUP_INVALID;
   if (resid == 0)
      return HELIOS_READ_LEDGER_LOOKUP_NONE;

   do {
      restart = 0;
      for (i = 0; i < HELIOS_READ_LEDGER_SLOTS; i++) {
         const struct HeliosReadLedgerSlot *s = &page->slots[i];
         uint64_t g, iss, ret;

         if (helios_rl_load32(&s->resid) != resid)
            continue;
         g = helios_rl_load64(&s->generation);
         /* Ordered after the generation load: see step 2 above. */
         helios_rl_fence_acquire();
         if (helios_rl_load32(&s->resid) != resid) {
            restart = 1;
            break;
         }
         iss = helios_rl_load64(&s->issued);
         ret = helios_rl_load64(&s->retired);
         /* The samples above must be complete before the identity is rechecked. */
         helios_rl_fence_acquire();
         if (helios_rl_load64(&s->generation) != g || helios_rl_load32(&s->resid) != resid) {
            restart = 1;
            break;
         }
         if (g == 0)
            continue;
         if (gen)
            *gen = g;
         if (issued)
            *issued = iss;
         if (retired)
            *retired = ret;
         return HELIOS_READ_LEDGER_LOOKUP_FOUND;
      }
   } while (restart);

   return HELIOS_READ_LEDGER_LOOKUP_NONE;
}

#endif
