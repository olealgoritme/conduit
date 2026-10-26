// SPDX-License-Identifier: Apache-2.0
#include "../umd_common/bridge/flip_chain_token.h"

#include <cassert>
#include <cstdint>

using namespace helios_flip_chain;

static_assert(make_token(0, 1) == 0x00001000u);
static_assert(make_token(4095, 1) == 0x00001fffu);
static_assert(make_token(1, 0xfffff) == 0xfffff001u);
static_assert(make_token(4096, 1) == 0);
static_assert(make_token(0, 0) == 0);
static_assert(make_token(0, 0x100000) == 0);
static_assert(index(0xfffff001u) == 1);
static_assert(generation(0xfffff001u) == 0xfffff);
static_assert(make_binding(0x00001000u, 7) == 0x0000100000000007ull);
static_assert(binding_token(0x0000100000000007ull, 7) == 0x00001000u);
static_assert(binding_token(0x0000100000000007ull, 8) == 0);
static_assert(binding_token(0x0000100000000000ull, 7) == 0);
static_assert(binding_token(0x0000100000000007ull, 0) == 0);
static_assert(make_binding(0, 1) == 0);
static_assert(make_binding(1, 0) == 0);
static_assert(make_binding(0x1000, 0) == 0);

int main() {
  // A recycled ledger record gets a new generation; the old token is stale.
  const std::uint32_t old_token = make_token(42, 7);
  const std::uint32_t new_token = make_token(42, 8);
  assert(index(old_token) == index(new_token));
  assert(old_token != new_token);
  assert(is_current(new_token, 8, true));
  assert(!is_current(old_token, 8, true));
  assert(!is_current(new_token, 7, true));
  assert(!is_current(new_token, 8, false));
  assert(!is_current(new_token, 0, true));

  // A reclaimed slot binding is rejected when the resource key no longer matches.
  const std::uint64_t binding = make_binding(new_token, 0xabcdu);
  assert(binding != 0);
  assert(binding_token(binding, 0xabcdu) == new_token);
  assert(binding_token(binding, 0xabceu) == 0);
  assert(binding_token(binding, 0) == 0);
  assert(binding_token(0, 0xabcdu) == 0);

  // Maximal generation is valid; one past it fails closed instead of wrapping.
  const std::uint32_t max_token = make_token(4095, kMaxGeneration);
  assert(max_token == 0xffffffffu);
  assert(is_current(max_token, kMaxGeneration, true));
  assert(make_token(0, kMaxGeneration + 1u) == 0);
  assert(make_token(4095, kMaxGeneration + 1u) == 0);
  assert(!is_current(max_token, kMaxGeneration + 1u, true));
  assert(make_binding(max_token, 1) == 0xffffffff00000001ull);

  // Record index 4095 is the last valid entry; 4096 is out of range.
  assert(make_token(4095, 1) != 0);
  assert(index(make_token(4095, 1)) == 4095);
  assert(make_token(4096, 1) == 0);

  // Zero ids never alias a live record, generation, or binding.
  assert(!is_current(0, 0, true));
  assert(!is_current(0, 1, true));
  assert(make_binding(0, 0) == 0);
  assert(binding_token(0, 0) == 0);
  return 0;
}
