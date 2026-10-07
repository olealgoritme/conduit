/* SPDX-License-Identifier: GPL-2.0 */
/*
 * The host driver release the backend serves in device config: a string,
 * "615.71.09", or "565.77" for a release NVIDIA numbers with two parts, which
 * is patch 0.
 *
 * This is the guest half of one rule; the host half is
 * abi::version::DriverVersion::parse (host/backend/gen/src/version.rs). Both
 * accept only two or three dotted numbers of digits, and both are tested on
 * the same lines (host/backend/gen/fixtures/proc_version.tsv, whose last
 * column is the string the guest receives): test/version_test.c.
 *
 * Plain C, no libc: it builds in the module and in that test.
 */
#ifndef NVGPU_VERSION_H
#define NVGPU_VERSION_H

#ifdef __KERNEL__
#include <linux/types.h>
#else
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
typedef uint32_t u32;
#endif

/* One run of digits at *s, as a number; false if there is none or it
 * overflows 32 bits. Advances *s past it. */
static inline bool nvgpu_version_number(const char **s, u32 *out)
{
	const char *p = *s;
	u32 v = 0;

	if (*p < '0' || *p > '9')
		return false;
	for (; *p >= '0' && *p <= '9'; p++) {
		u32 d = (u32)(*p - '0');

		if (v > (0xffffffffu - d) / 10)
			return false;
		v = v * 10 + d;
	}
	*s = p;
	*out = v;
	return true;
}

/*
 * Parse "major.minor" or "major.minor.patch". Anything else, including
 * trailing characters other than whitespace, is false and leaves the outputs
 * untouched.
 */
static inline bool nvgpu_parse_version(const char *s, u32 *major, u32 *minor,
				       u32 *patch)
{
	u32 a, b, c = 0;

	if (!s)
		return false;
	while (*s == ' ' || *s == '\t' || *s == '\n')
		s++;
	if (!nvgpu_version_number(&s, &a) || *s++ != '.' ||
	    !nvgpu_version_number(&s, &b))
		return false;
	if (*s == '.') {
		s++;
		if (!nvgpu_version_number(&s, &c))
			return false;
	}
	while (*s == ' ' || *s == '\t' || *s == '\n')
		s++;
	if (*s)
		return false;
	*major = a;
	*minor = b;
	*patch = c;
	return true;
}

#endif /* NVGPU_VERSION_H */
