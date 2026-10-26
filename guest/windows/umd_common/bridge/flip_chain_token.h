// SPDX-License-Identifier: Apache-2.0
// Flip-chain token: 12-bit handoff-ledger record index + 20-bit generation.
// Bindings reuse the ledger slot's reserved 64-bit payload without changing its layout.
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

inline constexpr bool valid_generation(std::uint32_t generation) noexcept {
  return generation != 0 && generation <= kMaxGeneration;
}

inline constexpr bool valid_token(std::uint32_t token) noexcept {
  return token != 0 && valid_generation(token >> kIndexBits);
}

inline constexpr std::uint32_t make_token(std::uint32_t index,
                                          std::uint32_t generation) noexcept {
  return index <= kMaxIndex && valid_generation(generation)
             ? (generation << kIndexBits) | index
             : 0;
}

inline constexpr std::uint32_t index(std::uint32_t token) noexcept {
  return token & kIndexMask;
}

inline constexpr std::uint32_t generation(std::uint32_t token) noexcept {
  return (token >> kIndexBits) & kGenerationMask;
}

inline constexpr bool is_current(std::uint32_t token,
                                 std::uint32_t current_generation,
                                 bool occupied) noexcept {
  return occupied && valid_token(token) && valid_generation(current_generation) &&
         generation(token) == current_generation;
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

}  // namespace helios_flip_chain

#endif  // HELIOS_FLIP_CHAIN_TOKEN_H
