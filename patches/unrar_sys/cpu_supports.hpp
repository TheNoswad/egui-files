// Force-included into unrar's sources on Linux in place of GCC's
// `__builtin_cpu_supports`, which links `__cpu_indicator_init` from the static
// libgcc.a. Distros that build GCC for a newer CPU level (CachyOS: x86-64-v3)
// ship that function compiled with BMI2, so it crashed at startup, before
// main, with SIGILL on older CPUs (e.g. Ivy Bridge), whether or not a RAR was
// ever opened. This asks the CPU directly instead.
#pragma once

#include <cpuid.h>
#include <string.h>

static inline int unrar_cpu_supports(const char *feature)
{
  unsigned int eax, ebx, ecx, edx;
  if (!__get_cpuid(1, &eax, &ebx, &ecx, &edx))
    return 0;
  if (strcmp(feature, "sse") == 0)
    return (edx >> 25) & 1;
  if (strcmp(feature, "sse2") == 0)
    return (edx >> 26) & 1;
  if (strcmp(feature, "ssse3") == 0)
    return (ecx >> 9) & 1;
  if (strcmp(feature, "sse4.1") == 0)
    return (ecx >> 19) & 1;
  if (strcmp(feature, "aes") == 0)
    return (ecx >> 25) & 1;
  if (strcmp(feature, "avx2") == 0)
  {
    // AVX needs the OS to save the YMM registers too (XCR0 bits 1 and 2).
    bool osxsave = (ecx >> 27) & 1, avx = (ecx >> 28) & 1;
    if (!osxsave || !avx)
      return 0;
    unsigned int xcr0, xcr0_high;
    __asm__("xgetbv" : "=a"(xcr0), "=d"(xcr0_high) : "c"(0));
    if ((xcr0 & 6) != 6)
      return 0;
    if (!__get_cpuid_count(7, 0, &eax, &ebx, &ecx, &edx))
      return 0;
    return (ebx >> 5) & 1;
  }
  return 0;
}

#define __builtin_cpu_supports(feature) unrar_cpu_supports(feature)
