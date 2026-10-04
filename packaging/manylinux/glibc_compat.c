/* glibc symbols that ONNX Runtime 1.28's prebuilt static libraries (ort-sys
 * download-binaries) reference but glibc 2.28 (manylinux_2_28) does not have. Linked
 * into the Linux full binary by packaging/manylinux/build.sh so it runs on glibc 2.28+
 * (Debian 12, Ubuntu 22.04, RHEL 8), as 0.5.2's wheels did. mokuro-bunko, MPL-2.0.
 *
 * __isoc23_strto{l,ll,ull} (glibc 2.38) are the C23 versions of strto*: identical
 * except that base 0/2 also accept a "0b" prefix, which ONNX Runtime never parses.
 * __libc_single_threaded (glibc 2.32) lets libstdc++ skip atomics while a process has
 * one thread; 0 ("maybe several threads") is always correct, only never faster. */
#include <stdlib.h>

long __isoc23_strtol(const char *s, char **end, int base) { return strtol(s, end, base); }
long long __isoc23_strtoll(const char *s, char **end, int base) { return strtoll(s, end, base); }
unsigned long long __isoc23_strtoull(const char *s, char **end, int base) {
	return strtoull(s, end, base);
}
char __libc_single_threaded = 0;
