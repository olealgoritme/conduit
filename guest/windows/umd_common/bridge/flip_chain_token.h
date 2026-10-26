// SPDX-License-Identifier: Apache-2.0
// Flip-chain token: 12-bit handoff-ledger record index + 20-bit generation.
// Bindings reuse the ledger slot's reserved 64-bit payload without changing its layout.
//
// The token's generation is the ledger record's 32-bit generation folded into
// 1..kMaxGeneration (it wraps instead of running out), so a record that is
// claimed and freed any number of times still yields valid tokens. A token is
// current while its record is occupied and the folded generation matches; an
// old token is told apart from the record's new one for kMaxGeneration
// claim/free cycles of that record. The checks at the end of this header run in
// every build that includes it.
#ifndef HELIOS_FLIP_CHAIN_TOKEN_H
#define HELIOS_FLIP_CHAIN_TOKEN_H

#include <cstdint>

namespace helios_flip_chain {

inline constexpr std::uint32_t kIndexBits = 12;
inline constexpr std::uint32_t kGenerationBits = 20;
inline constexpr std::uint32_t kIndexMask = (1u << kIndexBits) - 1u;
inline constexpr std::uint32_t kGenerationMask = (1u << kGenerationBits) - 1u;
inline constexpr std::uint32_t kMaxIndex = kIndexMask;
inline constexpr std::uint32_t kMaxGeneration = kGenerationMask;

// A ledger record generation folded into a token generation, 1..kMaxGeneration;
// 0 for a record generation of 0 (never claimed).
inline constexpr std::uint32_t token_generation(std::uint32_t ledger_generation) noexcept {
  return ledger_generation == 0 ? 0 : (ledger_generation - 1u) % kMaxGeneration + 1u;
}

inline constexpr bool valid_generation(std::uint32_t generation) noexcept {
  return generation != 0 && generation <= kMaxGeneration;
}

inline constexpr bool valid_token(std::uint32_t token) noexcept {
  return token != 0 && valid_generation(token >> kIndexBits);
}

// The token of ledger record `index` at its (32-bit) generation `ledger_generation`.
inline constexpr std::uint32_t make_token(std::uint32_t index,
                                          std::uint32_t ledger_generation) noexcept {
  return index <= kMaxIndex && ledger_generation != 0
             ? (token_generation(ledger_generation) << kIndexBits) | index
             : 0;
}

inline constexpr std::uint32_t index(std::uint32_t token) noexcept {
  return token & kIndexMask;
}

inline constexpr std::uint32_t generation(std::uint32_t token) noexcept {
  return (token >> kIndexBits) & kGenerationMask;
}

// Is `token` the token of its record now (generation `current_ledger_generation`)?
inline constexpr bool is_current(std::uint32_t token,
                                 std::uint32_t current_ledger_generation,
                                 bool occupied) noexcept {
  return occupied && valid_token(token) && current_ledger_generation != 0 &&
         generation(token) == token_generation(current_ledger_generation);
}

inline constexpr std::uint64_t make_binding(std::uint32_t token,
                                            std::uint32_t resid) noexcept {
  return token != 0 && resid != 0 && valid_generation(generation(token))
             ? (static_cast<std::uint64_t>(token) << 32) | resid
             : 0;
}

inline constexpr std::uint32_t binding_token(std::uint64_t binding,
                                             std::uint32_t resid) noexcept {
  const std::uint32_t token = static_cast<std::uint32_t>(binding >> 32);
  return resid != 0 && static_cast<std::uint32_t>(binding) == resid &&
                 valid_token(token)
             ? token
             : 0;
}

// ---- checks (compile time)

// Layout.
static_assert(make_token(0, 1) == 0x00001000u);
static_assert(make_token(4095, 1) == 0x00001fffu);
static_assert(make_token(1, kMaxGeneration) == 0xfffff001u);
static_assert(make_token(4096, 1) == 0);
static_assert(make_token(0, 0) == 0);
static_assert(index(0xfffff001u) == 1);
static_assert(generation(0xfffff001u) == 0xfffff);
static_assert(index(make_token(4095, 1)) == 4095);

// A recycled record gets a new generation; the old token is stale.
static_assert(index(make_token(42, 7)) == index(make_token(42, 8)));
static_assert(make_token(42, 7) != make_token(42, 8));
static_assert(is_current(make_token(42, 8), 8, true));
static_assert(!is_current(make_token(42, 7), 8, true));
static_assert(!is_current(make_token(42, 8), 7, true));
static_assert(!is_current(make_token(42, 8), 8, false));
static_assert(!is_current(make_token(42, 8), 0, true));

// Generations past 2^20 wrap and stay valid; the folded value skips 0.
static_assert(make_token(0, kMaxGeneration + 1u) == make_token(0, 1));
static_assert(make_token(4095, kMaxGeneration) == 0xffffffffu);
static_assert(make_token(7, 0x80000000u) != 0);
static_assert(make_token(7, 0xffffffffu) != 0);
static_assert(is_current(make_token(7, 0xffffffffu), 0xffffffffu, true));
static_assert(!is_current(make_token(7, 0xfffffffeu), 0xffffffffu, true));
static_assert(is_current(make_token(7, kMaxGeneration + 5u), kMaxGeneration + 5u, true));
static_assert(!is_current(make_token(7, kMaxGeneration + 5u), kMaxGeneration + 6u, true));
static_assert(token_generation(kMaxGeneration) == kMaxGeneration);
static_assert(token_generation(kMaxGeneration + 1u) == 1);
static_assert(token_generation(0xffffffffu) != 0);

// Bindings: a reclaimed slot is rejected when the resource key no longer matches.
static_assert(make_binding(0x00001000u, 7) == 0x0000100000000007ull);
static_assert(binding_token(0x0000100000000007ull, 7) == 0x00001000u);
static_assert(binding_token(0x0000100000000007ull, 8) == 0);
static_assert(binding_token(0x0000100000000000ull, 7) == 0);
static_assert(binding_token(0x0000100000000007ull, 0) == 0);
static_assert(binding_token(0, 0xabcdu) == 0);
static_assert(make_binding(0xffffffffu, 1) == 0xffffffff00000001ull);

// Zero never aliases a live record, generation or binding.
static_assert(make_binding(0, 1) == 0);
static_assert(make_binding(1, 0) == 0);
static_assert(make_binding(0x1000, 0) == 0);
static_assert(!is_current(0, 0, true));
static_assert(!is_current(0, 1, true));
static_assert(binding_token(0, 0) == 0);

}  // namespace helios_flip_chain

#endif  // HELIOS_FLIP_CHAIN_TOKEN_H
